# syntax=docker/dockerfile:1
# Build del bot TeamSpeak (chat-bot + music-bot).
# Contesto di build: radice del repo ts-bot (deve contenere ./tslib come submodule).
#
#   docker build -t ts-bot .
#   docker run --rm -e TS_SERVER=voce.example.com:9987 ts-bot

# ---------- builder ----------
# rust:bookworm = stable corrente (il Cargo.lock richiede una toolchain recente).
FROM rust:bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config cmake clang \
    libssl-dev libasound2-dev libopus-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copia prima i manifest per sfruttare la cache dei layer sulle dipendenze.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tslib ./tslib

# Build dei due binari.
# (Niente cache-mount BuildKit: così la build funziona anche senza BuildKit.
#  In CI la cache è gestita da docker/build-push-action con type=gha.)
RUN cargo build --release --bin chat-bot --bin music-bot \
    && cp target/release/chat-bot /usr/local/bin/chat-bot \
    && cp target/release/music-bot /usr/local/bin/music-bot

# ---------- runtime ----------
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates ffmpeg python3 python3-pip \
    libasound2 libssl3 \
    && pip install --no-cache-dir --break-system-packages yt-dlp \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -m -u 1000 bot

COPY --from=builder /usr/local/bin/chat-bot /usr/local/bin/music-bot /usr/local/bin/
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

# Identità TS e stato persistente (monta un volume qui).
RUN mkdir -p /data && chown bot:bot /data
VOLUME /data

USER bot
WORKDIR /data

# Configurazione via env (vedi docker-entrypoint.sh e docker-compose.yml).
ENV BOT_BIN=music-bot \
    TS_SERVER="" \
    TS_NICKNAME=MusicBot \
    TS_CHANNEL="" \
    TS_PASSWORD="" \
    TS_IDENTITY=/data/identity-music.json \
    TS_STATE=/data/music-state.json \
    TS_PREFIX="!" \
    RUST_LOG=info

ENTRYPOINT ["docker-entrypoint.sh"]
