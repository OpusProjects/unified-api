# Stage 1: the web UI. The ui repository is its own project with its own
# versions; each unified-api release pins the one it ships (UI_REF), so an
# image always serves the UI it was tested with.
FROM node:22-alpine AS ui
ARG UI_REF=v0.6.0
RUN apk add --no-cache git \
    && git clone --depth 1 --branch "$UI_REF" https://github.com/OpusProjects/ui.git /ui
WORKDIR /ui
RUN npm ci --no-audit --no-fund && npm run build

# Stage 2: build, in two steps so a release only compiles what changed.
# cargo-chef reduces the manifests to a recipe (the service's own version
# masked, so release-please's bump does not count), and `cook` compiles the
# dependencies from it alone: that layer is reused, from the CI build cache,
# for as long as Cargo.lock's dependencies stay the same. Any change to them
# rebuilds the whole layer once.
#
# --locked: build exactly the versions CI tested. Without it, cargo may
# quietly re-resolve inside the image and ship dependency versions the test
# suite never saw.
FROM rust:1.96-slim-trixie AS chef
RUN apt-get update && apt-get install -y pkg-config libssl-dev curl && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-chef --version 0.1.78 --locked
WORKDIR /app

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY benches/ benches/
COPY src/ src/
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY benches/ benches/
COPY src/ src/
RUN cargo build --release --locked

# Stage 3: runtime
FROM debian:trixie-slim

# git: the app clones connector-script projects at boot (projects.yaml).
# Python libraries commonly imported by connector scripts (requests, yaml,
# jinja2) come from apt on purpose: they track the distro's security updates
# with every image rebuild, unlike a pip install frozen at build time (which
# would also need --break-system-packages on trixie). python-is-python3
# provides the /usr/bin/python symlink for scripts with a bare
# `#!/usr/bin/env python` shebang.
RUN apt-get update && apt-get install -y \
    ca-certificates git \
    python3 python-is-python3 python3-venv python3-pip \
    python3-requests python3-yaml python3-jinja2 python3-pyvmomi \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd -r unified && useradd -r -g unified -s /sbin/nologin unified

# Writable state directory for what should survive a restart: the optional
# cache snapshot, and project checkouts when projects.dir points here (as
# docs/deployment.md does). In k8s, mount a volume over it.
RUN mkdir -p /var/lib/unified-api && chown unified:unified /var/lib/unified-api

COPY --from=builder /app/target/release/unified-api /usr/local/bin/unified-api
# Served at "/" when the config points server.ui.assets_dir here (the bundled
# config does).
COPY --from=ui /ui/apps/unified-api/dist /usr/share/unified-api/ui
COPY config/ /app/config/
# Demo connector/enricher/output scripts the default config points at. They
# double as the integration-test fixtures, so they live under tests/ (mirroring
# the src/adapters/out/ layout they stand in for).
COPY tests/adapters/out/ /app/tests/adapters/out/

WORKDIR /app

# The bundled config clones its projects into the default `projects.dir`,
# relative to /app, as the unprivileged user below: /app itself stays
# root-owned (config and scripts are not the process's to change), so the
# checkout directory is created here and handed over.
RUN mkdir -p /app/projects && chown unified:unified /app/projects

USER unified

EXPOSE 8182

# Report container health from the liveness probe. Uses python3 (already in the
# runtime image for connectors) so no extra package is needed. Orchestrators
# outside k8s (docker run, Compose) rely on this; k8s uses the /healthz and
# /readyz HTTP probes directly instead.
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD python3 -c "import urllib.request,sys; sys.exit(0 if urllib.request.urlopen('http://localhost:8182/healthz').read()==b'ok' else 1)"

CMD ["unified-api"]
