# Prossimo passo — da qui si riprende

## Stato attuale (funzionante)
- Progetto bot: `/home/rod/develop/ts-bot` — compila (`cargo check` ok)
  - `chat-bot` (`src/main.rs`, framework `tslib-bot`): `!echo !say !info !roll !ora !uptime` + built-in `!help !ping !version`
  - `music-bot` (`src/bin/music-bot.rs`, `tslib-core` + `tslib-audio`): `!help !users !channels !join !say !play [hz] !stop`
- Risposte nel contesto giusto: DM → DM, canale → canale (`MessageTarget::Channel` → `send_channel_message`, altrimenti `send_private_message`)
- Fix in `tslib` (solo working-tree, vedi `git diff` in `/home/rod/develop/tslib`):
  - `crates/tslib-bot/src/bot.rs`: `Bot::run()` ora pompa `client.process_events()` ogni 20 ms (prima non riceveva mai messaggi)
  - **Sicuro per TS6_Droid**: l'app usa `tslib-jni` → solo `tslib-core`; `tslib-bot` non la tocca
- Server: `god.serod.tech:9988`
- Test: `cargo test -p tslib-bot` → 30 + 1 ok

## Prossimo passo: `!play <url>` con yt-dlp (RIMANDATO, da fare)
Obiettivo: nel `music-bot`, `!play <url YouTube>` scarica l'audio e lo trasmette in canale.

### Design previsto
1. Requisiti host: binari `yt-dlp` e `ffmpeg` installati.
2. Pipeline: `yt-dlp -f bestaudio -o - <url>` → pipe a
   `ffmpeg -i pipe:0 -f s16le -ar 48000 -ac 1 pipe:1` → PCM letto a frame da 960 sample (20 ms)
   → `OpusEncoder` esistente → `client.send_audio(pkt, AudioCodec::OpusMusic)` nel tick da 20 ms già presente nel main loop.
3. Vincolo: `Client` non è `Send`, quindi lo streaming resta nel task principale (niente `tokio::spawn` con `&mut client`); aggiungere un `enum Sorgente { Seno { freq }, Pipe { child } }` al posto del flag `playing`.
4. Comandi da aggiungere: `!play <url>`, `!stop` (uccide il child), poi `!skip !queue !now` (coda).
5. Nuova dipendenza Rust: nessuna obbligatoria (si usano `tokio::process::Command` + pipe); opzionale `regex` per validare URL.

### Comandi per riprendere
```bash
cargo run --bin music-bot -- --server god.serod.tech:9988 --nickname MusicBot
which yt-dlp ffmpeg
```

### Checklist
- [ ] `!play <url>` avvia yt-dlp+ffmpeg e streamma
- [ ] `!stop` ferma e pulisce il processo
- [ ] Gestione errori: URL non valido, yt-dlp assente, canale pieno
- [ ] (dopo) coda con `!skip !queue !now`
