//! Music/utility bot con accesso completo al Client (chat + audio).
//!
//! A differenza del chat-bot (tslib-bot, solo risposte testuali),
//! qui usiamo tslib-core + tslib-audio direttamente, così il bot può:
//!   - elencare utenti/canali, spostarsi di canale
//!   - inviare messaggi in canale o privati
//!   - trasmettere audio da YouTube/URL via yt-dlp+ffmpeg, oppure sinusoide demo
//!
//! Uso:
//!   cargo run --bin music-bot -- --server god.serod.tech:9988 --nickname MusicBot
//! Requisiti host per !play <url|ricerca>: binari `yt-dlp` e `ffmpeg` installati.
//!
//! Comandi in chat:
//!   !help, !users, !channels, !join <id|nome>, !say <msg>,
//!   !play <url|ricerca|hz>, !stop, !skip, !queue, !now, !volume <0-200>,
//!   !pause, !resume, !eq <preset|show|list>, !bass/!mid/!treble <-12..+12>,
//!   !loop [off|one|all], !lofi, !ytplaylist <ricerca>, !playlist <nome>, !playlists,
//!   !playlistsave <nome> [url...], !playlistdel <nome>, !recent, !video [on|off]
//!
//! Video (TeamSpeak 6): con `!video on` il bot condivide il video del brano
//! come screen share P2P, mentre l'audio resta sul canale vocale. Ogni viewer
//! riceve una copia del video direttamente dal bot, quindi serve banda in
//! upload e il bot deve essere raggiungibile via UDP (in Docker: rete host).
//!
//! Benvenuto: se il bot è da solo nel canale e qualcuno entra, dopo
//! `--welcome-delay-ms` manda in canale un riepilogo (brano corrente,
//! playlist salvate, ultimi brani ascoltati).

use std::collections::VecDeque;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tracing::{info, warn, Level};
use tracing_subscriber::FmtSubscriber;

use tslib_audio::codec::{Encoder, OpusEncoder};
use tslib_audio::config::{AudioConfig, OpusApplication};
use tslib_core::events::{AudioCodec, Event, MessageTarget};
use tslib_core::{Client, ClientConfig, Identity, StreamSetup};
use tslib_stream::{BroadcastConfig, BroadcastEvent, Broadcaster, EncoderConfig, VideoInput, VideoProgress};

#[derive(Parser, Debug)]
#[command(author, version, about = "Music bot TeamSpeak con tslib")]
struct Args {
    /// Indirizzo server (host o host:porta)
    #[arg(short, long)]
    server: String,

    /// Nickname
    #[arg(short, long, default_value = "MusicBot")]
    nickname: String,

    /// Password server (opzionale)
    #[arg(short, long)]
    password: Option<String>,

    /// Canale iniziale (opzionale)
    #[arg(short, long)]
    channel: Option<String>,

    /// File identità
    #[arg(short, long, default_value = "identity-music.json")]
    identity: String,

    /// Prefisso comandi
    #[arg(long, default_value = "!")]
    prefix: String,

    /// File stato persistente (volume + ultimo canale)
    #[arg(long, default_value = "music-state.json")]
    state: String,

    /// File playlist salvate (nome -> lista URL)
    #[arg(long, default_value = "playlists.json")]
    playlists: String,

    /// Ritardo del messaggio di benvenuto quando qualcuno entra nel canale
    /// in cui il bot era da solo (ms). 0 = disattivato.
    #[arg(long, default_value_t = 1500)]
    welcome_delay_ms: u64,

    /// Altezza del video condiviso con !video (la larghezza segue il formato)
    #[arg(long, default_value_t = 480)]
    video_height: u32,

    /// Bitrate del video in kbit/s, per ogni viewer (in P2P ognuno ne riceve una copia)
    #[arg(long, default_value_t = 800)]
    video_bitrate: u32,

    /// Indirizzo locale per WebRTC (es. 192.168.1.10:0). Default: tutte le
    /// interfacce, bridge Docker compresi.
    #[arg(long)]
    video_bind: Option<String>,

    /// Apre la condivisione video all'avvio, senza aspettare un !video on
    /// (in Docker: TS_VIDEO_AUTO=1). Un !video off la spegne comunque, fino
    /// al riavvio successivo.
    #[arg(long)]
    video_auto: bool,
}

/// Stato persistente tra riavvii: volume, EQ, loop e ultimo canale joinato.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BotState {
    /// Volume 0.0-2.0 (default 1.0).
    #[serde(default = "default_volume")]
    volume: f32,
    /// Ultimo canale (ID + nome per fallback se l'ID cambia).
    #[serde(default)]
    channel_id: Option<u64>,
    #[serde(default)]
    channel_name: Option<String>,
    /// EQ a 3 bande in dB (-12..+12). Default 0 = flat.
    #[serde(default)]
    eq_bass: f32,
    #[serde(default)]
    eq_mid: f32,
    #[serde(default)]
    eq_treble: f32,
    /// Nome preset EQ attivo ("flat", "rock", "custom", ...).
    #[serde(default = "default_eq_preset")]
    eq_preset: String,
    /// Modalità loop: "off" | "one" | "all".
    #[serde(default = "default_loop_mode")]
    loop_mode: String,
    /// Ultimi brani riprodotti (più recente per primo), mostrati nel benvenuto e in !recent.
    #[serde(default)]
    recent: Vec<Track>,
    /// Condivisione video attiva (!video on).
    #[serde(default)]
    video: bool,
}

impl Default for BotState {
    fn default() -> Self {
        Self {
            volume: default_volume(),
            channel_id: None,
            channel_name: None,
            eq_bass: 0.0,
            eq_mid: 0.0,
            eq_treble: 0.0,
            eq_preset: default_eq_preset(),
            loop_mode: default_loop_mode(),
            recent: Vec::new(),
            video: false,
        }
    }
}

fn default_eq_preset() -> String {
    "flat".to_string()
}

fn default_loop_mode() -> String {
    "off".to_string()
}

fn default_volume() -> f32 {
    1.0
}

impl BotState {
    fn load(path: &str) -> Self {
        match std::fs::read_to_string(path) {
            Ok(content) => match serde_json::from_str(&content) {
                Ok(state) => state,
                Err(e) => {
                    warn!("stato {path} non valido ({e}), uso default");
                    Self::default()
                }
            },
            Err(_) => Self::default(), // primo avvio: nessun file
        }
    }

    fn save(&self, path: &str) {
        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, json) {
                    warn!("salvataggio stato {path} fallito: {e}");
                }
            }
            Err(e) => warn!("serializzazione stato fallita: {e}"),
        }
    }

    fn sanitized_volume(v: f32) -> f32 {
        if !v.is_finite() {
            return 1.0;
        }
        v.clamp(0.0, 2.0)
    }

    fn sanitized_eq_db(v: f32) -> f32 {
        if !v.is_finite() {
            return 0.0;
        }
        v.clamp(-12.0, 12.0)
    }
}

/// Un brano in coda o in riproduzione: URL + titolo + copertina + durata
/// (se risolti via yt-dlp).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Track {
    url: String,
    /// Titolo YouTube (es. "Big Buck Bunny ..."). Se `None`, si mostra l'URL.
    #[serde(default)]
    title: Option<String>,
    /// URL della copertina (thumbnail YouTube). Se `Some`, viene inviata come
    /// `[img]...[/img]` così il client TeamSpeak mostra la preview inline.
    #[serde(default)]
    thumbnail: Option<String>,
    /// Durata in secondi (da yt-dlp). Se `Some`, alimenta la barra di
    /// avanzamento in basso nel video e il conteggio in `!now`.
    #[serde(default)]
    duration_secs: Option<u64>,
}

impl Track {
    fn new(
        url: String,
        title: Option<String>,
        thumbnail: Option<String>,
        duration_secs: Option<u64>,
    ) -> Self {
        let title = title
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        let thumbnail = thumbnail
            .map(|t| t.trim().to_string())
            .filter(|t| t.starts_with("http://") || t.starts_with("https://"));
        let duration_secs = duration_secs.filter(|d| *d > 0);
        Self {
            url,
            title,
            thumbnail,
            duration_secs,
        }
    }

    /// Testo da mostrare in chat: preferisce il titolo all'URL.
    fn display(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.url)
    }

    /// Annuncio ricco per la chat TeamSpeak: titolo in grassetto + copertina
    /// inline (preview, non file allegato) + link originale.
    /// `prefix` è la riga iniziale (es. "Riproduco", "Prossimo", "In riproduzione").
    fn announce(&self, prefix: &str) -> String {
        let name = self.display();
        match &self.thumbnail {
            Some(thumb) => format!("[b]{prefix}: {name}[/b]\n[img]{thumb}[/img]\n{url}", url = self.url),
            None => format!("[b]{prefix}: {name}[/b]\n{url}", url = self.url),
        }
    }
}

/// Modalità di ripetizione della riproduzione.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopMode {
    Off,
    One,
    All,
}

impl LoopMode {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "off" | "no" | "0" => Some(Self::Off),
            "one" | "uno" | "1" | "track" | "brano" => Some(Self::One),
            "all" | "tutti" | "coda" | "queue" => Some(Self::All),
            _ => None,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::One => "one",
            Self::All => "all",
        }
    }
}

// ---------------------------------------------------------------------------
// Equalizer a 3 bande (biquad RBJ in serie, mono 48kHz).
//
// Catena: low-shelf 200Hz -> peaking 1kHz (Q=1) -> high-shelf 6kHz.
// I guadagni sono in dB (-12..+12). Con 0dB la banda è in bypass.
// Viene applicato sul PCM prima dell'encode Opus, quindi il cambio preset
// è istantaneo e non richiede il riavvio dello stream.
// ---------------------------------------------------------------------------

/// Un filtro biquad del secondo ordine (forma diretta I).
#[derive(Debug, Clone)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    fn identity() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }

    fn reset(&mut self) {
        self.x1 = 0.0;
        self.x2 = 0.0;
        self.y1 = 0.0;
        self.y2 = 0.0;
    }
}

/// Guadagni EQ in dB per i preset. Ordine: (nome, bassi, medi, alti).
fn eq_preset_gains(name: &str) -> Option<(f32, f32, f32)> {
    match name.trim().to_lowercase().as_str() {
        "flat" | "off" | "neutro" | "normal" => Some((0.0, 0.0, 0.0)),
        "bass" | "bassi" | "bassboost" => Some((8.0, 1.0, 0.0)),
        "treble" | "alti" | "trebleboost" => Some((-2.0, 1.0, 7.0)),
        "pop" => Some((2.0, 4.0, 3.0)),
        "rock" => Some((5.0, -1.0, 5.0)),
        "jazz" => Some((4.0, 2.0, 4.0)),
        "vocal" | "voce" | "voice" | "speech" => Some((-3.0, 4.0, 3.0)),
        "lofi" => Some((4.0, 1.0, -5.0)),
        "soft" | "night" | "notte" => Some((-1.0, 0.0, -3.0)),
        "dance" => Some((6.0, 0.0, 5.0)),
        _ => None,
    }
}

fn eq_preset_list() -> &'static str {
    "flat, bass, treble, pop, rock, jazz, vocal, lofi, soft, dance"
}

/// Equalizer completo: 3 biquad + guadagni correnti.
#[derive(Debug, Clone)]
struct Eq {
    bass_db: f32,
    mid_db: f32,
    treble_db: f32,
    low: Biquad,
    peak: Biquad,
    high: Biquad,
}

impl Eq {
    const SAMPLE_RATE: f32 = 48000.0;

    fn new(bass_db: f32, mid_db: f32, treble_db: f32) -> Self {
        let mut eq = Self {
            bass_db: 0.0,
            mid_db: 0.0,
            treble_db: 0.0,
            low: Biquad::identity(),
            peak: Biquad::identity(),
            high: Biquad::identity(),
        };
        eq.set_gains(bass_db, mid_db, treble_db);
        eq
    }

    fn is_flat(&self) -> bool {
        self.bass_db.abs() < 0.05 && self.mid_db.abs() < 0.05 && self.treble_db.abs() < 0.05
    }

    fn set_gains(&mut self, bass_db: f32, mid_db: f32, treble_db: f32) {
        self.bass_db = bass_db.clamp(-12.0, 12.0);
        self.mid_db = mid_db.clamp(-12.0, 12.0);
        self.treble_db = treble_db.clamp(-12.0, 12.0);
        self.low = Self::low_shelf(200.0, self.bass_db);
        self.peak = Self::peaking(1000.0, 1.0, self.mid_db);
        self.high = Self::high_shelf(6000.0, self.treble_db);
    }

    fn reset(&mut self) {
        self.low.reset();
        self.peak.reset();
        self.high.reset();
    }

    /// Applica l'EQ in place sui sample. No-op se flat.
    fn apply(&mut self, pcm: &mut [i16]) {
        if self.is_flat() {
            return;
        }
        for s in pcm.iter_mut() {
            let x = *s as f32 / 32768.0;
            let y = self.high.process(self.peak.process(self.low.process(x)));
            *s = (y * 32768.0).clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        }
    }

