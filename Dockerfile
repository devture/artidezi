# SPDX-FileCopyrightText: 2026 Slavi Pantaleev
#
# SPDX-License-Identifier: Apache-2.0
#
# Builds a fully static musl `artidezi` binary (libgit2 and OpenSSL vendored), so the very same
# file runs in this image and on any Linux worker VM. The runtime
# image adds only ca-certificates and the git binary the orchestrator uses for bundles.

FROM docker.io/rust:1.99.1-alpine3.24@sha256:44819b27be4c97b0e4d7203bd2e02c4820f97700211b6a37fb4c246a2de59383 AS build
RUN apk add --no-cache musl-dev build-base cmake perl pkgconfig linux-headers git
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src/ ./src/
COPY crates/ ./crates/
COPY assets/branding/artidezi-bot.svg ./assets/branding/
ARG ARTIDEZI_VERSION=dev
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin artidezi \
 && strip target/release/artidezi \
 && readelf --program-headers --dynamic target/release/artidezi > /tmp/artidezi-elf \
 && ! grep -Eq 'INTERP|\(NEEDED\)' /tmp/artidezi-elf \
 && target/release/artidezi --version \
 && cp target/release/artidezi /usr/local/bin/artidezi

FROM docker.io/ubuntu:26.04@sha256:6a06111c3b0a8600feadb270aec771dc239bf407b25625f93f44ab00e11f4d3c AS runtime
LABEL org.opencontainers.image.source="https://github.com/devture/artidezi" \
      org.opencontainers.image.licenses="Apache-2.0"
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd -g 2003 artidezi \
 && useradd -m -u 2003 -g artidezi -d /var/lib/artidezi -s /usr/sbin/nologin artidezi
COPY --from=build /usr/local/bin/artidezi /usr/local/bin/artidezi
COPY LICENSES/Apache-2.0.txt /LICENSE.txt
USER artidezi
WORKDIR /var/lib/artidezi
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/artidezi"]
CMD ["serve", "--config", "/etc/artidezi/artidezi.yaml", "--secrets-dir", "/etc/artidezi/secrets", "--listen", "0.0.0.0:8080"]

# The local scripted worker uses Python, and never receives model credentials or a Docker socket.
FROM runtime AS development
USER root
COPY --from=docker.io/node:24.22.2-trixie-slim@sha256:0820eaa85e07d0490446dd01d14707e17e6108a36988cf374899cd057eb3812d /usr/local/ /usr/local/
RUN apt-get update \
 && apt-get install -y --no-install-recommends python3 wget \
 && rm -rf /var/lib/apt/lists/* \
 && npm install --global --ignore-scripts @earendil-works/pi-coding-agent@0.99.2
USER artidezi

# Keep the default build as the minimal production image.
FROM runtime AS final
