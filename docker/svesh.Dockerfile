# svesh fork image: web UI + server + CLI tools, built from source, runs as uid 1000.
# Upstream's docker/Dockerfile packs prebuilt binaries and runs as root; this one is
# self-contained so that GitHub Actions can build it with one `docker build`.
#
#   docker build -f docker/svesh.Dockerfile -t bichon:dev .

FROM node:24.21.0-trixie-slim AS web
WORKDIR /src/web
RUN npm install -g pnpm@10.34.6
COPY web/package.json web/pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile
COPY web/ ./
RUN pnpm run build

FROM rust:1.99.0-slim-trixie AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends git pkg-config \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
# build.rs skips the frontend build when web/dist exists and pnpm is absent.
COPY --from=web /src/web/dist web/dist
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p bichon-server -p bichon-cli -p bichon-admin \
 && mkdir -p /out \
 && cp target/release/bichon-server target/release/bichon-cli target/release/bichon-admin /out/

FROM debian:trixie-20261005-slim
ARG VERSION=dev
LABEL org.opencontainers.image.source="https://github.com/svesh87/bichon" \
      org.opencontainers.image.description="Bichon email archiver, svesh fork (MCP, drafts)" \
      org.opencontainers.image.licenses="AGPL-3.0-only" \
      org.opencontainers.image.version="${VERSION}"
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl tini \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd -g 1000 bichon \
 && useradd -u 1000 -g 1000 -d /data -M -s /usr/sbin/nologin bichon \
 && mkdir -p /data && chown 1000:1000 /data
COPY --from=build /out/bichon-server /opt/bichon/bichon-server
COPY --from=build /out/bichon-cli /out/bichon-admin /usr/local/bin/
COPY LICENSE /opt/bichon/LICENSE
USER 1000:1000
WORKDIR /data
ENV BICHON_ROOT_DIR=/data
EXPOSE 15630
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD curl -fs http://127.0.0.1:${BICHON_HTTP_PORT:-15630}${BICHON_BASE_URL:-}/api/status || exit 1
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["/opt/bichon/bichon-server"]