    fn peaking(freq: f32, q: f32, gain_db: f32) -> Biquad {
        if gain_db.abs() < 0.05 {
            return Biquad::identity();
        }
        let a = 10.0_f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * freq / Self::SAMPLE_RATE;
        let alpha = w0.sin() / (2.0 * q);
        let cos_w0 = w0.cos();
        let b0 = 1.0 + alpha * a;
        let b1 = -2.0 * cos_w0;
        let b2 = 1.0 - alpha * a;
        let a0 = 1.0 + alpha / a;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha / a;
        Biquad {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn low_shelf(freq: f32, gain_db: f32) -> Biquad {
        if gain_db.abs() < 0.05 {
            return Biquad::identity();
        }
        let a = 10.0_f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * freq / Self::SAMPLE_RATE;
        let alpha = w0.sin() / 2.0 * std::f32::consts::SQRT_2;
        let cos_w0 = w0.cos();
        let sqrt_a = 2.0 * a.sqrt() * alpha;
        let b0 = a * ((a + 1.0) - (a - 1.0) * cos_w0 + sqrt_a);
        let b1 = 2.0 * a * ((a - 1.0) - (a + 1.0) * cos_w0);
        let b2 = a * ((a + 1.0) - (a - 1.0) * cos_w0 - sqrt_a);
        let a0 = (a + 1.0) + (a - 1.0) * cos_w0 + sqrt_a;
        let a1 = -2.0 * ((a - 1.0) + (a + 1.0) * cos_w0);
        let a2 = (a + 1.0) + (a - 1.0) * cos_w0 - sqrt_a;
        Biquad {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn high_shelf(freq: f32, gain_db: f32) -> Biquad {
        if gain_db.abs() < 0.05 {
            return Biquad::identity();
        }
        let a = 10.0_f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * freq / Self::SAMPLE_RATE;
        let alpha = w0.sin() / 2.0 * std::f32::consts::SQRT_2;
        let cos_w0 = w0.cos();
        let sqrt_a = 2.0 * a.sqrt() * alpha;
        let b0 = a * ((a + 1.0) + (a - 1.0) * cos_w0 + sqrt_a);
        let b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cos_w0);
        let b2 = a * ((a + 1.0) + (a - 1.0) * cos_w0 - sqrt_a);
        let a0 = (a + 1.0) - (a - 1.0) * cos_w0 + sqrt_a;
        let a1 = 2.0 * ((a - 1.0) - (a + 1.0) * cos_w0);
        let a2 = (a + 1.0) - (a - 1.0) * cos_w0 - sqrt_a;
        Biquad {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Playlist salvate: nome -> lista di URL (video o playlist YouTube).
// File JSON semplice, es: {"lofi": ["https://..."]}. Modificabile a mano
// oppure con !playlistsave / !playlistdel direttamente dalla chat.
// ---------------------------------------------------------------------------

type SavedPlaylists = std::collections::HashMap<String, Vec<String>>;

/// Query di ricerca usata da `!lofi`: niente URL fissi (muoiono in fretta,
/// es. le live di Lofi Girl cambiano ID), si prendono i primi 5 mix trovati.
const LOFI_SEARCH: &str = "ytsearch5:lofi hip hop mix";

/// Spec di ricerca YouTube per yt-dlp: `!play awake and alive` diventa
/// `ytsearch1:awake and alive`, cioè il primo risultato della ricerca.
fn youtube_search_spec(query: &str) -> String {
    format!("ytsearch1:{query}")
}

/// true se la stringa è una spec di ricerca yt-dlp (`ytsearch...:`) e non un URL.
fn is_search_spec(s: &str) -> bool {
    s.starts_with("ytsearch")
}

/// Estrae l'ID video (11 caratteri) da un URL YouTube nei formati comuni.
fn video_id_from_youtube_url(url: &str) -> Option<&str> {
    // https://www.youtube.com/watch?v=ID...
    if let Some(pos) = url.find("v=") {
        let rest = &url[pos + 2..];
        let end = rest
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
            .unwrap_or(rest.len());
        let id = &rest[..end];
        if id.len() == 11 {
            return Some(id);
        }
    }
    // https://youtu.be/ID, /shorts/ID, /live/ID, /embed/ID
    for marker in ["youtu.be/", "/shorts/", "/live/", "/embed/"] {
        if let Some(pos) = url.find(marker) {
            let rest = &url[pos + marker.len()..];
            let end = rest
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
                .unwrap_or(rest.len());
            let id = &rest[..end];
            if id.len() == 11 {
                return Some(id);
            }
        }
    }
    None
}

/// Copertina derivata dall'ID video quando yt-dlp non la fornisce
/// (i risultati flat-playlist/ytsearch riportano "NA", ma le thumbnail
/// di YouTube seguono un pattern stabile: i.ytimg.com/vi/ID/hqdefault.jpg).
fn youtube_thumbnail_fallback(page_url: &str) -> Option<String> {
    video_id_from_youtube_url(page_url)
        .map(|id| format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg"))
}

fn normalize_playlist_name(name: &str) -> String {
    name.trim()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

fn load_playlists(path: &str) -> SavedPlaylists {
    match std::fs::read_to_string(path) {
        Ok(content) => match serde_json::from_str(&content) {
            Ok(map) => map,
            Err(e) => {
                warn!("playlist {path} non valide ({e}), uso vuote");
                SavedPlaylists::new()
            }
        },
        Err(_) => SavedPlaylists::new(),
    }
}

fn save_playlists(path: &str, playlists: &SavedPlaylists) {
    match serde_json::to_string_pretty(playlists) {
        Ok(json) => {
            if let Err(e) = std::fs::write(path, json) {
                warn!("salvataggio playlist {path} fallito: {e}");
            }
        }
        Err(e) => warn!("serializzazione playlist fallita: {e}"),
    }
}

/// Sorgente audio attiva. Resta nel task principale perché `Client` non è `Send`.
enum Source {
    Idle,
    Sine { freq: f64, phase: f64 },
    Stream { stream: PipeStream },
}

/// Processo yt-dlp -> ffmpeg con stdout PCM s16le mono 48kHz da leggere a frame.
struct PipeStream {
    /// Assente quando si riparte da un seek: lì ffmpeg apre la URL diretta
    /// da solo, senza yt-dlp a monte nella pipe.
    ytdlp: Option<tokio::process::Child>,
    ffmpeg: tokio::process::Child,
    stdout: tokio::process::ChildStdout,
    /// Buffer di accumulo: ffmpeg produce più in fretta del realtime,
    /// ne consumiamo un frame (960 sample) per tick da 20ms.
    pending: Vec<u8>,
}

impl PipeStream {
    /// Un frame = 960 sample mono s16le = 1920 byte.
    const FRAME_BYTES: usize = 960 * 2;
    /// Cap del buffer (~2s di audio): oltre, non leggiamo per fare backpressure su ffmpeg.
    const MAX_PENDING: usize = Self::FRAME_BYTES * 100;
}

/// Condivisione video (screen share TeamSpeak 6) del brano in riproduzione.
/// L'audio resta sul canale vocale; qui c'è solo l'immagine.
struct Video {
    broadcaster: Broadcaster,
    /// Stream aperto sul server (`setupstream` inviato). Si chiude da solo
    /// dopo `IDLE_TIMEOUT` senza brani e si riapre col brano successivo.
    open: bool,
    /// Da quando non c'è nessun brano in riproduzione.
    idle_since: Option<Instant>,
    /// In onda c'è l'animazione di attesa, non un video.
    showing_idle: bool,
    /// Ultimo avvio dell'animazione di attesa, per non riprovare a raffica
    /// quando ffmpeg la rifiuta.
    idle_started: Option<Instant>,
    /// Assegnato dal server dopo `setupstream`: finché è `None` le richieste
    /// dei viewer non si possono accettare.
    stream_id: Option<String>,
    /// L'audio del brano aspetta il primo frame video fino a quest'istante,
    /// così partono insieme. Poi parte comunque.
    hold_audio_until: Option<Instant>,
    /// Nome da dare allo stream (titolo del brano) appena possibile.
    pending_name: Option<String>,
}

impl Video {
    /// Quanto al massimo l'audio aspetta il video.
    const MAX_AUDIO_HOLD: Duration = Duration::from_secs(10);
    /// Nomi lunghi potrebbero essere rifiutati dal server: meglio accorciare.
    const MAX_NAME_CHARS: usize = 40;
    /// Quanto resta aperta la live senza brani prima di chiudersi.
    const IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
    /// Ogni quanto riprovare l'animazione di attesa se non parte.
    const IDLE_RETRY: Duration = Duration::from_secs(5);

    fn new(config: &BroadcastConfig) -> Self {
        Self {
            broadcaster: Broadcaster::new(config.clone()),
            open: false,
            idle_since: None,
            showing_idle: false,
            idle_started: None,
            stream_id: None,
            hold_audio_until: None,
            pending_name: None,
        }
    }

    /// Manda in onda l'animazione di attesa. Con `countdown` mostra anche
    /// quanto manca alla chiusura della live.
    fn show_idle(&mut self, height: u32, countdown: Option<Duration>) {
        if let Err(e) = self.broadcaster.play(VideoInput::Lavfi(idle_animation(height, countdown))) {
            warn!("animazione di attesa non avviata: {e}");
        }
        self.showing_idle = true;
        self.idle_started = Some(Instant::now());
    }

    fn play(&mut self, track: &Track, height: u32, at: Duration) {
        self.showing_idle = false;
        let command = ytdlp_video_command(&track.url, height, at);
        // Barra in basso: quanto manca alla fine del video. Senza durata nota
        // (live, URL diretti) si manda il video liscio come prima.
        let input = match track.duration_secs.map(|d| (d as f64, at.as_secs_f64())) {
            Some((total, off)) => match VideoProgress::new(total, off) {
                Some(progress) => VideoInput::CommandWithProgress { command, progress },
                None => VideoInput::Command(command),
            },
            None => VideoInput::Command(command),
        };
        match self.broadcaster.play(input) {
            Ok(()) => self.hold_audio_until = Some(Instant::now() + Self::MAX_AUDIO_HOLD),
            Err(e) => {
                warn!("video non avviato: {e}");
                self.hold_audio_until = None;
            }
        }
        self.pending_name = Some(track.display().chars().take(Self::MAX_NAME_CHARS).collect());
    }

    /// true finché l'audio deve aspettare il primo frame video.
    fn holds_audio(&mut self) -> bool {
        let waiting = self
            .hold_audio_until
            .is_some_and(|until| Instant::now() < until && !self.broadcaster.video_started());
        if !waiting {
            self.hold_audio_until = None;
        }
        waiting
    }
}

/// Schermata di attesa generata da ffmpeg, stile vecchio salvaschermo DVD:
/// il logo "MusicBot" rimbalza sui bordi e cambia colore a ogni urto, sopra
/// un gradiente che oscilla lentamente di tinta (periodico: niente derive nel
/// tempo). In basso il suggerimento e, con `countdown`, quanto manca alla
/// chiusura della live. Costa meno di mezzo core.
fn idle_animation(height: u32, countdown: Option<Duration>) -> String {
    let (w, h) = ((height * 16 / 9) & !1, height & !1);
    let px = |size: u32| (size * h / 480).max(1);
    // Il logo è un livello trasparente di dimensione fissa: così sappiamo in
    // anticipo dove rimbalza, e quanti rimbalzi ha fatto al tempo t.
    let (lw, lh) = (px(240), px(100));
    let (vx, vy) = (px(90), px(70));
    let bounces = format!("floor(t*{vx}/{})+floor(t*{vy}/{})", w - lw, h - lh);

    // `speed` sta fermo sul minimo, non a zero: ffmpeg 5.1 (Debian bookworm,
    // l'immagine del bot) rifiuta 0 con "out of range [1e-05 - 1]" e il grafo
    // non parte affatto. A muovere le tinte ci pensa il `hue` qui sotto.
    let mut graph = format!(
        "gradients=s={w}x{h}:r=30:c0=0x0f0c29:c1=0x302b63:c2=0x24243e:c3=0x6a3093:nb_colors=4:speed=0.00001:type=radial:x0={cx}:y0={cy}:x1=0:y1=0,\
         hue=h='40*sin(2*PI*t/20)'",
        cx = w / 2,
        cy = h / 2,
    );
    let hint = match countdown {
        Some(_) => "In attesa del prossimo brano  ·  !play <url o titolo>",
        None => "Solo audio  ·  il video di questo brano è finito",
    };
    graph.push_str(&format!(
        ",drawtext=font=Sans:text='{hint}':fontcolor=white@0.7:fontsize={}:x=(w-tw)/2:y=h-{}",
        px(18),
        px(60),
    ));
    if let Some(left) = countdown {
        // Minuti:secondi calcolati da ffmpeg sul tempo dell'animazione, che
        // parte insieme al conto alla rovescia.
        let secs = left.as_secs();
        graph.push_str(&format!(
            r",drawtext=font=Sans:text='La live si chiude tra %{{eif\:max(0\,{secs}-t)/60\:d}}\:%{{eif\:mod(max(0\,{secs}-t)\,60)\:d\:2}}':fontcolor=white@0.45:fontsize={}:x=(w-tw)/2:y=h-{}",
            px(15),
            px(32),
        ));
    }
    graph.push_str(&format!("[bg];color=c=black@0:s={lw}x{lh}:r=30,format=rgba"));
    graph.push_str(&format!(
        r",drawtext=font='Sans\:bold':text='MusicBot':fontcolor=0xff4d6d:borderw=2:bordercolor=black@0.35:fontsize={}:x=(w-tw)/2:y=(h-th)/2-{}",
        px(46),
        px(10),
    ));
    graph.push_str(&format!(
        r",drawtext=font='Sans\:bold':text='VIDEO':fontcolor=0xff4d6d:fontsize={}:x=(w-tw)/2:y=h-{}",
        px(18),
        px(26),
    ));
    graph.push_str(&format!(",hue=h='72*({bounces})'[logo]"));
    graph.push_str(&format!(
        r";[bg][logo]overlay=x='abs(mod(t*{vx}\,2*(W-w))-(W-w))':y='abs(mod(t*{vy}\,2*(H-h))-(H-h))'"
    ));
    graph
}

/// `yt-dlp` con lo stream solo-video, su stdout. Preferisce H.264 (decodifica
/// leggera) entro l'altezza richiesta; il ricodificare in VP8 lo fa tslib-stream.
///
/// Con `at` > 0 (seek) è ffmpeg ad aprire la URL diretta (`yt-dlp -g`) e a
/// posizionarsi con `-ss`, rigirando il flusso in matroska senza ricodificare:
/// tslib-stream riceve un input già posizionato, come per l'audio. Sulla pipe
/// il seek non funzionerebbe: l'MP4 di YouTube non è demuxabile all'indietro.
fn ytdlp_video_command(url: &str, height: u32, at: Duration) -> std::process::Command {
    let format = format!("bv*[height<={height}][vcodec^=avc1]/bv*[height<={height}]/b[height<={height}]/b");
    if at.is_zero() {
        let mut cmd = std::process::Command::new("yt-dlp");
        cmd.args(["-f", &format, "-o", "-", "--no-playlist", "--quiet", "--no-warnings", url]);
        return cmd;
    }
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c").arg(format!(
        "exec ffmpeg -hide_banner -loglevel error -ss {ss:.3} \
         -i \"$(yt-dlp -f {format} -g --no-playlist --quiet --no-warnings {url})\" \
         -c copy -f matroska pipe:1",
        ss = at.as_secs_f64(),
        format = shell_quote(&format),
        url = shell_quote(url),
    ));
    cmd
}

/// Stato di riproduzione + coda.
struct Player {
    source: Source,
    queue: VecDeque<Track>,
    /// Brano corrente (URL + titolo) o descrizione ("sine 440Hz").
    current: Option<Track>,
    /// Volume 0.0-2.0 (1.0 = 100%). Applicato ai sample PCM prima dell'encode Opus.
    volume: f32,
    /// Pausa: se true, non si legge da ffmpeg né si invia audio
    /// (ffmpeg si blocca da solo per backpressure sulla pipe).
    paused: bool,
    /// Equalizer a 3 bande (applicato prima del volume).
    eq: Eq,
    /// Nome preset EQ attivo ("flat" oppure "custom").
    eq_preset: String,
    /// Modalità di ripetizione.
    loop_mode: LoopMode,
    /// Ultimi brani avviati (più recente per primo, max `RECENT_MAX`).
    recent: Vec<Track>,
    /// true se `recent` è cambiato e va persistito nel file stato.
    recent_dirty: bool,
    /// true se la coda è stata sostituita da una nuova playlist mentre questo
    /// brano suonava: a fine brano non va ripetuto né rimesso in coda dal loop,
    /// si passa direttamente alla nuova playlist.
    current_replaced: bool,
    /// Quanto del brano corrente è già stato riprodotto: i frame emessi da
    /// 20ms più l'eventuale punto di partenza di un seek. Base di !avanti,
    /// !indietro e !seek.
    position: Duration,
    /// Condivisione video, se attiva (!video on).
    video: Option<Video>,
    /// Configurazione con cui aprire la condivisione video.
    video_config: BroadcastConfig,
}

impl Player {
    /// Quanti brani ricordare nello storico.
    const RECENT_MAX: usize = 10;

    fn new(
        volume: f32,
        eq: Eq,
        eq_preset: String,
        loop_mode: LoopMode,
        recent: Vec<Track>,
        video_config: BroadcastConfig,
    ) -> Self {
        Self {
            source: Source::Idle,
            queue: VecDeque::new(),
            current: None,
            volume,
            paused: false,
            eq,
            eq_preset,
            loop_mode,
            recent,
            recent_dirty: false,
            current_replaced: false,
            position: Duration::ZERO,
            video: None,
            video_config,
        }
    }

    /// Avvia audio (e video, se attivo) di un brano. Non tocca la sorgente
    /// corrente: se fallisce, quella resta com'era.
    fn spawn_track(&mut self, track: &Track, at: Duration) -> Result<PipeStream> {
        let stream = spawn_stream(&track.url, at)?;
        let height = self.video_config.encoder.height;
        if let Some(video) = &mut self.video {
            video.play(track, height, at);
        }
        Ok(stream)
    }

    /// Apre la condivisione video. Il brano in corso riparte da capo, così
    /// audio e video partono allineati.
    fn enable_video(&mut self, client: &mut Client) -> Result<String> {
        if self.video.is_some() {
            return Ok("Video già attivo.".to_string());
        }
        let name = self
            .current
            .as_ref()
            .map(|t| t.display().chars().take(Video::MAX_NAME_CHARS).collect())
            .unwrap_or_else(|| "MusicBot".to_string());
        client.setup_stream(&StreamSetup::new(name, self.video_config.encoder.bitrate_kbps * 1000))?;
        let mut video = Video::new(&self.video_config);
        video.open = true;
        self.video = Some(video);
        let restarted = matches!(self.source, Source::Stream { .. }) && self.restart_current().is_some();
        Ok(if restarted {
            "Video attivo: apri lo stream del bot nel client TeamSpeak 6. Riparto il brano da capo per sincronizzare audio e video.".to_string()
        } else {
            "Video attivo: apri lo stream del bot nel client TeamSpeak 6. Il video parte col prossimo brano.".to_string()
        })
    }

    /// Chiude la condivisione video (i viewer vengono scollegati).
    fn disable_video(&mut self, client: &mut Client) -> String {
        let Some(video) = self.video.take() else {
            return "Video già spento.".to_string();
        };
        if let (true, Some(id)) = (video.open, &video.stream_id) {
            if let Err(e) = client.stop_stream(id) {
                warn!("stopstream: {e}");
            }
        }
        "Video spento.".to_string()
    }

    /// Registra un brano nello storico (in testa, senza duplicati).
    fn remember(&mut self, track: &Track) {
        self.recent.retain(|t| t.url != track.url);
        self.recent.insert(0, track.clone());
        self.recent.truncate(Self::RECENT_MAX);
        self.recent_dirty = true;
    }

    fn is_busy(&self) -> bool {
        !matches!(self.source, Source::Idle)
    }

    fn stop_all(&mut self) {
        kill_source(&mut self.source);
        self.queue.clear();
        self.current = None;
        self.paused = false;
        self.current_replaced = false;
    }

    /// Sostituisce la coda con una nuova playlist lasciando finire il brano corrente.
    fn replace_queue(&mut self, tracks: Vec<Track>) {
        self.queue = tracks.into();
        self.current_replaced = true;
    }

    fn current_display(&self) -> String {
        self.current
            .as_ref()
            .map(|t| t.display().to_string())
            .unwrap_or_else(|| "-".to_string())
    }

    /// Fa partire il brano subito, uccidendo la sorgente precedente.
    /// L'annuncio include titolo + copertina inline ([img] = preview, non file).
    fn start_track_now(&mut self, track: Track) -> String {
        match self.spawn_track(&track, Duration::ZERO) {
            Ok(stream) => {
                kill_source(&mut self.source);
                self.source = Source::Stream { stream };
                let msg = track.announce("Riproduco");
                self.remember(&track);
                self.current = Some(track);
                self.current_replaced = false;
                self.paused = false;
                self.position = Duration::ZERO;
                self.eq.reset();
                msg
            }
            Err(e) => format!("Play fallito: {e:#}"),
        }
    }

    /// Fa partire il prossimo in coda. Ritorna il messaggio da annunciare (se c'è).
    fn start_next(&mut self) -> Option<String> {
        let next = self.queue.pop_front()?;
        match self.spawn_track(&next, Duration::ZERO) {
            Ok(stream) => {
                kill_source(&mut self.source);
                self.source = Source::Stream { stream };
                let msg = next.announce("Prossimo");
                self.remember(&next);
                self.current = Some(next);
                self.current_replaced = false;
                self.paused = false;
                self.position = Duration::ZERO;
                self.eq.reset();
                Some(msg)
            }
            Err(e) => Some(format!("Play fallito per {}: {e:#}", next.display())),
        }
    }

    /// Fa ripartire il brano corrente (usato dal loop "one").
    fn restart_current(&mut self) -> Option<String> {
        let cur = self.current.clone()?;
        match self.spawn_track(&cur, Duration::ZERO) {
            Ok(stream) => {
                kill_source(&mut self.source);
                self.source = Source::Stream { stream };
                self.paused = false;
                self.position = Duration::ZERO;
                self.eq.reset();
                Some(cur.announce("Ripeto"))
            }
            Err(e) => Some(format!("Replay fallito per {}: {e:#}", cur.display())),
        }
    }

    /// Sposta la riproduzione del brano corrente a `target`, riavviando la
    /// pipe (yt-dlp/ffmpeg non sono riposizionabili a caldo). Il video, se
    /// attivo, riparte dallo stesso punto così resta in sincrono.
    fn seek_to(&mut self, target: Duration) -> String {
        if !matches!(self.source, Source::Stream { .. }) {
            return "Niente da spostare: nessun brano in riproduzione.".to_string();
        }
        let Some(track) = self.current.clone() else {
            return "Niente da spostare: nessun brano in riproduzione.".to_string();
        };
        match self.spawn_track(&track, target) {
            Ok(stream) => {
                kill_source(&mut self.source);
                self.source = Source::Stream { stream };
                self.position = target;
                self.paused = false;
                self.eq.reset();
                format!("{} a {}", track.display(), format_pos(target))
            }
            // La sorgente precedente è intatta: `spawn_track` non l'ha toccata.
            Err(e) => format!("Spostamento fallito: {e:#}"),
        }
    }

    /// `!avanti` / `!indietro`: seek relativo alla posizione corrente,
    /// senza andare sotto zero. Oltre la fine del brano, questo finisce e si
    /// passa al successivo, come per una riproduzione normale.
    fn seek_by(&mut self, delta: i64) -> String {
        if !matches!(self.source, Source::Stream { .. }) {
            return "Niente da spostare: nessun brano in riproduzione.".to_string();
        }
        let now = self.position.as_secs() as i64;
        let target = Duration::from_secs((now + delta).max(0) as u64);
        let verb = if delta >= 0 { "Avanti" } else { "Indietro" };
        format!("{verb} di {}s: {}", delta.abs(), self.seek_to(target))
    }

    fn eq_summary(&self) -> String {
        format!(
            "EQ {} (bass {:+.0}dB, mid {:+.0}dB, treble {:+.0}dB)",
            self.eq_preset, self.eq.bass_db, self.eq.mid_db, self.eq.treble_db
        )
    }
}

/// Posizione nel brano come `m:ss` (o `h:mm:ss` oltre l'ora).
fn format_pos(pos: Duration) -> String {
    let secs = pos.as_secs();
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Barra testuale di avanzamento per `!now` (la stessa info della barra in
/// basso nel video): `[██████░░░░] 60%`, con quanto manca deducibile dalla
/// parte vuota + `pos / totale` mostrato accanto.
fn text_progress_bar(pos_secs: u64, total_secs: u64) -> String {
    const WIDTH: u64 = 12;
    if total_secs == 0 {
        return String::new();
    }
    let pos = pos_secs.min(total_secs);
    let filled = (pos * WIDTH / total_secs) as usize;
    let pct = pos * 100 / total_secs;
    format!(
        "[{}{}] {pct}%",
        "█".repeat(filled),
        "░".repeat((WIDTH as usize).saturating_sub(filled))
    )
}

/// Interpreta la posizione di `!seek`: `90`, `1:30` o `1:02:03`.
fn parse_pos(raw: &str) -> Option<Duration> {
    let mut secs: u64 = 0;
    let parts: Vec<&str> = raw.split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    for part in &parts {
        secs = secs * 60 + part.trim().parse::<u64>().ok()?;
    }
    Some(Duration::from_secs(secs))
}

fn kill_source(source: &mut Source) {
    if let Source::Stream { stream } = source {
        if let Some(ytdlp) = &mut stream.ytdlp {
            let _ = ytdlp.start_kill();
        }
        let _ = stream.ffmpeg.start_kill();
    }
    *source = Source::Idle;
}

fn is_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// Scala i sample PCM per il volume con clipping. `volume`: 0.0 = muto, 1.0 = 100%.
fn apply_volume(pcm: &mut [i16], volume: f32) {
    if (volume - 1.0).abs() < f32::EPSILON {
        return;
    }
    if volume <= 0.0 {
        for s in pcm.iter_mut() {
            *s = 0;
        }
        return;
    }
    for s in pcm.iter_mut() {
        *s = (*s as f32 * volume).clamp(i16::MIN as f32, i16::MAX as f32) as i16;
    }
}

/// Avvia `yt-dlp -f bestaudio -o - <url>` pipato in
/// `ffmpeg -i pipe:0 -f s16le -ar 48000 -ac 1 pipe:1`.
///
/// Con `at` > 0 (seek) si salta il passaggio per la pipe: `yt-dlp -g` dà la
/// URL diretta del flusso e ffmpeg ci si posiziona sopra con `-ss`, scaricando
/// via HTTP solo da lì in avanti. La risoluzione avviene dentro la shell
/// figlia, così il tick audio da 20ms non si blocca ad aspettare yt-dlp.
fn spawn_stream(url: &str, at: Duration) -> Result<PipeStream> {
    if !is_url(url) {
        anyhow::bail!("URL non valido (deve iniziare con http:// o https://)");
    }
    if !at.is_zero() {
        return spawn_seeked_stream(url, at);
    }
    let mut ytdlp = tokio::process::Command::new("yt-dlp")
        .args([
            "-f",
            "bestaudio",
            "-o",
            "-",
            "--no-playlist",
            "--quiet",
            "--no-warnings",
            url,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("yt-dlp non trovato o non avviabile (installarlo sul host)")?;

    let ytdlp_out = ytdlp
        .stdout
        .take()
        .context("yt-dlp: stdout non disponibile")?;
    let ytdlp_stdio: Stdio = ytdlp_out
        .try_into()
        .map_err(|_| anyhow::anyhow!("conversione stdout yt-dlp fallita"))?;

    let mut ffmpeg = tokio::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            "pipe:0",
            "-f",
            "s16le",
            "-ar",
            "48000",
            "-ac",
            "1",
            "pipe:1",
        ])
        .stdin(ytdlp_stdio)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("ffmpeg non trovato o non avviabile (installarlo sul host)")?;

    let stdout = ffmpeg
        .stdout
        .take()
        .context("ffmpeg: stdout non disponibile")?;

    Ok(PipeStream {
        ytdlp: Some(ytdlp),
        ffmpeg,
        stdout,
        pending: Vec::with_capacity(8192),
    })
}

/// Audio del brano a partire da `at`, come sopra ma senza yt-dlp nella pipe:
/// è ffmpeg ad aprire la URL diretta, che (a differenza di uno stdin) può
/// posizionare con una richiesta HTTP range invece di leggere tutto.
fn spawn_seeked_stream(url: &str, at: Duration) -> Result<PipeStream> {
    let script = format!(
        "exec ffmpeg -hide_banner -loglevel error -ss {ss:.3} \
         -i \"$(yt-dlp -f bestaudio -g --no-playlist --quiet --no-warnings {url})\" \
         -f s16le -ar 48000 -ac 1 pipe:1",
        ss = at.as_secs_f64(),
        url = shell_quote(url),
    );
    let mut ffmpeg = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("sh/ffmpeg non avviabile")?;

    let stdout = ffmpeg.stdout.take().context("ffmpeg: stdout non disponibile")?;

    Ok(PipeStream {
        ytdlp: None,
        ffmpeg,
        stdout,
        pending: Vec::with_capacity(8192),
    })
}

/// Racchiude un argomento per `sh -c`. Le URL arrivano dalla chat: senza
/// questo, un apice nell'URL farebbe eseguire il resto come comando.
fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// Risolve un URL in uno o più brani con titolo + copertina + durata.
///
/// Usa `yt-dlp --flat-playlist --print "%(title)s ||| %(webpage_url)s ||| %(thumbnail)s ||| %(duration)s"`:
/// se l'URL è una playlist (o un video con `&list=`), restituisce tutti i brani;
/// se è un singolo video, restituisce un solo brano.
///
/// Accetta anche una spec di ricerca (`ytsearch1:awake and alive`): in quel caso
/// yt-dlp restituisce i risultati della ricerca YouTube.
///
/// In caso di errore torna comunque un brano con l'URL grezzo così la
/// riproduzione parte lo stesso; per una ricerca invece torna vuoto, perché
/// la spec `ytsearch...` non è riproducibile da `spawn_stream`.
async fn resolve_tracks(url: String) -> Vec<Track> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        tokio::process::Command::new("yt-dlp")
            .args([
                "--flat-playlist",
                "--no-warnings",
                "--quiet",
                "--print",
                "%(title)s ||| %(webpage_url)s ||| %(thumbnail)s ||| %(duration)s",
                &url,
            ])
            .output(),
    )
    .await;

    let output = match out {
        Ok(Ok(o)) if o.status.success() => o,
        Ok(Ok(o)) => {
            warn!(
                "yt-dlp titolo fallito per {url}: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            return fallback_tracks(url);
        }
        _ => {
            warn!("yt-dlp titolo timeout/errore per {url}");
            return fallback_tracks(url);
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut tracks = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Formato: "titolo ||| url ||| thumbnail ||| durata".
        // Thumbnail e durata possono mancare/essere "NA".
        let mut parts = line.splitn(4, " ||| ");
        let title = parts.next().unwrap_or("").trim();
        let link = parts.next().unwrap_or("").trim();
        let thumb = parts.next().unwrap_or("").trim();
        let duration = parts.next().unwrap_or("").trim();
        if link.is_empty() {
            continue;
        }
        let thumb = match thumb {
            "" | "NA" | "none" => youtube_thumbnail_fallback(link),
            t => Some(t.to_string()),
        };
        let title = if title.is_empty() || title == "NA" {
            None
        } else {
            Some(title.to_string())
        };
        tracks.push(Track::new(link.to_string(), title, thumb, parse_duration(duration)));
    }
    if tracks.is_empty() {
        fallback_tracks(url)
    } else {
        info!("Risolti {} brani da {url}", tracks.len());
        tracks
    }
}

/// Durata di yt-dlp (`%(duration)s`): secondi, anche decimali, oppure "NA".
fn parse_duration(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("na") || raw == "none" {
        return None;
    }
    raw.parse::<f64>().ok().filter(|d| d.is_finite() && *d > 0.0).map(|d| d as u64)
}

/// Fallback quando yt-dlp non risolve: l'URL grezzo si può comunque provare
/// a riprodurre, una ricerca senza risultati no.
fn fallback_tracks(url: String) -> Vec<Track> {
    if is_search_spec(&url) {
        Vec::new()
    } else {
        vec![Track::new(url, None, None, None)]
    }
}

/// Risolve una lista di URL (playlist salvata) concatenando i risultati.
async fn resolve_url_list(urls: Vec<String>) -> Vec<Track> {
    let mut all = Vec::new();
    for url in urls {
        let mut tracks = resolve_tracks(url).await;
        all.append(&mut tracks);
    }
    all
}

/// Brani risolti in background e inviati al main loop. `label` (es. il titolo
/// della playlist YouTube trovata) viene mostrato nell'annuncio in canale.
/// `replace_queue`: è una playlist caricata esplicitamente (!playlist,
/// !ytplaylist) e deve sostituire la coda invece di accodarsi.
struct Resolved {
    tracks: Vec<Track>,
    label: Option<String>,
    replace_queue: bool,
}

impl Resolved {
    fn plain(tracks: Vec<Track>) -> Self {
        Self { tracks, label: None, replace_queue: false }
    }

    fn playlist(tracks: Vec<Track>, label: Option<String>) -> Self {
        Self { tracks, label, replace_queue: true }
    }
}

/// URL di ricerca YouTube filtrata sulle sole playlist (`sp=EgIQAw==`):
/// yt-dlp non ha un prefisso tipo `ytsearch` per le playlist.
fn youtube_playlist_search_url(query: &str) -> String {
    let mut q = String::new();
    for b in query.trim().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                q.push(b as char)
            }
            b' ' => q.push('+'),
            _ => q.push_str(&format!("%{b:02X}")),
        }
    }
    format!("https://www.youtube.com/results?search_query={q}&sp=EgIQAw%3D%3D")
}

/// Prima playlist YouTube che corrisponde a `query`: (titolo, URL playlist).
async fn find_youtube_playlist(query: &str) -> Option<(String, String)> {
    let search_url = youtube_playlist_search_url(query);
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        tokio::process::Command::new("yt-dlp")
            .args([
                "--flat-playlist",
                "--no-warnings",
                "--quiet",
                "--playlist-items",
                "1",
                "--print",
                "%(title)s ||| %(url)s",
                &search_url,
            ])
            .output(),
    )
    .await;

    let output = match out {
        Ok(Ok(o)) if o.status.success() => o,
        Ok(Ok(o)) => {
            warn!(
                "yt-dlp ricerca playlist fallita per '{query}': {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            return None;
        }
        _ => {
            warn!("yt-dlp ricerca playlist timeout/errore per '{query}'");
            return None;
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().map(str::trim).find(|l| !l.is_empty())?;
    let mut parts = line.splitn(2, " ||| ");
    let title = parts.next().unwrap_or("").trim();
    let link = parts.next().unwrap_or("").trim();
    if !link.contains("list=") {
        return None;
    }
    let title = if title.is_empty() || title == "NA" { link } else { title };
    info!("Playlist trovata per '{query}': {title} ({link})");
    Some((title.to_string(), link.to_string()))
}

/// Benvenuto in attesa: chi è entrato e quando mandare il messaggio.
struct PendingWelcome {
    user_id: u16,
    nickname: String,
    at: tokio::time::Instant,
}

/// true se `user_id` è appena entrato in `channel_id` e nel canale, a parte
/// il bot e lui, non c'è nessun altro (client query esclusi).
fn joined_lonely_bot(client: &Client, user_id: u16, channel_id: u64) -> bool {
    let Some(me) = client.client_id() else {
        return false;
    };
    if user_id == me || client.channel_id() != Some(channel_id) {
        return false;
    }
    !client.users().iter().any(|u| {
        u.channel_id == channel_id && u.id != me && u.id != user_id && u.client_type == 0
    })
}

/// Messaggio di benvenuto: brano corrente, playlist salvate, ultimi brani.
fn welcome_message(nickname: &str, player: &Player, playlists: &SavedPlaylists) -> String {
    let mut out = format!("[b]Ciao {nickname}![/b] Sono il music bot, scrivi !help per i comandi.\n");
    if let Some(cur) = &player.current {
        let state = if player.paused { " (in pausa)" } else { "" };
        out.push_str(&format!("[b]In riproduzione{state}:[/b] {}\n", cur.display()));
        if !player.queue.is_empty() {
            out.push_str(&format!("In coda: {} brani (!queue)\n", player.queue.len()));
        }
    }
    if playlists.is_empty() {
        out.push_str("Nessuna playlist salvata (!playlistsave <nome>).\n");
    } else {
        let mut names: Vec<(&String, usize)> =
            playlists.iter().map(|(n, v)| (n, v.len())).collect();
        names.sort();
        let list: Vec<String> = names.iter().map(|(n, c)| format!("{n} ({c})")).collect();
        out.push_str(&format!(
            "[b]Playlist salvate:[/b] {} → !playlist <nome>\n",
            list.join(", ")
        ));
    }
    if !player.recent.is_empty() {
        out.push_str("[b]Ultimi brani:[/b]\n");
        for (i, t) in player.recent.iter().take(5).enumerate() {
            out.push_str(&format!("{}. [url={}]{}[/url]\n", i + 1, t.url, t.display()));
        }
    }
    out.push_str("Per ascoltare: !play <titolo o url>, !lofi, !ytplaylist <ricerca>");
    out
}

#[tokio::main]
async fn main() -> Result<()> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    let args = Args::parse();

    // Stato persistente: volume + ultimo canale (il --channel esplicito vince).
    let mut saved = BotState::load(&args.state);
    saved.volume = BotState::sanitized_volume(saved.volume);
    saved.eq_bass = BotState::sanitized_eq_db(saved.eq_bass);
    saved.eq_mid = BotState::sanitized_eq_db(saved.eq_mid);
    saved.eq_treble = BotState::sanitized_eq_db(saved.eq_treble);
    if eq_preset_gains(&saved.eq_preset).is_none() {
        saved.eq_preset = default_eq_preset();
    }
    let loop_mode = LoopMode::parse(&saved.loop_mode).unwrap_or(LoopMode::Off);
    saved.loop_mode = loop_mode.as_str().to_string();
    if saved.volume != 1.0 {
        info!("Volume ripristinato: {:.0}%", saved.volume * 100.0);
    }
    if loop_mode != LoopMode::Off {
        info!("Loop ripristinato: {}", loop_mode.as_str());
    }

    let identity = if std::path::Path::new(&args.identity).exists() {
        info!("Carico identità da {}", args.identity);
        Identity::load(&args.identity)?
    } else {
        info!("Creo nuova identità");
        let id = Identity::create()?;
        id.save(&args.identity)?;
        id
    };
    info!("UID: {}", identity.unique_id());

    let mut cfg = ClientConfig::builder()
        .address(&args.server)
        .identity(identity)
        .nickname(&args.nickname);
    if let Some(pw) = args.password {
        cfg = cfg.password(pw);
    }
    if let Some(ch) = args.channel.clone() {
        cfg = cfg.channel(ch);
    } else if let Some(name) = saved.channel_name.clone() {
        // Ritorna nell'ultimo canale prima di uscire (per nome: gli ID possono cambiare).
        info!("Ritorno nell'ultimo canale: {name}");
        cfg = cfg.channel(name);
    }
    let config = cfg.build()?;

    // SIGTERM (docker stop / compose down): nel container siamo PID 1 e senza
    // handler il segnale viene ignorato, Docker aspetta 10s e fa SIGKILL, e il
    // server vede "Timed Out". Lo gestiamo come Ctrl+C: disconnessione pulita.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("registrazione handler SIGTERM fallita")?;

    // Retry con backoff esponenziale: un exit immediato + restart di Docker
    // martella il server e fa scattare il ban anti-flood (ConnectFailedBanned).
    let mut client = {
        let mut delay = tokio::time::Duration::from_secs(5);
        let max_delay = tokio::time::Duration::from_secs(300);
        loop {
            info!("Connessione a {}...", args.server);
            let attempt: Result<Client> = async {
                let mut c = Client::connect(config.clone())?;
                c.wait_connected().await?;
                Ok(c)
            }
            .await;
            match attempt {
                Ok(c) => break c,
                Err(e) => {
                    warn!("Connessione fallita: {e:#}. Riprovo tra {}s", delay.as_secs());
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = sigterm.recv() => {
                            info!("SIGTERM durante la connessione, esco");
                            return Ok(());
                        }
                        _ = tokio::signal::ctrl_c() => return Ok(()),
                    }
                    delay = (delay * 2).min(max_delay);
                }
            }
        }
    };
    info!("Connesso! Comandi con prefisso '{}'", args.prefix);

    // Mostra microfono come NON mutato così gli altri vedono l'icona corretta
    // quando trasmettiamo musica.
    let _ = client.set_input_muted(false);

    // --- Encoder Opus per la musica (mono 48kHz, 20ms) ---
    let audio_cfg = AudioConfig {
        channels: 1,
        opus_application: OpusApplication::Audio,
        vad_enabled: false,
        ..AudioConfig::default()
    };
    let frame_samples = audio_cfg.frame_size_samples(); // 960
    let mut encoder = OpusEncoder::new(&audio_cfg)?;
    let mut encode_buf = vec![0u8; 1024];

    let mut player = Player::new(
        saved.volume,
        Eq::new(saved.eq_bass, saved.eq_mid, saved.eq_treble),
        saved.eq_preset.clone(),
        loop_mode,
        saved.recent.clone(),
        BroadcastConfig {
            encoder: EncoderConfig {
                height: args.video_height,
                bitrate_kbps: args.video_bitrate,
                ..Default::default()
            },
            bind_addrs: args.video_bind.clone().into_iter().collect::<Vec<_>>(),
            ..Default::default()
        },
    );
    if player.video_config.bind_addrs.is_empty() {
        player.video_config.bind_addrs = BroadcastConfig::default().bind_addrs;
    }
    // Video all'avvio: per scelta fissa (--video-auto) o perché era acceso
    // quando il bot si è fermato l'ultima volta.
    if args.video_auto || saved.video {
        let why = if args.video_auto { "automatico (--video-auto)" } else { "ripristinato" };
        match player.enable_video(&mut client) {
            Ok(_) => info!("Video {why}"),
            Err(e) => warn!("video all'avvio fallito: {e:#}"),
        }
    }
    let mut playlists = load_playlists(&args.playlists);
    if playlists.is_empty() {
        info!("Nessuna playlist salvata in {}", args.playlists);
    } else {
        info!("Caricate {} playlist da {}", playlists.len(), args.playlists);
    }
    let sample_rate = audio_cfg.sample_rate as f64;

    // Risoluzione titoli/playlist in background: `handle_chat` fa solo
    // `tokio::spawn(resolve_tracks(url))` e risponde subito "Caricamento...",
    // così il tick audio da 20ms non si blocca. I risultati arrivano qui.
    let (meta_tx, mut meta_rx) = tokio::sync::mpsc::unbounded_channel::<Resolved>();

    let mut tick = tokio::time::interval(tokio::time::Duration::from_millis(20));

    // Benvenuto: ignora gli eventi dei primi secondi (sync iniziale della lista
    // utenti) così al connect non si salutano tutti quelli già presenti.
    let welcome_delay = tokio::time::Duration::from_millis(args.welcome_delay_ms);
    let welcome_armed_at = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
    let mut pending_welcome: Option<PendingWelcome> = None;

    loop {
        tokio::select! {
            _ = tick.tick() => {
                // 1) Eventi server (poll)
                match client.process_events().await {
                    Ok(events) => {
                        for ev in events {
                            match ev {
                                Event::TextMessage { sender_id, sender_name, message, target } => {
                                    info!("[chat] {sender_name}: {message}");
                                    if let Some(reply) = handle_chat(
                                        &mut client,
                                        &args.prefix,
                                        sender_id,
                                        &sender_name,
                                        &message,
                                        &mut player,
                                        &args.state,
                                        &mut saved,
                                        &args.playlists,
                                        &mut playlists,
                                        meta_tx.clone(),
                                    ) {
                                        // Risponde nello stesso contesto:
                                        // DM/server → messaggio privato, canale → messaggio in canale
                                        let res = if matches!(target, MessageTarget::Channel) {
                                            client.send_channel_message(reply)
                                        } else {
                                            client.send_private_message(sender_id, reply)
                                        };
                                        if let Err(e) = res {
                                            warn!("reply fallita: {e}");
                                        }
                                    }
                                }
                                Event::UserJoined { user } => {
                                    info!("[join] {}", user.nickname);
                                    if args.welcome_delay_ms > 0
                                        && tokio::time::Instant::now() >= welcome_armed_at
                                        && pending_welcome.is_none()
                                        && joined_lonely_bot(&client, user.id, user.channel_id)
                                    {
                                        pending_welcome = Some(PendingWelcome {
                                            user_id: user.id,
                                            nickname: user.nickname.clone(),
                                            at: tokio::time::Instant::now() + welcome_delay,
                                        });
                                    }
                                }
                                Event::UserLeft { user, .. } => {
                                    info!("[leave] {}", user.nickname);
                                    // Chi esce dal server non manda notifystreamclientleft.
                                    if let Some(video) = &mut player.video {
                                        video.broadcaster.remove_viewer(user.id);
                                    }
                                }
                                Event::StreamsChanged { .. } => {
                                    if let Some(video) = &mut player.video {
                                        // Lo stream che abbiamo già resta finché esiste: se ne
                                        // compare un secondo non va preso il primo della lista.
                                        let own_streams = client.own_streams();
                                        let own = video
                                            .stream_id
                                            .clone()
                                            .filter(|id| own_streams.iter().any(|s| &s.id == id))
                                            .or_else(|| own_streams.into_iter().next().map(|s| s.id));
                                        if own != video.stream_id {
                                            info!("Stream video: {own:?}");
                                            video.stream_id = own;
                                        }
                                    }
                                }
                                Event::StreamJoinRequest { viewer_id, stream_id, is_remove } => {
                                    let current = player
                                        .video
                                        .as_mut()
                                        .filter(|v| v.stream_id.as_deref() == Some(stream_id.as_str()));
                                    match current {
                                        Some(video) if is_remove => {
                                            info!("[video] {viewer_id} ha chiuso lo stream");
                                            video.broadcaster.remove_viewer(viewer_id);
                                        }
                                        Some(video) => {
                                            info!("[video] {viewer_id} apre lo stream");
                                            video.broadcaster.add_viewer(viewer_id);
                                        }
                                        None if is_remove => {}
                                        // Senza risposta il client resta su "Waiting to be let in".
                                        None => {
                                            warn!("[video] {viewer_id} chiede lo stream sconosciuto {stream_id}: rifiuto");
                                            if let Err(e) = client.refuse_stream_viewer(viewer_id, &stream_id) {
                                                warn!("refuse_stream_viewer: {e}");
                                            }
                                        }
                                    }
                                }
                                Event::StreamSignaling { owner_id: viewer_id, stream_id, json } => {
                                    if let Some(video) = &mut player.video {
                                        if video.stream_id.as_deref() == Some(stream_id.as_str()) {
                                            video.broadcaster.handle_signaling(viewer_id, &json);
                                        }
                                    }
                                }
                                Event::ChannelJoined { channel } => {
                                    // Ci siamo spostati (via !join o trascinati): ricorda il canale.
                                    saved.channel_id = Some(channel.id);
                                    saved.channel_name = Some(channel.name.clone());
                                    saved.save(&args.state);
                                }
                                Event::UserMoved { user, to_channel, .. } => {
                                    // Copre anche gli spostamenti fatti da altri (drag nel client TS).
                                    if Some(user.id) == client.client_id() {
                                        let name = client.channel(to_channel).map(|c| c.name);
                                        saved.channel_id = Some(to_channel);
                                        if let Some(n) = name {
                                            saved.channel_name = Some(n);
                                        }
                                        saved.save(&args.state);
                                    } else if args.welcome_delay_ms > 0
                                        && tokio::time::Instant::now() >= welcome_armed_at
                                        && pending_welcome.is_none()
                                        && joined_lonely_bot(&client, user.id, to_channel)
                                    {
                                        pending_welcome = Some(PendingWelcome {
                                            user_id: user.id,
                                            nickname: user.nickname.clone(),
                                            at: tokio::time::Instant::now() + welcome_delay,
                                        });
                                    }
                                }
                                Event::Disconnected { reason } => {
                                    warn!("Disconnesso: {reason}");
                                    return Ok(());
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(e) => warn!("process_events: {e}"),
                }

                // 1b) Brani risolti in background (titoli/playlist via yt-dlp)
                while let Ok(Resolved { tracks, label, replace_queue }) = meta_rx.try_recv() {
                    if tracks.is_empty() {
                        let _ = client.send_channel_message(
                            "Nessun risultato trovato.".to_string(),
                        );
                        continue;
                    }
                    let n = tracks.len();
                    // Titolo della playlist trovata con !ytplaylist, se presente.
                    let pl_name = label
                        .map(|l| format!("Playlist '{l}'"))
                        .unwrap_or_else(|| "Playlist".to_string());
                    // Una playlist (esplicita o URL con più brani) sostituisce la coda:
                    // il brano corrente finisce, poi parte la nuova playlist.
                    // Un brano singolo invece si accoda.
                    let replace = replace_queue || n > 1;
                    if player.is_busy() && replace {
                        let first = tracks[0].display().to_string();
                        player.replace_queue(tracks);
                        let _ = client.send_channel_message(format!(
                            "{pl_name}: coda sostituita con {n} brani, parte dopo il brano corrente ({}). Primo: {first} (!skip per passare subito)",
                            player.current_display()
                        ));
                    } else if player.is_busy() {
                        // C'è già qualcosa in riproduzione: accoda.
                        let first = tracks[0].display().to_string();
                        for t in tracks {
                            player.queue.push_back(t);
                        }
                        if n == 1 {
                            let _ = client.send_channel_message(format!(
                                "In coda (#{}) : {first}",
                                player.queue.len()
                            ));
                        } else {
                            let _ = client.send_channel_message(format!(
                                "{pl_name}: aggiunti {n} brani in coda (tot. {}). Primo: {first}",
                                player.queue.len(),
                                first = first
                            ));
                        }
                    } else {
                        // Libero: parte subito il primo, il resto in coda
                        // (una playlist rimpiazza gli eventuali avanzi in coda).
                        if replace {
                            player.queue.clear();
                        }
                        let mut it = tracks.into_iter();
                        let first = it.next().expect("non vuoto");
                        let rest: Vec<Track> = it.collect();
                        let rest_n = rest.len();
                        for t in rest {
                            player.queue.push_back(t);
                        }
                        let msg = player.start_track_now(first);
                        let _ = client.set_input_muted(false);
                        if rest_n > 0 {
                            let _ = client.send_channel_message(format!(
                                "{pl_name}: {rest_n_plus} brani in coda.\n{msg}",
                                rest_n_plus = rest_n + 1,
                                msg = msg
                            ));
                        } else {
                            let _ = client.send_channel_message(msg);
                        }
                    }
                }

                // 1c) Benvenuto scaduto: lo mandiamo solo se l'utente è ancora
                // nel nostro canale (non è uscito/passato oltre nel frattempo).
                if pending_welcome.as_ref().is_some_and(|w| tokio::time::Instant::now() >= w.at) {
                    let w = pending_welcome.take().expect("controllato sopra");
                    let still_here = client
                        .user(w.user_id)
                        .is_some_and(|u| Some(u.channel_id) == client.channel_id());
                    if still_here {
                        let msg = welcome_message(&w.nickname, &player, &playlists);
                        if let Err(e) = client.send_channel_message(msg) {
                            warn!("benvenuto fallito: {e}");
                        }
                    }
                }

                // 1d) Storico brani cambiato: persistilo.
                if player.recent_dirty {
                    player.recent_dirty = false;
                    saved.recent = player.recent.clone();
                    saved.save(&args.state);
                }

                // 1e) Video: offerte ai viewer, nome dello stream, pausa e fine brano.
                if let Some(video) = &mut player.video {
                    while let Some(ev) = video.broadcaster.poll_event() {
                        match ev {
                            BroadcastEvent::Offer { viewer_id, sdp } => {
                                if let Some(id) = &video.stream_id {
                                    if let Err(e) = client.accept_stream_viewer(viewer_id, id, &sdp) {
                                        warn!("accept_stream_viewer: {e}");
                                    }
                                }
                            }
                            BroadcastEvent::Connected { viewer_id } => info!("[video] in onda per {viewer_id}"),
                            BroadcastEvent::Gone { viewer_id, reason } => {
                                info!("[video] {viewer_id} scollegato: {reason}");
                            }
                        }
                    }
                    if let Some(id) = video.stream_id.clone() {
                        if let Some(name) = video.pending_name.take() {
                            if let Err(e) = client.rename_stream(&id, &name) {
                                warn!("rename_stream: {e}");
                            }
                        }
                    }
                    let height = player.video_config.encoder.height;
                    if matches!(player.source, Source::Stream { .. }) {
                        video.idle_since = None;
                        if !video.open {
                            // La live si era chiusa per inattività: riapriamola.
                            let name = video.pending_name.take().unwrap_or_else(|| "MusicBot".to_string());
                            let bitrate = player.video_config.encoder.bitrate_kbps * 1000;
                            match client.setup_stream(&StreamSetup::new(name, bitrate)) {
                                Ok(()) => {
                                    video.open = true;
                                    info!("Live video riaperta");
                                }
                                Err(e) => warn!("setup_stream: {e}"),
                            }
                        }
                        // Il video è finito prima dell'audio (o non è partito).
                        if video.broadcaster.video_finished() && !video.showing_idle {
                            video.show_idle(height, None);
                        }
                    } else if video.open {
                        // Niente brano (fermo, finito o sinusoide): animazione di
                        // attesa, poi chiusura se nessuno rimette musica.
                        let since = *video.idle_since.get_or_insert_with(Instant::now);
                        // (Ri)parte anche se l'animazione si è interrotta da sola.
                        // Se l'animazione non è partita (ffmpeg che rifiuta il
                        // grafo, per dire) si riprova, ma non a ogni tick da 20ms.
                        let retry_due = video
                            .idle_started
                            .is_none_or(|since| since.elapsed() >= Video::IDLE_RETRY);
                        if !video.showing_idle
                            || ((!video.broadcaster.has_video() || video.broadcaster.video_finished()) && retry_due)
                        {
                            video.show_idle(height, Some(Video::IDLE_TIMEOUT.saturating_sub(since.elapsed())));
                        }
                        if since.elapsed() >= Video::IDLE_TIMEOUT && player.queue.is_empty() {
                            info!("Nessun brano da {} minuti: chiudo la live video", Video::IDLE_TIMEOUT.as_secs() / 60);
                            if let Some(id) = video.stream_id.take() {
                                if let Err(e) = client.stop_stream(&id) {
                                    warn!("stop_stream: {e}");
                                }
                            }
                            video.broadcaster.stop_video();
                            video.broadcaster.clear_viewers();
                            video.open = false;
                            video.showing_idle = false;
                            video.idle_started = None;
                            video.idle_since = None;
                        }
                    }
                    video.broadcaster.set_paused(player.paused);
                }

                // 2) Streaming audio: un frame Opus ogni 20ms
                // Se in pausa: non leggiamo da ffmpeg (backpressure = freeze)
                // e non inviamo audio.
                // Nota: l'EQ è applicato qui sul PCM (prima di volume+Opus),
                // quindi il cambio preset è istantaneo senza riavviare lo stream.
                let vol = player.volume;
                let paused = player.paused;
                if paused {
                    continue;
                }
                match &mut player.source {
                    Source::Idle => {}
                    Source::Sine { freq, phase } => {
                        let freq = *freq;
                        let mut pcm = vec![0i16; frame_samples];
                        for s in pcm.iter_mut() {
                            let v = (*phase * 2.0 * std::f64::consts::PI).sin();
                            *s = (v * 8000.0) as i16;
                            *phase += freq / sample_rate;
                            if *phase >= 1.0 { *phase -= 1.0; }
                        }
                        player.eq.apply(&mut pcm);
                        apply_volume(&mut pcm, vol);
                        match encoder.encode(&pcm, &mut encode_buf) {
                            Ok(len) => {
                                if let Err(e) = client.send_audio(&encode_buf[..len], AudioCodec::OpusMusic) {
                                    warn!("send_audio: {e}");
                                }
                            }
                            Err(e) => warn!("encode: {e}"),
                        }
                    }
                    Source::Stream { stream } => {
                        // Leggi senza bloccare il tick: al massimo ~4KB per tick,
                        // con cap per fare backpressure su ffmpeg se siamo avanti.
                        let mut stream_ended = false;
                        if stream.pending.len() < PipeStream::MAX_PENDING {
                            let mut tmp = [0u8; 4096];
                            // Timeout corto: se non ci sono dati, saltiamo il tick
                            // (fase di avvio/buffering) invece di bloccare la chat.
                            match tokio::time::timeout(
                                tokio::time::Duration::from_millis(2),
                                stream.stdout.read(&mut tmp),
                            )
                            .await
                            {
                                Ok(Ok(0)) => stream_ended = true, // EOF: download finito
                                Ok(Ok(n)) => stream.pending.extend_from_slice(&tmp[..n]),
                                Ok(Err(e)) => {
                                    warn!("lettura ffmpeg: {e}");
                                    stream_ended = true;
                                }
                                Err(_) => {} // timeout: nessun dato pronto, riprova al prossimo tick
                            }
                        }
                        if stream_ended {
                            let finished = player.current_display();
                            let finished_track = player.current.clone();
                            kill_source(&mut player.source);
                            player.current = None;
                            // Loop "one": riparte lo stesso brano.
                            // Loop "all": il brano finito torna in fondo alla coda.
                            // Se nel frattempo la coda è stata sostituita da una nuova
                            // playlist, il brano vecchio non si ripete: si avanza e basta.
                            let loop_mode = if std::mem::take(&mut player.current_replaced) {
                                LoopMode::Off
                            } else {
                                player.loop_mode
                            };
                            match loop_mode {
                                LoopMode::One => {
                                    if let Some(cur) = finished_track {
                                        player.current = Some(cur);
                                        if let Some(msg) = player.restart_current() {
                                            let _ = client.send_channel_message(msg);
                                        } else if let Some(msg) = player.start_next() {
                                            let _ = client.send_channel_message(format!("Finito: {finished}\n{msg}"));
                                        }
                                    }
                                }
                                LoopMode::All => {
                                    if let Some(cur) = finished_track {
                                        player.queue.push_back(cur);
                                    }
                                    if let Some(msg) = player.start_next() {
                                        let _ = client.send_channel_message(format!("Finito: {finished}\n{msg}"));
                                    } else {
                                        let _ = client.send_channel_message(format!("Finito: {finished}"));
                                    }
                                }
                                LoopMode::Off => {
                                    // Auto-avanza con la coda, annunciando titolo + cover
                                    if let Some(msg) = player.start_next() {
                                        let _ = client.send_channel_message(format!("Finito: {finished}\n{msg}"));
                                    } else {
                                        let _ = client.send_channel_message(format!("Finito: {finished}"));
                                    }
                                }
                            }
                        } else if player.video.as_mut().is_some_and(Video::holds_audio) {
                            // Il video non è ancora partito: l'audio aspetta
                            // (ffmpeg intanto riempie il buffer, poi si ferma).
                        } else if stream.pending.len() >= PipeStream::FRAME_BYTES {
                            let raw: Vec<u8> = stream.pending.drain(..PipeStream::FRAME_BYTES).collect();
                            // Un frame emesso = 20ms di brano andati (in pausa
                            // non si arriva qui, quindi la posizione sta ferma).
                            player.position += Duration::from_millis(20);
                            let mut pcm = vec![0i16; frame_samples];
                            for (i, s) in pcm.iter_mut().enumerate() {
                                *s = i16::from_le_bytes([raw[2 * i], raw[2 * i + 1]]);
                            }
                            player.eq.apply(&mut pcm);
                            apply_volume(&mut pcm, vol);
                            match encoder.encode(&pcm, &mut encode_buf) {
                                Ok(len) => {
                                    if let Err(e) = client.send_audio(&encode_buf[..len], AudioCodec::OpusMusic) {
                                        warn!("send_audio: {e}");
                                    }
                                }
                                Err(e) => warn!("encode: {e}"),
                            }
                        }
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Chiusura...");
                break;
            }
            _ = sigterm.recv() => {
                info!("SIGTERM ricevuto, chiusura...");
                break;
            }
        }
    }

    kill_source(&mut player.source);
    player.disable_video(&mut client);
    // Ricorda volume + EQ + loop + canale corrente prima di uscire.
    saved.volume = player.volume;
    saved.eq_bass = player.eq.bass_db;
    saved.eq_mid = player.eq.mid_db;
    saved.eq_treble = player.eq.treble_db;
    saved.eq_preset = player.eq_preset.clone();
    saved.loop_mode = player.loop_mode.as_str().to_string();
    saved.recent = player.recent.clone();
    if saved.channel_id != client.channel_id() {
        saved.channel_id = client.channel_id();
        if let Some(id) = saved.channel_id {
            if let Some(ch) = client.channel(id) {
                saved.channel_name = Some(ch.name);
            }
        }
    }
    saved.save(&args.state);
    // Disconnessione graceful: lascia il tempo al pacchetto di partire,
    // altrimenti il server mostra "Timed Out" invece di "disconnect".
    let _ = client.disconnect();
    for _ in 0..10 {
        let _ = client.process_events().await;
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
    Ok(())
}

/// Gestisce un messaggio chat, ritorna eventuale risposta.
/// Lo streaming resta nel task principale: qui si fa solo spawn/kill (sincroni),
/// mai `tokio::spawn` con `&mut client` (Client non è Send).
fn handle_chat(
    client: &mut Client,
    prefix: &str,
    _sender_id: u16,
    sender_name: &str,
    message: &str,
    player: &mut Player,
    state_path: &str,
    saved: &mut BotState,
    playlists_path: &str,
    playlists: &mut SavedPlaylists,
    meta_tx: tokio::sync::mpsc::UnboundedSender<Resolved>,
) -> Option<String> {
    let msg = message.trim();
    if !msg.starts_with(prefix) {
        return None;
    }
    let body = msg[prefix.len()..].trim();
    let mut parts = body.split_whitespace();
    let cmd = parts.next().unwrap_or("").to_lowercase();
    let rest: Vec<&str> = parts.collect();

    /// Secondi di salto di `!avanti`/`!indietro`: l'argomento se c'è e ha
    /// senso, altrimenti il default. `None` = argomento non numerico.
    fn seek_arg(rest: &[&str], default: i64) -> Option<i64> {
        match rest.first() {
            None => Some(default),
            Some(raw) => raw.trim().parse::<i64>().ok().filter(|s| *s > 0),
        }
    }

    /// Applica un preset EQ e lo persiste. Ritorna il messaggio di conferma.
    fn apply_eq_preset(player: &mut Player, saved: &mut BotState, state_path: &str, name: &str) -> String {
        match eq_preset_gains(name) {
            Some((b, m, t)) => {
                player.eq.set_gains(b, m, t);
                let canonical = if name.trim().to_lowercase() == "neutro" {
                    "flat".to_string()
                } else {
                    name.trim().to_lowercase()
                };
                player.eq_preset = canonical;
                saved.eq_bass = player.eq.bass_db;
                saved.eq_mid = player.eq.mid_db;
                saved.eq_treble = player.eq.treble_db;
                saved.eq_preset = player.eq_preset.clone();
                saved.save(state_path);
                format!("{}.", player.eq_summary())
            }
            None => format!(
                "Preset '{name}' sconosciuto. Disponibili: {}",
                eq_preset_list()
            ),
        }
    }

    match cmd.as_str() {
        "help" => Some(
            "Comandi: !play <url|ricerca|hz> !stop !skip !ff [s] !rw [s] !seek <m:ss> !queue !now !volume !pause !resume !eq <preset> !bass/!mid/!treble !loop !lofi !ytplaylist <ricerca> !playlist <nome> !playlists !recent !video [on|off] !join !say !users !channels".to_string(),
        ),
        "users" => {
            let mut users = client.users();
            users.sort_by_key(|u| u.id);
            let mut out = format!("Utenti online ({}):\n", users.len());
            for u in users.iter().take(20) {
                out.push_str(&format!("- {} (ch {}){}\n", u.nickname, u.channel_id, ""));
            }
            let _ = sender_name;
            Some(out)
        }
        "channels" => {
            let mut chs = client.channels();
            chs.sort_by_key(|c| c.id);
            let mut out = format!("Canali ({}):\n", chs.len());
            for c in chs.iter().take(30) {
                out.push_str(&format!("- {} [{}]\n", c.name, c.id));
            }
            Some(out)
        }
        "join" => {
            if rest.is_empty() {
                return Some("Uso: !join <id|nome>".to_string());
            }
            let target = rest.join(" ");
            // Prova come ID numerico
            let cid: Option<u64> = target.parse().ok().or_else(|| {
                client
                    .channels()
                    .iter()
                    .find(|c| c.name.eq_ignore_ascii_case(&target))
                    .map(|c| c.id)
            });
            match cid {
                Some(id) => match client.move_to_channel(id) {
                    // La risposta viene instradata dal chiamante (DM o canale)
                    Ok(()) => Some(format!("Mi sposto nel canale {id}...")),
                    Err(e) => Some(format!("Spostamento fallito: {e}")),
                },
                None => Some(format!("Canale '{target}' non trovato")),
            }
        }
        "say" => {
            let text = rest.join(" ");
            if text.is_empty() {
                return Some("Uso: !say <messaggio>".to_string());
            }
            match client.send_channel_message(format!("[b]{sender_name} dice:[/b] {text}")) {
                Ok(()) => None, // già inviato in canale, nessuna risposta privata
                Err(e) => Some(format!("Invio fallito: {e}")),
            }
        }
        "play" => {
            if rest.is_empty() {
                return Some(
                    "Uso: !play <url YouTube | titolo da cercare> oppure !play [hz 50-2000]"
                        .to_string(),
                );
            }
            let arg = rest[0];
            // Frequenza numerica = demo sinusoide (retrocompatibile)
            if let Ok(f) = arg.parse::<f64>() {
                if !(50.0..=2000.0).contains(&f) {
                    return Some("Frequenza fuori range (50-2000 Hz)".to_string());
                }
                // Se c'è già uno stream, la sinusoide lo sostituisce (e svuota la coda? no: resta).
                kill_source(&mut player.source);
                player.source = Source::Sine { freq: f, phase: 0.0 };
                player.current = Some(Track::new(format!("sine {f:.0}Hz"), Some(format!("sine {f:.0}Hz")), None, None));
                player.paused = false;
                player.eq.reset();
                let _ = client.set_input_muted(false);
                return Some(format!("Riproduzione nota a {f:.0} Hz (OpusMusic). !stop per fermare."));
            }
            // Altrimenti URL, oppure testo libero = ricerca su YouTube.
            // Risolviamo in background per non bloccare il tick audio da 20ms:
            // l'annuncio col titolo arriva in canale.
            let query = rest.join(" ");
            let (spec, loading) = if is_url(arg) {
                (arg.to_string(), "Caricamento... (risolvo titolo/playlist)".to_string())
            } else {
                (
                    youtube_search_spec(&query),
                    format!("Cerco su YouTube: {query}..."),
                )
            };
            let _ = client.set_input_muted(false);
            let tx = meta_tx.clone();
            tokio::spawn(async move {
                let tracks = resolve_tracks(spec).await;
                let _ = tx.send(Resolved::plain(tracks));
            });
            Some(loading)
        }
        "recent" | "history" | "ultimi" | "storico" => {
            if player.recent.is_empty() {
                return Some("Nessun brano ascoltato di recente.".to_string());
            }
            let mut out = format!("Ultimi brani ({}):\n", player.recent.len());
            for (i, t) in player.recent.iter().enumerate() {
                out.push_str(&format!("{}. [url={}]{}[/url]\n", i + 1, t.url, t.display()));
            }
            Some(out)
        }
        "video" => {
            let reply = match rest.first().map(|a| a.to_lowercase()).as_deref() {
                Some("on" | "si" | "sì") => match player.enable_video(client) {
                    Ok(msg) => {
                        saved.video = true;
                        saved.save(state_path);
                        msg
                    }
                    Err(e) => format!("Video non attivato: {e:#}"),
                },
                Some("off" | "no") => {
                    saved.video = false;
                    saved.save(state_path);
                    player.disable_video(client)
                }
                Some(_) => "Uso: !video [on|off]".to_string(),
                None => match &player.video {
                    Some(video) => format!(
                        "Video attivo, {} viewer. !video off per spegnerlo.",
                        video.broadcaster.viewer_count()
                    ),
                    None => "Video spento. !video on per condividere il video dei brani (client TeamSpeak 6).".to_string(),
                },
            };
            Some(reply)
        }
        "stop" => {
            player.stop_all();
            Some("Riproduzione fermata e coda svuotata.".to_string())
        }
        "skip" => {
            kill_source(&mut player.source);
            player.current = None;
            player.paused = false;
            if let Some(msg) = player.start_next() {
                Some(format!("Skip. {msg}"))
            } else {
                Some("Skip. Niente altro in coda.".to_string())
            }
        }
        "ff" | "fw" | "avanti" | "forward" => match seek_arg(&rest, 10) {
            Some(secs) => Some(player.seek_by(secs)),
            None => Some("Uso: !ff [secondi] (default 10), es. !ff 30".to_string()),
        },
        "rw" | "indietro" | "back" | "rewind" => match seek_arg(&rest, 10) {
            Some(secs) => Some(player.seek_by(-secs)),
            None => Some("Uso: !rw [secondi] (default 10), es. !rw 30".to_string()),
        },
        "seek" | "vai" => {
            if rest.is_empty() {
                return Some(format!(
                    "Posizione: {} (uso: !seek <m:ss|secondi>)",
                    format_pos(player.position)
                ));
            }
            match parse_pos(rest[0]) {
                Some(target) => Some(format!("Vado a {}", player.seek_to(target))),
                None => Some("Uso: !seek <m:ss|secondi>, es. !seek 1:30".to_string()),
            }
        }
        "pause" | "pausa" => {
            if !player.is_busy() {
                return Some("Niente in riproduzione.".to_string());
            }
            if player.paused {
                return Some("Già in pausa.".to_string());
            }
            player.paused = true;
            Some(format!("Pausa: {}", player.current_display()))
        }
        "resume" | "unpause" | "continue" | "riprendi" => {
            if !player.is_busy() {
                return Some("Niente in riproduzione.".to_string());
            }
            if !player.paused {
                return Some("Già in riproduzione.".to_string());
            }
            player.paused = false;
            let _ = client.set_input_muted(false);
            Some(format!("Ripresa: {}", player.current_display()))
        }
        "queue" => {
            if player.queue.is_empty() {
                return Some("Coda vuota.".to_string());
            }
            let mut out = format!("Coda ({}):\n", player.queue.len());
            for (i, t) in player.queue.iter().take(10).enumerate() {
                out.push_str(&format!("{}. {}\n", i + 1, t.display()));
            }
            Some(out)
        }
        "now" | "nowplaying" | "np" => {
            let state = if player.paused { " (in pausa)" } else { "" };
            let pos = match &player.current {
                Some(c) => match c.duration_secs {
                    Some(total) => format!(
                        "{} / {} {}",
                        format_pos(player.position),
                        format_pos(Duration::from_secs(total)),
                        text_progress_bar(player.position.as_secs(), total)
                    ),
                    None => format_pos(player.position),
                },
                None => format_pos(player.position),
            };
            let extra = format!(
                "(a {pos}, vol {:.0}%, loop {}, {})",
                player.volume * 100.0,
                player.loop_mode.as_str(),
                player.eq_summary()
            );
            match &player.current {
                Some(c) => Some(format!("{} {extra}", c.announce(&format!("In riproduzione{state}")))),
                None => Some(format!("Niente in riproduzione. {extra}")),
            }
        }
        "volume" | "vol" => {
            if rest.is_empty() {
                return Some(format!("Volume: {:.0}% (uso: !volume 0-200)", player.volume * 100.0));
            }
            let raw = rest[0].trim().trim_end_matches('%');
            // Forme accettate: "80" / "80%" (=80%), "+10"/"-10" (relativo).
            let new_vol: Option<f32> = if let Some(delta) = raw.strip_prefix('+').and_then(|s| s.parse::<f32>().ok()) {
                Some(player.volume + delta / 100.0)
            } else if let Some(delta) = raw.strip_prefix('-').and_then(|s| s.parse::<f32>().ok()) {
                Some(player.volume - delta / 100.0)
            } else if let Ok(pct) = raw.replace(',', ".").parse::<f32>() {
                Some(pct / 100.0)
            } else {
                None
            };
            match new_vol {
                Some(v) => {
                    player.volume = v.clamp(0.0, 2.0);
                    saved.volume = player.volume;
                    saved.save(state_path);
                    Some(format!("Volume: {:.0}%", player.volume * 100.0))
                }
                None => Some("Uso: !volume <0-200> (es. !volume 80, !volume +10)".to_string()),
            }
        }
        "eq" | "equalizer" | "equalizzatore" => {
            if rest.is_empty() {
                return Some(format!(
                    "{}. Uso: !eq <preset|show|list|off>. Preset: {}",
                    player.eq_summary(),
                    eq_preset_list()
                ));
            }
            let arg = rest[0].to_lowercase();
            match arg.as_str() {
                "show" | "status" | "stato" => Some(format!("{}.", player.eq_summary())),
                "list" | "lista" | "presets" => {
                    Some(format!("Preset disponibili: {}", eq_preset_list()))
                }
                _ => Some(apply_eq_preset(player, saved, state_path, rest[0])),
            }
        }
        "bass" | "bassi" | "low" => {
            if rest.is_empty() {
                return Some(format!(
                    "Bassi: {:+.0}dB (-12..+12). Uso: !bass <dB> (es. !bass +4)",
                    player.eq.bass_db
                ));
            }
            match rest[0].replace(',', ".").parse::<f32>() {
                Ok(db) => {
                    let db = db.clamp(-12.0, 12.0);
                    player.eq.set_gains(db, player.eq.mid_db, player.eq.treble_db);
                    player.eq_preset = "custom".to_string();
                    saved.eq_bass = player.eq.bass_db;
                    saved.eq_mid = player.eq.mid_db;
                    saved.eq_treble = player.eq.treble_db;
                    saved.eq_preset = "custom".to_string();
                    saved.save(state_path);
                    Some(format!("{}.", player.eq_summary()))
                }
                Err(_) => Some("Uso: !bass <-12..+12> (es. !bass +4)".to_string()),
            }
        }
        "mid" | "medi" | "medie" => {
            if rest.is_empty() {
                return Some(format!(
                    "Medi: {:+.0}dB (-12..+12). Uso: !mid <dB> (es. !mid -2)",
                    player.eq.mid_db
                ));
            }
            match rest[0].replace(',', ".").parse::<f32>() {
                Ok(db) => {
                    let db = db.clamp(-12.0, 12.0);
                    player.eq.set_gains(player.eq.bass_db, db, player.eq.treble_db);
                    player.eq_preset = "custom".to_string();
                    saved.eq_bass = player.eq.bass_db;
                    saved.eq_mid = player.eq.mid_db;
                    saved.eq_treble = player.eq.treble_db;
                    saved.eq_preset = "custom".to_string();
                    saved.save(state_path);
                    Some(format!("{}.", player.eq_summary()))
                }
                Err(_) => Some("Uso: !mid <-12..+12> (es. !mid -2)".to_string()),
            }
        }
        "treble" | "alti" | "high" | "alto" => {
            if rest.is_empty() {
                return Some(format!(
                    "Alti: {:+.0}dB (-12..+12). Uso: !treble <dB> (es. !treble +3)",
                    player.eq.treble_db
                ));
            }
            match rest[0].replace(',', ".").parse::<f32>() {
                Ok(db) => {
                    let db = db.clamp(-12.0, 12.0);
                    player.eq.set_gains(player.eq.bass_db, player.eq.mid_db, db);
                    player.eq_preset = "custom".to_string();
                    saved.eq_bass = player.eq.bass_db;
                    saved.eq_mid = player.eq.mid_db;
                    saved.eq_treble = player.eq.treble_db;
                    saved.eq_preset = "custom".to_string();
                    saved.save(state_path);
                    Some(format!("{}.", player.eq_summary()))
                }
                Err(_) => Some("Uso: !treble <-12..+12> (es. !treble +3)".to_string()),
            }
        }
        "loop" | "repeat" | "ripeti" | "replay-mode" => {
            if rest.is_empty() {
                // Senza argomento: cicla off -> all -> one -> off.
                player.loop_mode = match player.loop_mode {
                    LoopMode::Off => LoopMode::All,
                    LoopMode::All => LoopMode::One,
                    LoopMode::One => LoopMode::Off,
                };
            } else if let Some(m) = LoopMode::parse(rest[0]) {
                player.loop_mode = m;
            } else {
                return Some("Uso: !loop [off|all|one] (off=spento, all=tutta la coda, one=brano corrente)".to_string());
            }
            saved.loop_mode = player.loop_mode.as_str().to_string();
            saved.save(state_path);
            let desc = match player.loop_mode {
                LoopMode::Off => "Loop disattivato.",
                LoopMode::All => "Loop attivo: tutta la coda (alla fine il brano torna in fondo).",
                LoopMode::One => "Loop attivo: ripeto il brano corrente.",
            };
            Some(desc.to_string())
        }
        "lofi" | "radio" | "chill" => {
            // Niente URL fisso (le live muoiono/girano ID): cerca 5 mix lofi
            // e li accoda come una mini-playlist. Con !loop all girano a ripetizione.
            let _ = client.set_input_muted(false);
            let tx = meta_tx.clone();
            tokio::spawn(async move {
                let tracks = resolve_tracks(LOFI_SEARCH.to_string()).await;
                let _ = tx.send(Resolved::plain(tracks));
            });
            Some("Caricamento lofi... (5 mix in arrivo, poi !loop all per ripeterli)".to_string())
        }
        "ytplaylist" | "ytpl" | "searchplaylist" | "cercaplaylist" => {
            if rest.is_empty() {
                return Some("Uso: !ytplaylist <titolo playlist da cercare su YouTube>".to_string());
            }
            // Cerca la prima playlist YouTube per titolo e la carica come
            // un !play <url playlist>: il primo brano parte, il resto va in coda.
            let query = rest.join(" ");
            let _ = client.set_input_muted(false);
            let tx = meta_tx.clone();
            let q = query.clone();
            tokio::spawn(async move {
                let resolved = match find_youtube_playlist(&q).await {
                    Some((title, url)) => {
                        Resolved::playlist(resolve_tracks(url).await, Some(title))
                    }
                    None => Resolved::plain(Vec::new()),
                };
                let _ = tx.send(resolved);
            });
            Some(format!("Cerco playlist su YouTube: {query}..."))
        }
        "playlists" | "playlist-list" => {
            if playlists.is_empty() {
                Some("Nessuna playlist salvata. Uso: !playlistsave <nome> per salvare coda+corrente.".to_string())
            } else {
                let mut names: Vec<&String> = playlists.keys().collect();
                names.sort();
                let mut out = format!("Playlist salvate ({}):\n", names.len());
                for n in names {
                    let count = playlists.get(n).map(|v| v.len()).unwrap_or(0);
                    out.push_str(&format!("- {n} ({count} voci)\n"));
                }
                out.push_str("Uso: !playlist <nome>");
                Some(out)
            }
        }
        "playlist" => {
            if rest.is_empty() {
                return Some("Uso: !playlist <nome> (vedi !playlists)".to_string());
            }
            let name = normalize_playlist_name(rest[0]);
            if name.is_empty() {
                return Some("Nome playlist non valido (usa lettere/numeri/-/_.".to_string());
            }
            let urls = match playlists.get(&name) {
                Some(u) if !u.is_empty() => u.clone(),
                _ => return Some(format!("Playlist '{name}' non trovata o vuota. Vedi !playlists.")),
            };
            let n = urls.len();
            let pl_label = name.clone();
            let _ = client.set_input_muted(false);
            let tx = meta_tx.clone();
            tokio::spawn(async move {
                let tracks = resolve_url_list(urls).await;
                let _ = tx.send(Resolved::playlist(tracks, Some(pl_label)));
            });
            Some(format!("Caricamento playlist '{name}' ({n} voci)..."))
        }
        "playlistsave" | "playlist-save" | "saveplaylist" => {
            if rest.is_empty() {
                return Some(
                    "Uso: !playlistsave <nome> [url] (con url salva quel link/playlist YouTube, senza salva brano corrente + coda)"
                        .to_string(),
                );
            }
            let name = normalize_playlist_name(rest[0]);
            if name.is_empty() {
                return Some("Nome playlist non valido (usa lettere/numeri/-/_.".to_string());
            }
            // Con URL espliciti si salvano i link così come sono: una playlist
            // YouTube viene espansa da yt-dlp a ogni !playlist, quindi resta
            // allineata ai brani aggiunti/rimossi su YouTube.
            let explicit: Vec<String> = rest[1..]
                .iter()
                .filter(|u| is_url(u))
                .map(|u| u.to_string())
                .collect();
            if rest.len() > 1 && explicit.len() != rest.len() - 1 {
                return Some("Dopo il nome sono ammessi solo URL (http/https).".to_string());
            }
            if !explicit.is_empty() {
                let count = explicit.len();
                playlists.insert(name.clone(), explicit);
                save_playlists(playlists_path, playlists);
                return Some(format!(
                    "Playlist '{name}' salvata ({count} link). Caricala con !playlist {name}."
                ));
            }
            let mut urls: Vec<String> = Vec::new();
            if let Some(cur) = &player.current {
                if is_url(&cur.url) {
                    urls.push(cur.url.clone());
                }
            }
            for t in &player.queue {
                if is_url(&t.url) {
                    urls.push(t.url.clone());
                }
            }
            if urls.is_empty() {
                return Some("Niente da salvare: né corrente né coda contengono URL.".to_string());
            }
            urls.dedup();
            let count = urls.len();
            playlists.insert(name.clone(), urls);
            save_playlists(playlists_path, playlists);
            Some(format!("Playlist '{name}' salvata ({count} voci). Caricala con !playlist {name}."))
        }
        "playlistdel" | "playlist-del" | "delplaylist" | "playlistrm" => {
            if rest.is_empty() {
                return Some("Uso: !playlistdel <nome>".to_string());
            }
            let name = normalize_playlist_name(rest[0]);
            if playlists.remove(&name).is_some() {
                save_playlists(playlists_path, playlists);
                Some(format!("Playlist '{name}' eliminata."))
            } else {
                Some(format!("Playlist '{name}' non trovata."))
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Il grafo dell'animazione passa per tre livelli di escaping (grafo,
    /// opzioni, espansione del testo): l'unica verifica affidabile è farlo
    /// renderizzare a ffmpeg. Saltato se ffmpeg non c'è.
    #[test]
    fn idle_animation_renders() {
        if std::process::Command::new("ffmpeg").arg("-version").output().is_err() {
            return;
        }
        for (height, countdown) in [(480, Some(Duration::from_secs(300))), (720, None)] {
            let graph = idle_animation(height, countdown);
            let out = std::process::Command::new("ffmpeg")
                .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", &graph])
                .args(["-frames:v", "3", "-f", "null", "-"])
                .output()
                .unwrap();
            assert!(out.status.success(), "{graph}\n{}", String::from_utf8_lossy(&out.stderr));
        }
    }

    #[test]
    fn pos_roundtrip() {
        assert_eq!(format_pos(Duration::from_secs(0)), "0:00");
        assert_eq!(format_pos(Duration::from_secs(95)), "1:35");
        assert_eq!(format_pos(Duration::from_secs(3723)), "1:02:03");
        assert_eq!(parse_pos("90"), Some(Duration::from_secs(90)));
        assert_eq!(parse_pos("1:30"), Some(Duration::from_secs(90)));
        assert_eq!(parse_pos("1:02:03"), Some(Duration::from_secs(3723)));
        assert_eq!(parse_pos("un minuto"), None);
        assert_eq!(parse_pos("1:2:3:4"), None);
    }

    /// Senza seek si resta sulla pipe di yt-dlp; col seek ffmpeg apre la URL
    /// diretta e `-ss` sta prima di `-i` (input seek, non decodifica inutile).
    #[test]
    fn video_command_seeks_on_direct_url() {
        let plain = ytdlp_video_command("https://esempio/x", 720, Duration::ZERO);
        assert_eq!(plain.get_program(), "yt-dlp");
        let seeked = ytdlp_video_command("https://esempio/x", 720, Duration::from_secs(42));
        assert_eq!(seeked.get_program(), "sh");
        let line: String = seeked
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(line.contains("-ss 42.000"), "{line}");
        assert!(line.contains("yt-dlp") && line.contains("-g"), "{line}");
        assert!(line.find("-ss").unwrap() < line.find("-i ").unwrap(), "{line}");
    }

    /// Le URL arrivano dalla chat e finiscono in `sh -c`: un apice non deve
    /// poter chiudere la stringa e far eseguire altro.
    #[test]
    fn shell_quote_neutralizes_quotes() {
        assert_eq!(shell_quote("https://x/y"), "'https://x/y'");
        // Il controllo vero è che `sh` la tratti come un argomento solo,
        // stampandola identica e senza eseguire il comando iniettato.
        let evil = shell_quote("https://x/'; touch /tmp/pwned; echo '");
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf %s {evil}"))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "https://x/'; touch /tmp/pwned; echo '");
        assert!(!std::path::Path::new("/tmp/pwned").exists());
    }

    #[test]
    fn eq_presets_all_known() {
        for name in [
            "flat", "off", "bass", "treble", "pop", "rock", "jazz", "vocal", "lofi",
            "soft", "dance",
        ] {
            assert!(eq_preset_gains(name).is_some(), "preset mancante: {name}");
        }
        assert!(eq_preset_gains("inesistente").is_none());
    }

    #[test]
    fn eq_flat_is_noop() {
        let mut eq = Eq::new(0.0, 0.0, 0.0);
        let mut pcm = vec![1000i16, -2000, 3000, -4000];
        let before = pcm.clone();
        eq.apply(&mut pcm);
        assert_eq!(pcm, before);
    }

    #[test]
    fn eq_bass_boosts_low_sine() {
        // Seno a 100Hz: con +10dB sui bassi l'RMS deve crescere nettamente.
        let make_sine = || {
            (0..4800)
                .map(|i| {
                    (i as f32 * 2.0 * std::f32::consts::PI * 100.0 / 48000.0).sin() * 10000.0
                        as f32
                })
                .map(|v| v as i16)
                .collect::<Vec<_>>()
        };
        let rms = |pcm: &[i16]| {
            (pcm.iter().map(|s| (*s as f32).powi(2)).sum::<f32>() / pcm.len() as f32).sqrt()
        };
        let dry = make_sine();
        let dry_rms = rms(&dry);
        let mut wet = dry.clone();
        Eq::new(10.0, 0.0, 0.0).apply(&mut wet);
        assert!(rms(&wet) > dry_rms * 1.5, "boost bassi troppo debole");
    }

    #[test]
    fn eq_gains_clamped() {
        let eq = Eq::new(99.0, -99.0, 0.5);
        assert_eq!(eq.bass_db, 12.0);
        assert_eq!(eq.mid_db, -12.0);
    }

    #[test]
    fn track_announce_embeds_cover_preview() {
        let t = Track::new(
            "https://youtu.be/x".to_string(),
            Some("Titolo".to_string()),
            Some("https://i.ytimg.com/vi/x/hqdefault.jpg".to_string()),
            Some(213),
        );
        let msg = t.announce("Riproduco");
        assert!(msg.contains("[img]https://i.ytimg.com/vi/x/hqdefault.jpg[/img]"));
        assert!(msg.contains("[b]"));
        assert_eq!(t.duration_secs, Some(213));
        let plain = Track::new("https://youtu.be/x".to_string(), None, None, None);
        assert!(!plain.announce("Riproduco").contains("[img]"));
        assert!(plain.display() == "https://youtu.be/x");
    }

    #[test]
    fn recent_dedups_and_caps() {
        let mut p = Player::new(1.0, Eq::new(0.0, 0.0, 0.0), "flat".into(), LoopMode::Off, Vec::new(), BroadcastConfig::default());
        for i in 0..15 {
            p.remember(&Track::new(format!("https://youtu.be/{i}"), None, None, None));
        }
        assert_eq!(p.recent.len(), Player::RECENT_MAX);
        assert_eq!(p.recent[0].url, "https://youtu.be/14");
        // Riascoltare un brano lo riporta in testa senza duplicarlo.
        p.remember(&Track::new("https://youtu.be/10".into(), None, None, None));
        assert_eq!(p.recent[0].url, "https://youtu.be/10");
        assert_eq!(p.recent.iter().filter(|t| t.url == "https://youtu.be/10").count(), 1);
    }

    #[test]
    fn replace_queue_drops_old_tracks_and_skips_loop() {
        let mut p = Player::new(1.0, Eq::new(0.0, 0.0, 0.0), "flat".into(), LoopMode::All, Vec::new(), BroadcastConfig::default());
        p.current = Some(Track::new("https://youtu.be/skillet0".into(), None, None, None));
        p.queue.push_back(Track::new("https://youtu.be/skillet1".into(), None, None, None));
        p.queue.push_back(Track::new("https://youtu.be/skillet2".into(), None, None, None));
        p.replace_queue(vec![
            Track::new("https://youtu.be/lp1".into(), None, None, None),
            Track::new("https://youtu.be/lp2".into(), None, None, None),
        ]);
        let urls: Vec<&str> = p.queue.iter().map(|t| t.url.as_str()).collect();
        assert_eq!(urls, ["https://youtu.be/lp1", "https://youtu.be/lp2"]);
        // A fine brano il loop non deve rimettere in coda il brano vecchio.
        assert!(p.current_replaced);
        p.stop_all();
        assert!(!p.current_replaced);
    }

    #[test]
    fn welcome_lists_playlists_and_recent() {
        let mut p = Player::new(1.0, Eq::new(0.0, 0.0, 0.0), "flat".into(), LoopMode::Off, Vec::new(), BroadcastConfig::default());
        p.remember(&Track::new("https://youtu.be/x".into(), Some("Brano X".into()), None, None));
        let mut pls = SavedPlaylists::new();
        pls.insert("lofi".into(), vec!["https://a".into(), "https://b".into()]);
        let msg = welcome_message("Mario", &p, &pls);
        assert!(msg.contains("Ciao Mario"));
        assert!(msg.contains("lofi (2)"));
        assert!(msg.contains("Brano X"));
    }

    #[test]
    fn loop_mode_parse() {
        assert_eq!(LoopMode::parse("off"), Some(LoopMode::Off));
        assert_eq!(LoopMode::parse("ONE"), Some(LoopMode::One));
        assert_eq!(LoopMode::parse("all"), Some(LoopMode::All));
        assert_eq!(LoopMode::parse("ciao"), None);
    }

    #[test]
    fn playlist_name_normalized() {
        assert_eq!(normalize_playlist_name(" LoFi-2024! "), "lofi-2024");
    }

    #[test]
    fn youtube_thumbnail_fallback_from_watch_url() {
        let thumb = youtube_thumbnail_fallback("https://www.youtube.com/watch?v=n61ULEU7CO0");
        assert_eq!(
            thumb.as_deref(),
            Some("https://i.ytimg.com/vi/n61ULEU7CO0/hqdefault.jpg")
        );
        // Con parametri extra dopo l'ID.
        let thumb = youtube_thumbnail_fallback("https://www.youtube.com/watch?v=n61ULEU7CO0&list=PLx");
        assert!(thumb.is_some());
        // Formati corti.
        assert!(youtube_thumbnail_fallback("https://youtu.be/n61ULEU7CO0").is_some());
        assert!(youtube_thumbnail_fallback("https://www.youtube.com/shorts/n61ULEU7CO0").is_some());
        // Non-YouTube: niente fallback.
        assert!(youtube_thumbnail_fallback("https://example.com/audio.mp3").is_none());
    }

    #[test]
    fn playlist_search_url_encodes_query() {
        assert_eq!(
            youtube_playlist_search_url(" lofi hip hop "),
            "https://www.youtube.com/results?search_query=lofi+hip+hop&sp=EgIQAw%3D%3D"
        );
        // Caratteri speciali/accentati percent-encoded (UTF-8), niente '&' grezzi.
        let url = youtube_playlist_search_url("città & mare");
        assert!(url.contains("search_query=citt%C3%A0+%26+mare&"));
    }

    #[test]
    fn track_without_thumbnail_still_announces() {
        let t = Track::new(
            "https://example.com/x.mp3".to_string(),
            Some("Brano".to_string()),
            Some("NA".to_string()),
            None,
        );
        assert!(t.thumbnail.is_none());
        assert!(t.announce("Riproduco").contains("Brano"));
    }

    #[test]
    fn duration_parses_yt_dlp_format() {
        assert_eq!(parse_duration("213"), Some(213));
        assert_eq!(parse_duration("213.7"), Some(213));
        assert_eq!(parse_duration("NA"), None);
        assert_eq!(parse_duration("none"), None);
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("0"), None);
        assert_eq!(parse_duration("-5"), None);
        assert_eq!(parse_duration("abc"), None);
    }

    #[test]
    fn track_drops_zero_duration() {
        let t = Track::new("https://youtu.be/x".to_string(), None, None, Some(0));
        assert_eq!(t.duration_secs, None);
    }

    #[test]
    fn text_progress_bar_shows_elapsed_and_left() {
        let bar = text_progress_bar(0, 200);
        assert!(bar.contains("[░░░░░░░░░░░░] 0%"), "{bar}");
        let bar = text_progress_bar(100, 200);
        assert!(bar.contains("[██████░░░░░░] 50%"), "{bar}");
        let bar = text_progress_bar(200, 200);
        assert!(bar.contains("[████████████] 100%"), "{bar}");
        // Oltre la fine: clamp a pieno, niente panico.
        let bar = text_progress_bar(999, 200);
        assert!(bar.contains("100%"), "{bar}");
        assert_eq!(text_progress_bar(10, 0), "");
    }

    /// La barra in basso nel video deve passare per gli stessi tre livelli di
    /// escaping del grafo idle: l'unica verifica affidabile è farla
    /// renderizzare a ffmpeg. Saltato se ffmpeg non c'è.
    #[test]
    fn video_progress_bar_renders() {
        if std::process::Command::new("ffmpeg").arg("-version").output().is_err() {
            return;
        }
        for (total, off) in [(213.0, 0.0), (3723.0, 42.5)] {
            let progress = VideoProgress::new(total, off).expect("progress valida");
            let vf = format!(
                "testsrc2=size=640x480:rate=30{}",
                VideoInput::progress_filter(Some(progress), 480)
            );
            let out = std::process::Command::new("ffmpeg")
                .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", &vf])
                .args(["-frames:v", "3", "-f", "null", "-"])
                .output()
                .unwrap();
            assert!(out.status.success(), "{vf}\n{}", String::from_utf8_lossy(&out.stderr));
        }
        // Senza durata niente filtro: video liscio come prima.
        assert_eq!(VideoInput::progress_filter(None, 480), "");
        assert!(VideoProgress::new(0.0, 0.0).is_none());
        assert!(VideoProgress::new(f64::NAN, 0.0).is_none());
    }

    /// La barra deve partire quasi vuota, riempirsi col tempo e stare
    /// davvero in basso (drawbox non rivaluta la geometria per-frame:
    /// una larghezza animata con `t` resterebbe congelata, qui i segmenti
    /// si accendono via `enable`). Su sfondo nero ogni pixel rosso in
    /// basso può venire solo dalla barra.
    #[test]
    fn video_progress_bar_grows_along_the_bottom() {
        if std::process::Command::new("ffmpeg").arg("-version").output().is_err() {
            return;
        }
        let total = 2.0;
        let height = 240u32;
        let bar_h = 8usize; // (240/48).clamp(8,18), deve restare in sync col filtro
        let (w, h) = (320usize, 240usize);
        let progress = VideoProgress::new(total, 0.0).expect("progress valida");
        let vf = format!(
            "color=black:size={w}x{h}:rate=30{}",
            VideoInput::progress_filter(Some(progress), height)
        );
        let out = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", &vf])
            .args(["-frames:v", "60", "-f", "rawvideo", "-pix_fmt", "rgb24", "pipe:1"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{vf}\n{}", String::from_utf8_lossy(&out.stderr));
        let frame_bytes = w * h * 3;
        assert_eq!(out.stdout.len(), 60 * frame_bytes, "frame inattesi");

        // Frazione di rosso nella banda in basso e (controllo) sopra di essa.
        let red_frac = |frame: &[u8], rows: std::ops::Range<usize>| -> f64 {
            let mut red = 0usize;
            let mut tot = 0usize;
            for y in rows {
                for x in 0..w {
                    let o = (y * w + x) * 3;
                    tot += 1;
                    if frame[o] > 150 && frame[o + 1] < 100 && frame[o + 2] < 100 {
                        red += 1;
                    }
                }
            }
            red as f64 / tot as f64
        };
        let frame = |n: usize| &out.stdout[n * frame_bytes..(n + 1) * frame_bytes];
        let bottom = (h - bar_h)..h;
        let above = 0..(h - bar_h);

        let early = red_frac(frame(2), bottom.clone());
        let late = red_frac(frame(57), bottom.clone());
        assert!(early < 0.10, "a inizio video la barra deve essere quasi vuota, rosso={early:.3}");
        assert!(late > 0.80, "a fine video la barra deve essere quasi piena, rosso={late:.3}");
        assert!(late > early + 0.5, "la barra deve crescere nel tempo ({early:.3} -> {late:.3})");
        // Niente rosso sopra la banda: la barra sta in basso, non in mezzo.
        assert!(
            red_frac(frame(57), above) < 0.01,
            "rosso fuori dalla banda in basso"
        );
    }
}

