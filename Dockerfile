# syntax=docker/dockerfile:1
# Build del bot TeamSpeak (chat-bot + music-bot).
# Contesto di build: radice del repo ts-bot (deve contenere ./tslib come submodule).
#
#   docker build -t ts-bot .
#   docker run --rm -e TS_SERVER=voce.example.com:9987 ts-bot

# ---------- base: toolchain + dipendenze di sistema ----------
# rust:bookworm = stable corrente (il Cargo.lock richiede una toolchain recente).
FROM rust:bookworm AS base

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config cmake clang \
    libssl-dev libasound2-dev libopus-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# ---------- planner: scheletro con i soli manifest ----------
# Tutti i Cargo.toml (anche quelli del submodule tslib) con sorgenti vuoti.
# Il contenuto cambia solo se cambiano manifest o Cargo.lock, non il codice.
FROM base AS planner
COPY . /src
RUN cd /src \
    && find . -name Cargo.toml -not -path '*/target/*' | while read -r f; do \
         d=/skel/$(dirname "$f"); mkdir -p "$d/src"; cp "$f" "$d/"; \
         echo 'fn main() {}' > "$d/src/main.rs"; touch "$d/src/lib.rs"; \
       done \
    && cp Cargo.lock /skel/ && cp tslib/Cargo.lock /skel/tslib/ \
    && rm /skel/src/lib.rs \
    && mkdir -p /skel/src/bin && echo 'fn main() {}' > /skel/src/bin/music-bot.rs

# ---------- builder ----------
FROM base AS builder

# Dipendenze compilate in un layer a sé: resta in cache finché lo scheletro
# non cambia, quindi un commit che tocca solo il codice non le ricompila.
COPY --from=planner /skel/ ./
RUN cargo build --release --bin chat-bot --bin music-bot

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tslib ./tslib

# touch: i sorgenti copiati possono avere mtime più vecchio degli stub,
# e cargo li considererebbe già compilati.
RUN find src tslib -name '*.rs' -exec touch {} + \
    && cargo build --release --bin chat-bot --bin music-bot \
    && cp target/release/chat-bot /usr/local/bin/chat-bot \
    && cp target/release/music-bot /usr/local/bin/music-bot

# ---------- runtime ----------
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates ffmpeg python3 python3-pip fonts-dejavu-core \
    libasound2 libssl3 \
    && pip install --no-cache-dir --break-system-packages yt-dlp \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -m -u 1000 bot

COPY --from=builder /usr/local/bin/chat-bot /usr/local/bin/music-bot /usr/local/bin/
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

# fonts-dejavu-core: font per le scritte dell'animazione di attesa di !video.
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
