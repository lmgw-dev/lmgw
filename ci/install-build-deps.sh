#!/usr/bin/env bash
# Install the Rust toolchain + Tauri v2 Linux build dependencies on Fedora, plus
# the `cargo tauri` CLI. The CLI and cargo caches persist via $CARGO_HOME across
# pipelines (see .gitlab-ci.yml cache).
#
# Usage: bash ci/install-build-deps.sh   (run as root inside the fedora image)
set -euo pipefail

# The list follows Tauri's Fedora prerequisites, which is the only reason
# openssl-devel is on it: nothing in lmgw's build links OpenSSL (its TLS is
# rustls, ort's build-time download included), and neither does tauri-cli with
# its default features.
dnf -y install \
  rust cargo \
  rust-std-static-wasm32-unknown-unknown \
  webkit2gtk4.1-devel \
  gtk3-devel \
  libsoup3-devel \
  libayatana-appindicator-gtk3-devel \
  librsvg2-devel \
  openssl-devel \
  pkgconf-pkg-config \
  rpm-build \
  gcc gcc-c++ make \
  curl wget file git

export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"

# Upstream prebuilt binaries for both tools, pinned and hash-checked. Compiling
# tauri-cli from source took 12 minutes of every cold CI job, and a cold cache is
# the normal case on GitHub, which drops caches unused for a week. Version-checked,
# not just presence-checked: the binaries land in the cached $CARGO_HOME/bin, so
# a plain `command -v` would pin CI to whatever version a past pipeline happened
# to install and silently ignore a bump here. Bump each VERSION and SHA256
# together.
install_prebuilt() { # <name> <want --version output> <url> <sha256>
  if [[ "$("$1" --version 2>/dev/null || true)" != "$2" ]]; then
    local tmp
    tmp="$(mktemp -d)"
    curl -fsSL "$3" -o "$tmp/pkg.tar.gz"
    echo "$4  $tmp/pkg.tar.gz" | sha256sum -c -
    tar -xzf "$tmp/pkg.tar.gz" -C "$tmp" "$1"
    install -D -m 0755 "$tmp/$1" "${CARGO_HOME:-$HOME/.cargo}/bin/$1"
    rm -rf "$tmp"
  fi
  "$1" --version
}

# The tauri-cli release ships no checksum file; this hash was taken from the
# downloaded tarball.
TAURI_CLI_VERSION="2.12.0"
TAURI_CLI_SHA256="8544e19c4312f653e80f92241ee4212b4d928ad33461bfb1d2997c52560c418f"
install_prebuilt cargo-tauri "tauri-cli ${TAURI_CLI_VERSION}" \
  "https://github.com/tauri-apps/tauri/releases/download/tauri-cli-v${TAURI_CLI_VERSION}/cargo-tauri-x86_64-unknown-linux-gnu.tgz" \
  "$TAURI_CLI_SHA256"

# Trunk builds the Leptos UI (crates/lmgw-ui) into the dist/ that lmgw-core
# embeds. From source it pulls libdeflate-sys (via zip), whose vendored C uses
# the `evex512` target attribute that Fedora's current GCC (16) no longer
# accepts, so `cargo install trunk` dies before the UI is ever built. The hash
# is the one upstream publishes next to the tarball as .sha256.
TRUNK_VERSION="0.21.14"
TRUNK_SHA256="f2b4680cd239693a646a2795e4633c625328d7b2a044fbe749fa3a2fe9e7036b"
install_prebuilt trunk "trunk ${TRUNK_VERSION}" \
  "https://github.com/trunk-rs/trunk/releases/download/v${TRUNK_VERSION}/trunk-x86_64-unknown-linux-gnu.tar.gz" \
  "$TRUNK_SHA256"
