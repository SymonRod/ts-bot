//! Chat bot TeamSpeak basato su tslib-bot.
//!
//! Uso:
//!   cargo run --bin chat-bot -- --server localhost:9987 --nickname MioBot
//!   cargo run --bin chat-bot -- --server localhost --nickname MioBot --password segreta --channel Lobby
//!
//! Comandi in chat (prefisso di default `!`):
//!   !ping, !help, !version (built-in) + !echo, !say, !info, !roll, !ora, !uptime

use anyhow::Result;
use clap::Parser;
use std::time::Instant;
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

use tslib_bot::{Bot, BotConfig};
use tslib_core::Identity;

#[derive(Parser, Debug)]
#[command(author, version, about = "Bot chat TeamSpeak con tslib")]
struct Args {
    /// Indirizzo server (host o host:porta, default 9987)
    #[arg(short, long)]
    server: String,

    /// Nickname del bot
    #[arg(short, long, default_value = "TsBot")]
    nickname: String,

    /// Password del server (opzionale)
    #[arg(short, long)]
    password: Option<String>,

    /// Canale iniziale da joinare (opzionale)
    #[arg(short, long)]
    channel: Option<String>,

    /// File identità (creato se non esiste)
    #[arg(short, long, default_value = "identity.json")]
    identity: String,

    /// Prefisso comandi
    #[arg(long, default_value = "!")]
    prefix: String,

    /// UID owner (ripetibile, per comandi owner-only futuri)
    #[arg(long)]
    owner: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    let args = Args::parse();
    let started = Instant::now();

    // --- Identità: carica o crea ---
    let identity = if std::path::Path::new(&args.identity).exists() {
        info!("Carico identità da {}", args.identity);
        Identity::load(&args.identity)?
    } else {
        info!("Creo nuova identità");
        let identity = Identity::create()?;
        identity.save(&args.identity)?;
        info!("Identità salvata in {}", args.identity);
        identity
    };
    info!("UID: {}", identity.unique_id());

    // --- Config ---
    let mut builder = BotConfig::builder()
        .address(&args.server)
        .identity(identity)
        .nickname(&args.nickname)
        .command_prefix(&args.prefix)
        .auto_reconnect(true);
    if let Some(pw) = args.password {
        builder = builder.password(pw);
    }
    if let Some(ch) = args.channel {
        builder = builder.channel(ch);
    }
    for o in args.owner {
        builder = builder.owner(o);
    }
    let config = builder.build()?;

    // --- Bot ---
    let mut bot = Bot::new(config).await?;
    let prefix = args.prefix.clone();

    // !echo <testo>
    bot.command("echo", |ctx| async move {
        let msg = ctx.args_string();
        if msg.is_empty() {
            ctx.reply("Uso: !echo <messaggio>").await
        } else {
            ctx.reply(msg).await
        }
    })
    .await;

    // !say <testo> (con BBCode)
    bot.command("say", |ctx| async move {
        let msg = ctx.args_string();
        if msg.is_empty() {
            ctx.reply("Uso: !say <messaggio>").await
        } else {
            ctx.reply(format!("[b]Il bot dice:[/b] {msg}")).await
        }
    })
    .await;

    // !info
    {
        let p = prefix.clone();
        bot.command("info", move |ctx| {
            let p = p.clone();
            async move {
                ctx.reply(format!(
                    "[b]TsBot[/b] basato su tslib\nComandi: {p}help per la lista"
                ))
                .await
            }
        })
        .await;
    }

    // !roll [facce] — tiro dado
    bot.command("roll", |ctx| async move {
        use std::time::{SystemTime, UNIX_EPOCH};
        let sides: u32 = ctx.arg(0).and_then(|s| s.parse().ok()).unwrap_or(6);
        if sides < 2 {
            return ctx.reply("Il dado deve avere almeno 2 facce!").await;
        }
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let roll = (nanos % sides) + 1;
        ctx.reply(format!("Dado d{sides}... [b]{roll}[/b]!")).await
    })
    .await;

    // !ora — orario server (del bot)
    bot.command("ora", |ctx| async move {
        let now = chrono_now();
        ctx.reply(format!("Sono le {now}")).await
    })
    .await;

    // !uptime
    bot.command("uptime", move |ctx| async move {
        let s = started.elapsed().as_secs();
        let (h, m, s) = (s / 3600, (s % 3600) / 60, s % 60);
        ctx.reply(format!("Online da {h}h {m}m {s}s")).await
    })
    .await;

    info!("Comandi registrati: echo, say, info, roll, ora, uptime (+ help/ping/version built-in)");
    info!("Avvio bot su {}...", args.server);
    bot.run().await?;
    Ok(())
}

/// Orario HH:MM:SS senza dipendenze extra.
fn chrono_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let s = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (h, m, s) = ((s / 3600) % 24, (s / 60) % 60, s % 60);
    format!("{h:02}:{m:02}:{s:02}")
}
