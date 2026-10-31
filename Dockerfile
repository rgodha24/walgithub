# walgit — one binary in front of an object store.
#
#   docker build -t walgit .
#   docker run --rm --stop-timeout 60 -p 127.0.0.1:8080:8080 \
#       -e WALGIT__STORE__BUCKET=my-bucket \
#       -e WALGIT_TOKEN="$(openssl rand -hex 32)" \
#       -v walgit-cache:/var/lib/walgit \
#       walgit
#
# The image bakes `deploy/host.toml` at /etc/walgit/walgit.toml and takes the bucket, token and
# every other setting from the environment (`WALGIT__SECTION__KEY`). docs/HOSTING.md is the host
# guide; the Compose stack is deploy/compose.yaml (WAL-Git + WAL-Github).
#
# The runtime image carries git (upload-pack, repack, bundle, index-pack run as subprocesses),
# git-lfs, CA certificates and tini. /var/lib/walgit is a disposable cache: materialized packs,
# a self-signed certificate, task logs. The bucket is the only durable state.
#
# The facade E2E image (docs/GITHUB.md) is this same build with a different baked config:
#   docker build --build-arg WALGIT_CONFIG_FILE=deploy/walgithub.toml \
#                --build-arg WALGIT_HTTP_PORT=8097 -t walgithub:smoke .

# ---- 1. web UI (embedded into the binary at compile time) ---------------------------
FROM docker.io/library/node:24-bookworm-slim AS web
RUN corepack enable && corepack prepare pnpm@10 --activate
WORKDIR /src/web
COPY web/package.json web/pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile
COPY web/ ./
RUN pnpm run build && test -f dist/index.html && test -f dist/repos.js

# ---- 2. rust build ------------------------------------------------------------------
FROM docker.io/library/rust:1.97-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler libprotobuf-dev pkg-config cmake perl python3 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY --from=web /src/web/dist ./web/dist
ARG WALGIT_BUILD_SHA=dev
ENV WALGIT_BUILD_SHA=${WALGIT_BUILD_SHA}
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p walgit-cli \
    && install -D target/release/walgit /out/bin/walgit \
    && install -D target/release/walgit-server /out/bin/walgit-server

# ---- 3. runtime ---------------------------------------------------------------------
# trixie ships git 2.47+: walgit wants >= 2.47 on the server (pack.writeReverseIndex, bundle-uri,
# `index-pack --rev-index`); clients need >= 2.46.
FROM docker.io/library/debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends git git-lfs ca-certificates tini curl \
    && rm -rf /var/lib/apt/lists/* \
    && git --version
RUN useradd --uid 1000 --create-home --shell /bin/sh walgit \
    && mkdir -p /etc/walgit /var/lib/walgit && chown walgit:walgit /var/lib/walgit
COPY --from=build /out/bin/walgit /out/bin/walgit-server /usr/local/bin/
ARG WALGIT_CONFIG_FILE=deploy/host.toml
ARG WALGIT_HTTP_PORT=8080
COPY ${WALGIT_CONFIG_FILE} /etc/walgit/walgit.toml
LABEL org.opencontainers.image.title="walgit" \
      org.opencontainers.image.description="git hosting on an object store: smart HTTP, bundle-uri, LFS, web UI" \
      org.opencontainers.image.licenses="MIT"
ENV WALGIT_CONFIG=/etc/walgit/walgit.toml \
    WALGIT_HTTP_PORT=${WALGIT_HTTP_PORT}
USER walgit
WORKDIR /home/walgit
EXPOSE ${WALGIT_HTTP_PORT}
VOLUME ["/var/lib/walgit"]
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD port="${PORT:-${WALGIT__SERVER__LISTEN##*:}}"; port="${port:-${WALGIT_HTTP_PORT}}"; \
        curl -fsS "http://127.0.0.1:${port}/readyz" || curl -fsSk "https://127.0.0.1:${port}/readyz" || exit 1
STOPSIGNAL SIGTERM
ENTRYPOINT ["tini", "--", "walgit-server"]
