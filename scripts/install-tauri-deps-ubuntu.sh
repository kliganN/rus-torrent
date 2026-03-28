#!/usr/bin/env bash

set -euo pipefail

if [[ "${EUID}" -ne 0 ]]; then
  echo "Run this script with sudo:"
  echo "  sudo ./scripts/install-tauri-deps-ubuntu.sh"
  exit 1
fi

apt-get update
apt-get install -y \
  build-essential \
  curl \
  wget \
  file \
  libssl-dev \
  libgtk-3-dev \
  libwebkit2gtk-4.1-dev \
  libayatana-appindicator3-dev \
  librsvg2-dev \
  libxdo-dev

echo
echo "Tauri desktop prerequisites were installed."
echo "Next step:"
echo "  cargo run --manifest-path src-tauri/Cargo.toml"
