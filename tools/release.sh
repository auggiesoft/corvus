#!/usr/bin/env bash
# Build every Corvus binary and package a release into dist/<version>/.
#   tools/release.sh [version]        (default: git describe)
# Needs Docker (firmware and libraries build in containers) and a Rust toolchain (the app).
# GitHub Actions runs this on every v* tag (.github/workflows/release.yml); it works the same locally.
set -euo pipefail
R=$(cd "$(dirname "$0")/.." && pwd)
VER=${1:-$(git -C "$R" describe --tags --always --dirty)}
OUT=$R/dist/$VER
SRC_URL="https://github.com/auggiesoft/corvus/tree/$VER"
case "$OUT" in "$R"/dist/?*) rm -rf "$OUT" ;; *) exit 1 ;; esac
mkdir -p "$OUT"

echo "== firmware"
"$R/firmware/build/build.sh" rfone
"$R/firmware/build/build.sh" airspy
echo "== host libraries"
"$R/host/build/build.sh" all
echo "== Corvus Sweep"
(cd "$R/app" && cargo build --release --locked)

notice() { # what the archive holds and where its source and licences are
  cat <<EOF
Corvus $VER (unofficial; not affiliated with, endorsed by or supported by HydraSDR or Airspy)
Source for everything in this archive: $SRC_URL

Licences:
- Corvus Sweep (corvus-sweep) and Corvus's own code: MIT, see LICENSE. It embeds the Barlow and
  Auggie Mono (IBM Plex Mono based) fonts, SIL Open Font License 1.1: see OFL-*.txt.
- libhydrasdr: MIT, with hydrasdr.c/hydrasdr.h BSD-3-Clause: see LICENSE-libhydrasdr.md.
- libairspy: BSD-3-Clause: see LICENSE-libairspy.txt.
- hydrasdr_* and airspy_* tools: GPL-2.0 (from the HackRF project); source at the link above
  (host/hydrasdr-host/hydrasdr-tools, host/airspyone-host/airspy-tools).
- Firmware images: GPL-2.0-or-later as a whole; source at the link above (firmware/rfone, firmware/airspy).
EOF
}

licences() { # <dir>
  cp "$R/LICENSE" "$1/LICENSE"
  cp "$R/host/hydrasdr-host/libhydrasdr/LICENSE.md" "$1/LICENSE-libhydrasdr.md"
  tr -d '\r' < "$R/host/airspyone-host/libairspy/src/airspy.c" | sed -n '1,/\*\//p' > "$1/LICENSE-libairspy.txt"
  cp "$R"/app/assets/fonts/OFL-*.txt "$1/"
  notice > "$1/NOTICE.txt"
}

echo "== packaging $VER"
# firmware images, as built
cp "$R"/firmware/build/out/rfone/corvus-fw-*-rfone.bin "$OUT/"
cp "$R"/firmware/build/out/airspy/corvus-fw-*-airspy.bin "$OUT/"

# Corvus Sweep for Linux, with both libraries beside it (it loads the one next to itself first)
A=corvus-sweep-$VER-linux-x86_64
S=$OUT.stage
case "$S" in "$R"/dist/?*.stage) rm -rf "$S" ;; *) exit 1 ;; esac
mkdir -p "$S/$A"
cp "$R/app/target/release/corvus-sweep" "$S/$A/"
cp "$R/host/build/out/linux/libhydrasdr.so.1.1.4" "$R/host/build/out/airspy-linux/libairspy.so.1.0.10" "$S/$A/"
cp "$R/app/README.md" "$S/$A/README-corvus-sweep.md"
licences "$S/$A"
tar -C "$S" -czf "$OUT/$A.tar.gz" "$A"

# host libraries and tools for Linux (libhydrasdr + hydrasdr_* tools, libairspy + airspy_* tools)
H=corvus-host-$VER-linux-x86_64
mkdir -p "$S/$H/hydrasdr" "$S/$H/airspy"
cp -a "$R"/host/build/out/linux/. "$S/$H/hydrasdr/"
cp -a "$R"/host/build/out/airspy-linux/. "$S/$H/airspy/"
rm -f "$S/$H"/*/build.log
licences "$S/$H"
tar -C "$S" -czf "$OUT/$H.tar.gz" "$H"

# libhydrasdr for Windows x64 (MinGW build; DLLs, import library, headers, tools)
W=corvus-libhydrasdr-$VER-windows-x64
mkdir -p "$S/$W"
cp -a "$R"/host/build/out/windows/. "$S/$W/"
rm -f "$S/$W/build.log"
licences "$S/$W"
(cd "$S" && python3 -m zipfile -c "$OUT/$W.zip" "$W")

case "$S" in "$R"/dist/?*.stage) rm -rf "$S" ;; esac
(cd "$OUT" && sha256sum -- * | tee SHA256SUMS)
echo "release files in $OUT"
