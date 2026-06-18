# Builder image for the cluster build Job: the Rust toolchain plus a modern
# protoc baked in, so the Job needs NO host-cached protoc and NO runtime download
# of it. Built with `pelagos build` and pushed with `pelagos image push` (dogfood)
# to the cluster registry; the Job's pod pulls it like any other image.
#
#   pelagos build -t <registry>/pelagos-builder:rust-protoc-34.1 \
#     --file k8s/build/builder.Dockerfile k8s/build
#   pelagos image push <registry>/pelagos-builder:rust-protoc-34.1
#
# `rust:1-bookworm` resolves via the Zot docker.io mirror (registries.toml).
FROM rust:1-bookworm

# protoc must be new enough for the CRI api.proto's `debug_redact` option;
# Debian apt's protoc (3.21) is too old. Match the dev box (omen) at 34.1.
ARG PROTOC_VER=34.1
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends unzip; \
    rm -rf /var/lib/apt/lists/*; \
    curl -fsSL -o /tmp/protoc.zip \
      "https://github.com/protocolbuffers/protobuf/releases/download/v${PROTOC_VER}/protoc-${PROTOC_VER}-linux-x86_64.zip"; \
    unzip -o /tmp/protoc.zip -d /usr/local; \
    rm /tmp/protoc.zip; \
    protoc --version
