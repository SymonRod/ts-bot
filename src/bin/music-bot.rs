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
//!   !loop [off|one|all], !lofi, !playlist <nome>, !playlists,
//!   !playlistsave <nome>, !playlistdel <nome>

use std::collections::VecDeque;
use std::process::Stdio;

use anyhow::{Context, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tracing::{info, warn, Level};
use tracing_subscriber::FmtSubscriber;

use tslib_audio::codec::{Encoder, OpusEncoder};
use tslib_audio::config::{AudioConfig, OpusApplication};
use tslib_core::events::{AudioCodec, Event, MessageTarget};
use tslib_core::{Client, ClientConfig, Identity};

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

/// Un brano in coda o in riproduzione: URL + titolo + copertina (se risolti via yt-dlp).
#[derive(Debug, Clone)]
struct Track {
    url: String,
    /// Titolo YouTube (es. "Big Buck Bunny ..."). Se `None`, si mostra l'URL.
    title: Option<String>,
    /// URL della copertina (thumbnail YouTube). Se `Some`, viene inviata come
    /// `[img]...[/img]` così il client TeamSpeak mostra la preview inline.
    thumbnail: Option<String>,
}

impl Track {
    fn new(url: String, title: Option<String>, thumbnail: Option<String>) -> Self {
        let title = title
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        let thumbnail = thumbnail
            .map(|t| t.trim().to_string())
            .filter(|t| t.starts_with("http://") || t.starts_with("https://"));
        Self {
            url,
            title,
            thumbnail,
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
    ytdlp: tokio::process::Child,
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
}

impl Player {
    fn new(volume: f32, eq: Eq, eq_preset: String, loop_mode: LoopMode) -> Self {
        Self {
            source: Source::Idle,
            queue: VecDeque::new(),
            current: None,
            volume,
            paused: false,
            eq,
            eq_preset,
            loop_mode,
        }
    }

    fn is_busy(&self) -> bool {
        !matches!(self.source, Source::Idle)
    }

    fn stop_all(&mut self) {
        kill_source(&mut self.source);
        self.queue.clear();
        self.current = None;
        self.paused = false;
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
        match spawn_stream(&track.url) {
            Ok(stream) => {
                kill_source(&mut self.source);
                self.source = Source::Stream { stream };
                let msg = track.announce("Riproduco");
                self.current = Some(track);
                self.paused = false;
                self.eq.reset();
                msg
            }
            Err(e) => format!("Play fallito: {e:#}"),
        }
    }

    /// Fa partire il prossimo in coda. Ritorna il messaggio da annunciare (se c'è).
    fn start_next(&mut self) -> Option<String> {
        let next = self.queue.pop_front()?;
        match spawn_stream(&next.url) {
            Ok(stream) => {
                kill_source(&mut self.source);
                self.source = Source::Stream { stream };
                let msg = next.announce("Prossimo");
                self.current = Some(next);
                self.paused = false;
                self.eq.reset();
                Some(msg)
            }
            Err(e) => Some(format!("Play fallito per {}: {e:#}", next.display())),
        }
    }

    /// Fa ripartire il brano corrente (usato dal loop "one").
    fn restart_current(&mut self) -> Option<String> {
        let cur = self.current.clone()?;
        match spawn_stream(&cur.url) {
            Ok(stream) => {
                kill_source(&mut self.source);
                self.source = Source::Stream { stream };
                self.paused = false;
                self.eq.reset();
                Some(cur.announce("Ripeto"))
            }
            Err(e) => Some(format!("Replay fallito per {}: {e:#}", cur.display())),
        }
    }

    fn eq_summary(&self) -> String {
        format!(
            "EQ {} (bass {:+.0}dB, mid {:+.0}dB, treble {:+.0}dB)",
            self.eq_preset, self.eq.bass_db, self.eq.mid_db, self.eq.treble_db
        )
    }
}

fn kill_source(source: &mut Source) {
    if let Source::Stream { stream } = source {
        let _ = stream.ytdlp.start_kill();
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
fn spawn_stream(url: &str) -> Result<PipeStream> {
    if !is_url(url) {
        anyhow::bail!("URL non valido (deve iniziare con http:// o https://)");
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
        ytdlp,
        ffmpeg,
        stdout,
        pending: Vec::with_capacity(8192),
    })
}

/// Risolve un URL in uno o più brani con titolo + copertina.
///
/// Usa `yt-dlp --flat-playlist --print "%(title)s ||| %(webpage_url)s ||| %(thumbnail)s"`:
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
                "%(title)s ||| %(webpage_url)s ||| %(thumbnail)s",
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
        // Formato: "titolo ||| url ||| thumbnail" (thumbnail può mancare/essere "NA").
        let mut parts = line.splitn(3, " ||| ");
        let title = parts.next().unwrap_or("").trim();
        let link = parts.next().unwrap_or("").trim();
        let thumb = parts.next().unwrap_or("").trim();
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
        tracks.push(Track::new(link.to_string(), title, thumb));
    }
    if tracks.is_empty() {
        fallback_tracks(url)
    } else {
        info!("Risolti {} brani da {url}", tracks.len());
        tracks
    }
}

/// Fallback quando yt-dlp non risolve: l'URL grezzo si può comunque provare
/// a riprodurre, una ricerca senza risultati no.
fn fallback_tracks(url: String) -> Vec<Track> {
    if is_search_spec(&url) {
        Vec::new()
    } else {
        vec![Track::new(url, None, None)]
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
                    tokio::time::sleep(delay).await;
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
    );
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
    let (meta_tx, mut meta_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<Track>>();

    let mut tick = tokio::time::interval(tokio::time::Duration::from_millis(20));

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
                                Event::UserJoined { user } => info!("[join] {}", user.nickname),
                                Event::UserLeft { user, .. } => info!("[leave] {}", user.nickname),
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
                while let Ok(tracks) = meta_rx.try_recv() {
                    if tracks.is_empty() {
                        let _ = client.send_channel_message(
                            "Nessun risultato trovato.".to_string(),
                        );
                        continue;
                    }
                    let n = tracks.len();
                    if player.is_busy() {
                        // C'è già qualcosa in riproduzione: accoda tutto.
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
                                "Playlist: aggiunti {n} brani in coda (tot. {}). Primo: {first}",
                                player.queue.len(),
                                first = first
                            ));
                        }
                    } else {
                        // Libero: parte subito il primo, il resto in coda.
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
                                "Playlist: {rest_n_plus} brani in coda.\n{msg}",
                                rest_n_plus = rest_n + 1,
                                msg = msg
                            ));
                        } else {
                            let _ = client.send_channel_message(msg);
                        }
                    }
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
                            match player.loop_mode {
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
                        } else if stream.pending.len() >= PipeStream::FRAME_BYTES {
                            let raw: Vec<u8> = stream.pending.drain(..PipeStream::FRAME_BYTES).collect();
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
        }
    }

    kill_source(&mut player.source);
    // Ricorda volume + EQ + loop + canale corrente prima di uscire.
    saved.volume = player.volume;
    saved.eq_bass = player.eq.bass_db;
    saved.eq_mid = player.eq.mid_db;
    saved.eq_treble = player.eq.treble_db;
    saved.eq_preset = player.eq_preset.clone();
    saved.loop_mode = player.loop_mode.as_str().to_string();
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
    meta_tx: tokio::sync::mpsc::UnboundedSender<Vec<Track>>,
) -> Option<String> {
    let msg = message.trim();
    if !msg.starts_with(prefix) {
        return None;
    }
    let body = msg[prefix.len()..].trim();
    let mut parts = body.split_whitespace();
    let cmd = parts.next().unwrap_or("").to_lowercase();
    let rest: Vec<&str> = parts.collect();

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
            "Comandi: !play <url|ricerca|hz> !stop !skip !queue !now !volume !pause !resume !eq <preset> !bass/!mid/!treble !loop !lofi !playlist <nome> !playlists !join !say !users !channels".to_string(),
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
                player.current = Some(Track::new(format!("sine {f:.0}Hz"), Some(format!("sine {f:.0}Hz")), None));
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
                let _ = tx.send(tracks);
            });
            Some(loading)
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
            let extra = format!(
                "(vol {:.0}%, loop {}, {})",
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
                let _ = tx.send(tracks);
            });
            Some("Caricamento lofi... (5 mix in arrivo, poi !loop all per ripeterli)".to_string())
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
            let _ = client.set_input_muted(false);
            let tx = meta_tx.clone();
            tokio::spawn(async move {
                let tracks = resolve_url_list(urls).await;
                let _ = tx.send(tracks);
            });
            Some(format!("Caricamento playlist '{name}' ({n} voci)..."))
        }
        "playlistsave" | "playlist-save" | "saveplaylist" => {
            if rest.is_empty() {
                return Some("Uso: !playlistsave <nome> (salva brano corrente + coda)".to_string());
            }
            let name = normalize_playlist_name(rest[0]);
            if name.is_empty() {
                return Some("Nome playlist non valido (usa lettere/numeri/-/_.".to_string());
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
        );
        let msg = t.announce("Riproduco");
        assert!(msg.contains("[img]https://i.ytimg.com/vi/x/hqdefault.jpg[/img]"));
        assert!(msg.contains("[b]"));
        let plain = Track::new("https://youtu.be/x".to_string(), None, None);
        assert!(!plain.announce("Riproduco").contains("[img]"));
        assert!(plain.display() == "https://youtu.be/x");
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
    fn track_without_thumbnail_still_announces() {
        let t = Track::new(
            "https://example.com/x.mp3".to_string(),
            Some("Brano".to_string()),
            Some("NA".to_string()),
        );
        assert!(t.thumbnail.is_none());
        assert!(t.announce("Riproduco").contains("Brano"));
    }
}
