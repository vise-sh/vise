# syntax=docker/dockerfile:1.7
#
# vise-server container image, published to ghcr.io/vise-sh/vise-server on
# every release (see .github/workflows/publish-docker.yml).
#
# Multi-stage: a Rust builder that cross-compiles for the requested platform,
# then a slim Debian runtime holding only the binary. Cross-compiling (rather
# than emulating the target with QEMU) keeps a
# `docker buildx build --platform linux/amd64,linux/arm64` build fast on any
# host, and a same-arch build takes the same path with the native gcc.
#
#   docker build -t vise-server .
#   docker run --rm -p 3000:3000 -e DATABASE_URL=postgres://... vise-server
#
# The server applies pending migrations from crates/vise-core/migrations on
# startup, so the image needs no sqlx-cli.

# The rust:<version> tag to build with. rust-toolchain.toml tracks stable, so
# bump this whenever the code starts needing a newer compiler.
ARG RUST_VERSION=1.98

# ---------------------------------------------------------------------------
# Builder: runs on the build host's architecture and targets $TARGETPLATFORM.
# ---------------------------------------------------------------------------
FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-bookworm AS builder
ARG TARGETPLATFORM
WORKDIR /src

# Map the Docker platform to a Rust target triple. Debian's cross toolchains
# are only needed when the target differs from the build host; the linker
# names below exist for the native architecture too (`gcc` provides them).
RUN set -eux; \
    case "$TARGETPLATFORM" in \
      linux/amd64) target=x86_64-unknown-linux-gnu;  deb=amd64; gnu=x86-64-linux-gnu ;; \
      linux/arm64) target=aarch64-unknown-linux-gnu; deb=arm64; gnu=aarch64-linux-gnu ;; \
      *) echo "unsupported TARGETPLATFORM: $TARGETPLATFORM" >&2; exit 1 ;; \
    esac; \
    if [ "$deb" != "$(dpkg --print-architecture)" ]; then \
      apt-get update; \
      apt-get install -y --no-install-recommends "gcc-$gnu" "libc6-dev-$deb-cross"; \
      rm -rf /var/lib/apt/lists/*; \
    fi; \
    rustup target add "$target"; \
    echo "$target" > /rust-target

# Linkers for both targets; cargo only consults the entry for the target in
# use. The `cc` crate (aws-lc-sys, ring, ...) finds `<gnu>-gcc` on its own.
RUN mkdir -p .cargo && cat > .cargo/config.toml <<'CARGO'
[target.x86_64-unknown-linux-gnu]
linker = "x86_64-linux-gnu-gcc"

[target.aarch64-unknown-linux-gnu]
linker = "aarch64-linux-gnu-gcc"
CARGO

COPY . .

# The sqlx query macros are checked against the committed .sqlx/ cache; no
# database is reachable at build time.
ENV SQLX_OFFLINE=true

# The `dist` profile is what the release archives are built with. The cache
# mounts make repeated local builds incremental; CI relies on layer caching.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target,id=vise-target-$TARGETPLATFORM \
    set -eux; \
    target="$(cat /rust-target)"; \
    cargo build --locked --profile dist -p vise-server --target "$target"; \
    cp "target/$target/dist/vise-server" /vise-server

# ---------------------------------------------------------------------------
# Runtime: the binary, CA certificates (GitHub API over TLS) and a non-root
# user. No shell tooling beyond what the base image ships.
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates; \
    rm -rf /var/lib/apt/lists/*; \
    useradd --system --uid 10001 --user-group --no-create-home --shell /usr/sbin/nologin vise

COPY --from=builder /vise-server /usr/local/bin/vise-server

USER vise
# The server listens on 0.0.0.0:3000.
EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/vise-server"]
