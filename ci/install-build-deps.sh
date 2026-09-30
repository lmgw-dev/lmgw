#!/usr/bin/env bash
# Install the Rust toolchain + Tauri v2 Linux build dependencies on Fedora, plus
# the `cargo tauri` CLI. The CLI and cargo caches persist via $CARGO_HOME across
# pipelines (see .gitlab-ci.yml cache).
#
# Usage: bash ci/install-build-deps.sh   (run as root inside the fedora image)
set -euo pipefail

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
if ! command -v cargo-tauri >/dev/null 2>&1; then
  cargo install tauri-cli --version '^2.0' --locked
fi
cargo tauri --version

# Trunk builds the Leptos UI (crates/lmgw-ui) into the dist/ that lmgw-core
# embeds. Install the upstream prebuilt binary instead of `cargo install trunk`:
# from source it pulls libdeflate-sys (via zip), whose vendored C uses the
# `evex512` target attribute that Fedora's current GCC (16) no longer accepts,
# so the install dies before the UI is ever built. The binary also saves several
# minutes on a cold cache. Bump TRUNK_VERSION and TRUNK_SHA256 together — the
# hash is the one upstream publishes next to the tarball as .sha256.
TRUNK_VERSION="0.21.14"
TRUNK_SHA256="f2b4680cd239693a646a2795e4633c625328d7b2a044fbe749fa3a2fe9e7036b"

# Version-checked, not just presence-checked: the binary lands in the cached
# $CARGO_HOME/bin, so a plain `command -v` would pin CI to whatever version a
# past pipeline happened to install and silently ignore a bump here.
if [[ "$(trunk --version 2>/dev/null || true)" != "trunk ${TRUNK_VERSION}" ]]; then
  url="https://github.com/trunk-rs/trunk/releases/download/v${TRUNK_VERSION}/trunk-x86_64-unknown-linux-gnu.tar.gz"
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  curl -fsSL "$url" -o "$tmp/trunk.tar.gz"
  echo "${TRUNK_SHA256}  $tmp/trunk.tar.gz" | sha256sum -c -
  tar -xzf "$tmp/trunk.tar.gz" -C "$tmp" trunk
  install -D -m 0755 "$tmp/trunk" "${CARGO_HOME:-$HOME/.cargo}/bin/trunk"
fi
trunk --version
