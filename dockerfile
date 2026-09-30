# ============================
# 🛰️ TIDasONE CORE DOCKERFILE
# ============================

# Builder
FROM rust:1.90 AS builder
WORKDIR /app

# Workspace
COPY Cargo.toml Cargo.lock ./
COPY .sqlx ./.sqlx
COPY apps ./apps
COPY packages ./packages

ENV SQLX_OFFLINE=true

# Build the Core API binary
RUN cargo build --release --manifest-path apps/api/Cargo.toml


# ============================
# Runtime
# ============================
FROM debian:bookworm-slim
WORKDIR /app

RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    netcat-openbsd \
    postgresql-client \
 && rm -rf /var/lib/apt/lists/*

# 👉 Correct binary name for core
COPY --from=builder /app/target/release/tid-wayfarer /app/api

# Migrations baked in so the K8s migrate Job (same image) can apply them
COPY --from=builder /app/packages/db/migrations /app/migrations

# Wait-for-DB
COPY scripts/wait-for-db.sh /app/wait-for-db.sh
RUN chmod +x /app/wait-for-db.sh

EXPOSE 3000
ENV RUST_LOG=info

CMD ["./wait-for-db.sh"]

