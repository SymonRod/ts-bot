#!/bin/sh
# Entrypoint: traduce le variabili d'ambiente negli argomenti CLI del bot.
set -eu

BIN="${BOT_BIN:-music-bot}"
case "$BIN" in
  chat-bot|music-bot) ;;
  *) echo "BOT_BIN non valido: '$BIN' (usa 'chat-bot' o 'music-bot')" >&2; exit 1 ;;
esac

if [ -z "${TS_SERVER:-}" ]; then
  echo "Errore: variabile TS_SERVER non impostata (es. voce.example.com:9987)" >&2
  exit 1
fi

# Default diversi per i due bot se non specificato.
if [ -z "${TS_IDENTITY:-}" ]; then
  if [ "$BIN" = "chat-bot" ]; then TS_IDENTITY=/data/identity.json; else TS_IDENTITY=/data/identity-music.json; fi
fi
: "${TS_NICKNAME:=MusicBot}"
: "${TS_STATE:=/data/music-state.json}"
: "${TS_PREFIX:=!}"

set -- "$BIN" --server "$TS_SERVER" --nickname "$TS_NICKNAME" \
  --identity "$TS_IDENTITY" --prefix "$TS_PREFIX"

# music-bot supporta anche --state (persistenza volume/canale).
if [ "$BIN" = "music-bot" ]; then
  set -- "$@" --state "$TS_STATE"
fi
if [ -n "${TS_PASSWORD:-}" ]; then
  set -- "$@" --password "$TS_PASSWORD"
fi
if [ -n "${TS_CHANNEL:-}" ]; then
  set -- "$@" --channel "$TS_CHANNEL"
fi

exec "$@"
