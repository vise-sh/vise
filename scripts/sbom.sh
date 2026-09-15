#!/usr/bin/env bash
# Generate a CycloneDX SBOM for every binary archive published on a GitHub
# Release, so downstream consumers can feed them into their own vulnerability
# scanners.
#
# dist runs this from the workspace root in the build-global-artifacts job of
# .github/workflows/release.yml (see `[[dist.extra-artifacts]]` in
# dist-workspace.toml) and uploads the files it lists there to the release
# alongside the archives. `just sbom` runs it locally.
#
# One SBOM is written per (app, target) pair to target/sbom/, named after the
# release archive it describes:
#
#   vise-cli-x86_64-unknown-linux-gnu.tar.xz  ->  vise-cli-x86_64-unknown-linux-gnu.cdx.json
#
# The SBOM lists the dependency graph resolved from Cargo.lock for that target
# (default features, build dependencies included), which is what dist compiles
# into the archive.
set -euo pipefail

# Pinned so releases are reproducible; bump deliberately. Prebuilt binaries
# and the installer come from the tool's own GitHub Releases.
CARGO_CYCLONEDX_VERSION="0.5.9"

# The dist-able packages and the release targets. Keep both in sync with
# dist-workspace.toml (`targets`) and with the `artifacts` list there.
# vise-server is excluded on purpose: it is `publish = false`, so dist does
# not release it; it ships as a container image instead.
APPS=(vise-cli vise-host)
TARGETS=(aarch64-apple-darwin aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu)

OUT_DIR="target/sbom"

cd "$(dirname "$0")/.."

if ! cargo cyclonedx --version >/dev/null 2>&1; then
    if [ -z "${CI:-}" ]; then
        echo "cargo-cyclonedx not found; install it with:" >&2
        echo "  cargo install cargo-cyclonedx --locked --version $CARGO_CYCLONEDX_VERSION" >&2
        exit 1
    fi
    # Same pattern release.yml uses to install dist itself: a pinned,
    # prebuilt release rather than a multi-minute `cargo install`.
    echo "Installing cargo-cyclonedx $CARGO_CYCLONEDX_VERSION"
    curl --proto '=https' --tlsv1.2 -LsSf \
        "https://github.com/CycloneDX/cyclonedx-rust-cargo/releases/download/cargo-cyclonedx-${CARGO_CYCLONEDX_VERSION}/cargo-cyclonedx-installer.sh" \
        | sh
    export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
fi

# Make the SBOM timestamp follow the commit rather than the wall clock.
if [ -z "${SOURCE_DATE_EPOCH:-}" ] && git rev-parse --git-dir >/dev/null 2>&1; then
    SOURCE_DATE_EPOCH="$(git log -1 --format=%ct)"
    export SOURCE_DATE_EPOCH
fi

rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"

for target in "${TARGETS[@]}"; do
    # cargo-cyclonedx always walks the whole workspace (--manifest-path does
    # not narrow it to one package) and writes <bin>_bin_<target>.cdx.json
    # next to each package's Cargo.toml.
    cargo cyclonedx \
        --format json \
        --spec-version 1.5 \
        --describe binaries \
        --target "$target" \
        --target-in-filename

    for app in "${APPS[@]}"; do
        mv "bins/${app}/${app}_bin_${target}.cdx.json" "${OUT_DIR}/${app}-${target}.cdx.json"
    done
    # Drop the SBOMs for binaries that are not published on the release.
    rm -f bins/*/*_bin_"${target}".cdx.json
done

echo "SBOMs written to ${OUT_DIR}/:"
ls -1 "$OUT_DIR"
