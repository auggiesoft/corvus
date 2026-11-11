#!/usr/bin/env bash
# Build corvus-pluto for the board (ARMv7, static musl): firmware/pluto/target/armv7-unknown-linux-musleabihf/release/corvus-pluto
set -euo pipefail
cd "$(dirname "$0")"
T=armv7-unknown-linux-musleabihf
rustup target list --installed | grep -qx "$T" || rustup target add "$T"
cargo build --release --locked --target "$T"
ls -l "target/$T/release/corvus-pluto"
