# Stage 1: build
FROM rust:1.87-slim AS builder

RUN apt-get update && apt-get install -y pkg-config libssl-dev curl && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release && rm -rf src

COPY src/ src/
RUN touch src/main.rs && cargo build --release

# Stage 2: runtime
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates python3 && rm -rf /var/lib/apt/lists/*

RUN groupadd -r unified && useradd -r -g unified -s /sbin/nologin unified

COPY --from=builder /app/target/release/unified-api /usr/local/bin/unified-api
COPY config/ /app/config/
COPY test-connectors/ /app/test-connectors/

WORKDIR /app

USER unified

EXPOSE 8182

CMD ["unified-api"]
