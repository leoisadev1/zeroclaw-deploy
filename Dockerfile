# syntax=docker/dockerfile:1

# ── Stage 1: Build ────────────────────────────────────────────
# Keep builder and release on Debian 12 to avoid GLIBC ABI drift
FROM rust:1.93-slim-bookworm AS builder

WORKDIR /app

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# 1. Copy manifests to cache dependencies
COPY Cargo.toml Cargo.lock ./
# Create dummy main.rs to build dependencies
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release --locked
RUN rm -rf src

# 2. Copy source code
COPY . .
# Touch main.rs to force rebuild
RUN touch src/main.rs
RUN cargo build --release --locked && \
    strip target/release/zeroclaw

# ── Stage 2: Permissions & Config Prep ────────────────────────
FROM busybox:latest AS permissions
# Create directory structure
RUN mkdir -p /zeroclaw-data/.zeroclaw /zeroclaw-data/workspace

# Create config - tokens will be set via environment variables at runtime
RUN cat > /zeroclaw-data/.zeroclaw/config.toml << 'EOF'
workspace_dir = "/zeroclaw-data/workspace"
config_path = "/zeroclaw-data/.zeroclaw/config.toml"
api_key = ""
default_provider = "openrouter"
default_model = "anthropic/claude-sonnet-4-20250514"
default_temperature = 0.7

[gateway]
port = 3000
host = "[::]"
allow_public_bind = true

[browser]
enabled = true
allowed_domains = ["*"]
EOF

RUN chown -R 65534:65534 /zeroclaw-data

# ── Stage 3: Production Runtime (Distroless) ─────────────────
FROM gcr.io/distroless/cc-debian12:nonroot AS release

COPY --from=builder /app/target/release/zeroclaw /usr/local/bin/zeroclaw
COPY --from=permissions /zeroclaw-data /zeroclaw-data

ENV ZEROCLAW_WORKSPACE=/zeroclaw-data/workspace
ENV HOME=/zeroclaw-data
ENV PROVIDER="openrouter"
ENV ZEROCLAW_MODEL="anthropic/claude-sonnet-4-20250514"
ENV ZEROCLAW_GATEWAY_PORT=3000
ENV TELEGRAM_BOT_TOKEN="7652109185:AAEdM-qbi72WAcL-Bwhsbw_-d0ZesG0yfz8"

WORKDIR /zeroclaw-data
USER 65534:65534
EXPOSE 3000
ENTRYPOINT ["zeroclaw"]
CMD ["gateway", "--port", "3000", "--host", "[::]"]
