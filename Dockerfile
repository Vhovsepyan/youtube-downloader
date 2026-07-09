## ---- build stage ----
FROM rust:1-slim-bookworm AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

## ---- runtime stage ----
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        ffmpeg \
        python3 \
        python3-pip \
    && pip3 install --no-cache-dir --break-system-packages -U yt-dlp \
    && apt-get purge -y --auto-remove python3-pip \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/youtube-downloader /usr/local/bin/youtube-downloader

# Bake the static frontend into the image so the app can serve it directly
# when there's no reverse proxy in front (e.g. Cloud Run). The docker-compose
# setup still fronts this with Caddy, which serves web/ itself.
COPY web /srv/frontend

ENV CACHE_DIR=/data/cache
ENV FRONTEND_DIR=/srv/frontend
ENV MAX_CACHE_GB=10
ENV MAX_CONCURRENT_DOWNLOADS=2
ENV DOWNLOAD_TIMEOUT_SECS=900

VOLUME ["/data/cache"]
EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/youtube-downloader"]
