#!/usr/bin/env bash
# Build Corvus Sweep for another platform in Docker.
#   app/build/build.sh windows     -> app/target/x86_64-pc-windows-gnu/release/corvus-sweep.exe
#                                     (and corvus-sweep-stripped.exe, without debug symbols, to ship)
# (Linux: plain `cargo build --release` in app/.)
set -euo pipefail
B=$(cd "$(dirname "$0")" && pwd)
APP=$(cd "$B/.." && pwd)
WHAT=${1:-windows}
case "$WHAT" in
  windows)
    image=corvus-app-mingw
    docker image inspect "$image" >/dev/null 2>&1 || docker build -q -t "$image" -f "$B/Dockerfile.windows" "$B" >/dev/null
    # the crate registry is cached under target/ (gitignored) so rebuilds don't download everything again
    mkdir -p "$APP/target/docker-cargo"
    docker run --rm -u "$(id -u):$(id -g)" -e CARGO_HOME=/app/target/docker-cargo \
      -v "$APP":/app -w /app "$image" \
      bash -ec 'cargo build --release --locked --target x86_64-pc-windows-gnu
        x86_64-w64-mingw32-strip -o target/x86_64-pc-windows-gnu/release/corvus-sweep-stripped.exe target/x86_64-pc-windows-gnu/release/corvus-sweep.exe'
    ls -l "$APP/target/x86_64-pc-windows-gnu/release/corvus-sweep-stripped.exe"
    ;;
  *) echo "usage: $0 windows" >&2; exit 2 ;;
esac
