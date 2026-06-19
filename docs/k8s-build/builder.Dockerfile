# Build environment for pelagos: the Rust toolchain plus a recent protoc.
#
# Why a custom image: pelagos-cri compiles the CRI proto with tonic/prost, which
# needs `protoc` — and recent enough to understand the `debug_redact` option used
# in the Kubernetes CRI api.proto. Debian/Ubuntu apt's protoc (3.21) is TOO OLD;
# you need protoc >= 22 (this uses 34.1). Baking it into an image keeps it a
# pinned, reproducible dependency instead of something each build re-downloads.
#
# Build + push to your own registry, then reference it from build-job.yaml.
# (Built here with `docker`, but `pelagos build` works too.)
FROM rust:1-bookworm

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
