# Stage 1: build
FROM rust:1.96-slim-trixie AS builder

RUN apt-get update && apt-get install -y pkg-config libssl-dev curl && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release && rm -rf src

COPY src/ src/
RUN touch src/main.rs && cargo build --release

# Stage 2: runtime
FROM debian:trixie-slim

RUN apt-get update && apt-get install -y ca-certificates python3 && rm -rf /var/lib/apt/lists/*

RUN groupadd -r unified && useradd -r -g unified -s /sbin/nologin unified

COPY --from=builder /app/target/release/unified-api /usr/local/bin/unified-api
COPY config/ /app/config/
# Demo connector/enricher/output scripts the default config points at. They
# double as the integration-test fixtures, so they live under tests/ (mirroring
# the src/adapters/out/ layout they stand in for).
COPY tests/adapters/out/ /app/tests/adapters/out/

WORKDIR /app

USER unified

EXPOSE 8182

# Report container health from the liveness probe. Uses python3 (already in the
# runtime image for connectors) so no extra package is needed. Orchestrators
# outside k8s (docker run, Compose) rely on this; k8s uses the /healthz and
# /readyz HTTP probes directly instead.
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD python3 -c "import urllib.request,sys; sys.exit(0 if urllib.request.urlopen('http://localhost:8182/healthz').read()==b'ok' else 1)"

CMD ["unified-api"]
