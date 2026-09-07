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
//! Requisiti host per !play <url>: binari `yt-dlp` e `ffmpeg` installati.
//!
//! Comandi in chat:
//!   !help, !users, !channels, !join <id|nome>, !say <msg>,
//!   !play <url|hz>, !stop, !skip, !queue, !now, !volume <0-200>

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
}

/// Stato persistente tra riavvii: volume e ultimo canale joinato.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct BotState {
    /// Volume 0.0-2.0 (default 1.0).
    #[serde(default = "default_volume")]
    volume: f32,
    /// Ultimo canale (ID + nome per fallback se l'ID cambia).
    #[serde(default)]
    channel_id: Option<u64>,
    #[serde(default)]
    channel_name: Option<String>,
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
    url: String,
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
    queue: VecDeque<String>,
    /// URL corrente o descrizione ("sine 440Hz").
    current: Option<String>,
    /// Volume 0.0-2.0 (1.0 = 100%). Applicato ai sample PCM prima dell'encode Opus.
    volume: f32,
}

impl Player {
    fn new(volume: f32) -> Self {
        Self { source: Source::Idle, queue: VecDeque::new(), current: None, volume }
    }

    fn is_busy(&self) -> bool {
        !matches!(self.source, Source::Idle)
    }

    fn stop_all(&mut self) {
        kill_source(&mut self.source);
        self.queue.clear();
        self.current = None;
    }

    /// Fa partire l'URL subito, uccidendo la sorgente precedente.
    fn start_url_now(&mut self, url: String) -> String {
        match spawn_stream(&url) {
            Ok(stream) => {
                kill_source(&mut self.source);
                self.source = Source::Stream { stream };
                self.current = Some(url.clone());
                format!("Riproduco: {url}")
            }
            Err(e) => format!("Play fallito: {e:#}"),
        }
    }

    /// Fa partire il prossimo in coda. Ritorna il messaggio da annunciare (se c'è).
    fn start_next(&mut self) -> Option<String> {
        let next = self.queue.pop_front()?;
        match spawn_stream(&next) {
            Ok(stream) => {
                kill_source(&mut self.source);
                self.source = Source::Stream { stream };
                self.current = Some(next.clone());
                Some(format!("Prossimo: {next}"))
            }
            Err(e) => Some(format!("Play fallito per {next}: {e:#}")),
        }
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
        .args(["-f", "bestaudio", "-o", "-", "--no-playlist", "--quiet", "--no-warnings", url])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("yt-dlp non trovato o non avviabile (installarlo sul host)")?;

    let ytdlp_out = ytdlp.stdout.take().context("yt-dlp: stdout non disponibile")?;
    let ytdlp_stdio: Stdio = ytdlp_out
        .try_into()
        .map_err(|_| anyhow::anyhow!("conversione stdout yt-dlp fallita"))?;

    let mut ffmpeg = tokio::process::Command::new("ffmpeg")
        .args([
            "-hide_banner", "-loglevel", "error",
            "-i", "pipe:0",
            "-f", "s16le", "-ar", "48000", "-ac", "1",
            "pipe:1",
        ])
        .stdin(ytdlp_stdio)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("ffmpeg non trovato o non avviabile (installarlo sul host)")?;

    let stdout = ffmpeg.stdout.take().context("ffmpeg: stdout non disponibile")?;

    Ok(PipeStream { ytdlp, ffmpeg, stdout, url: url.to_string(), pending: Vec::with_capacity(8192) })
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
    if saved.volume != 1.0 {
        info!("Volume ripristinato: {:.0}%", saved.volume * 100.0);
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

    info!("Connessione a {}...", args.server);
    let mut client = Client::connect(config)?;
    client.wait_connected().await?;
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

    let mut player = Player::new(saved.volume);
    let sample_rate = audio_cfg.sample_rate as f64;

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

                // 2) Streaming audio: un frame Opus ogni 20ms
                let vol = player.volume;
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
                            let finished = stream.url.clone();
                            kill_source(&mut player.source);
                            player.current = None;
                            // Auto-avanza con la coda
                            if let Some(msg) = player.start_next() {
                                let _ = client.send_channel_message(format!("Finito: {finished}\n{msg}"));
                            } else {
                                let _ = client.send_channel_message(format!("Finito: {finished}"));
                            }
                        } else if stream.pending.len() >= PipeStream::FRAME_BYTES {
                            let raw: Vec<u8> = stream.pending.drain(..PipeStream::FRAME_BYTES).collect();
                            let mut pcm = vec![0i16; frame_samples];
                            for (i, s) in pcm.iter_mut().enumerate() {
                                *s = i16::from_le_bytes([raw[2 * i], raw[2 * i + 1]]);
                            }
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
    // Ricorda volume + canale corrente prima di uscire.
    saved.volume = player.volume;
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
) -> Option<String> {
    let msg = message.trim();
    if !msg.starts_with(prefix) {
        return None;
    }
    let body = msg[prefix.len()..].trim();
    let mut parts = body.split_whitespace();
    let cmd = parts.next().unwrap_or("").to_lowercase();
    let rest: Vec<&str> = parts.collect();

    match cmd.as_str() {
        "help" => Some(
            "Comandi: !help !users !channels !join <id|nome> !say <msg> !play <url|hz> !stop !skip !queue !now !volume <0-200>".to_string(),
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
                return Some("Uso: !play <url YouTube> oppure !play [hz 50-2000]".to_string());
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
                player.current = Some(format!("sine {f:.0}Hz"));
                let _ = client.set_input_muted(false);
                return Some(format!("Riproduzione nota a {f:.0} Hz (OpusMusic). !stop per fermare."));
            }
            // Altrimenti URL
            let url = arg.to_string();
            if !is_url(&url) {
                return Some("Uso: !play <url YouTube> oppure !play [hz 50-2000]".to_string());
            }
            let _ = client.set_input_muted(false);
            if player.is_busy() {
                player.queue.push_back(url.clone());
                Some(format!("In coda (#{}) : {url}", player.queue.len()))
            } else {
                Some(player.start_url_now(url))
            }
        }
        "stop" => {
            player.stop_all();
            Some("Riproduzione fermata e coda svuotata.".to_string())
        }
        "skip" => {
            kill_source(&mut player.source);
            player.current = None;
            if let Some(msg) = player.start_next() {
                Some(format!("Skip. {msg}"))
            } else {
                Some("Skip. Niente altro in coda.".to_string())
            }
        }
        "queue" => {
            if player.queue.is_empty() {
                return Some("Coda vuota.".to_string());
            }
            let mut out = format!("Coda ({}):\n", player.queue.len());
            for (i, u) in player.queue.iter().take(10).enumerate() {
                out.push_str(&format!("{}. {u}\n", i + 1));
            }
            Some(out)
        }
        "now" | "nowplaying" | "np" => match &player.current {
            Some(c) => Some(format!("In riproduzione: {c} (vol {:.0}%)", player.volume * 100.0)),
            None => Some(format!("Niente in riproduzione. (vol {:.0}%)", player.volume * 100.0)),
        },
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
        _ => None,
    }
}
