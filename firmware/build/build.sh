#!/usr/bin/env bash
# Build the Corvus firmware for one board inside the GCC 4.7-2013q3 container.
#   firmware/build/build.sh [board]        boards: rfone (default)
# Output: firmware/build/out/<board>/corvus-fw-<version>-<board>.bin, plus the .elf, .map and log.
set -euo pipefail
B=$(cd "$(dirname "$0")" && pwd)
BOARD=${1:-rfone}
SRC=$B/../$BOARD
[ -f "$SRC/Makefile" ] || { echo "unknown board: $BOARD (have: rfone)" >&2; exit 1; }
WORK=$B/work/$BOARD
OUT=$B/out/$BOARD
IMAGE=corvus-fw-gcc47
TC=gcc-arm-none-eabi-4_7-2013q3-20130916-linux.tar.bz2
TC_URL=https://launchpad.net/gcc-arm-embedded/4.7/4.7-2013-q3-update/+download/$TC
TC_MD5=c35b662e371f369619cf202692a4d10b

if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  mkdir -p "$B/dl"
  [ -f "$B/dl/$TC" ] || curl -fL --retry 3 -o "$B/dl/$TC" "$TC_URL"
  echo "$TC_MD5  $B/dl/$TC" | md5sum -c --quiet -
  docker build -q -t "$IMAGE" -f "$B/Dockerfile" "$B/dl" >/dev/null
fi

rm -rf "$WORK"; mkdir -p "$WORK"
(cd "$SRC" && tar -cf - .) | (cd "$WORK" && tar -xf -)
docker run --rm -u "$(id -u):$(id -g)" -v "$WORK":/src -w /src "$IMAGE" bash -ec '
  arm-none-eabi-gcc --version | head -1
  make -C libopencm3 -j"$(nproc)" > libopencm3-build.log 2>&1 || { tail -30 libopencm3-build.log; exit 1; }
  make -j"$(nproc)" > fw-build.log 2>&1 || { tail -40 fw-build.log; exit 1; }
'
VER=$(sed -n 's/.*_GIT_TAG "\(.*\)".*/\1/p' "$SRC/m0/m0.hdr")
NAME=corvus-fw-$VER-$BOARD
rm -rf "$OUT"; mkdir -p "$OUT"
cp "$WORK"/fw_bin/hydrasdr_rfone_fw.bin "$OUT/$NAME.bin"
cp "$WORK"/fw_bin/hydrasdr_rfone_fw.elf "$OUT/$NAME.elf"
cp "$WORK"/fw_bin/hydrasdr_rfone_fw.map "$OUT/$NAME.map"
cp "$WORK"/fw-build.log "$OUT"/
(cd "$OUT" && sha256sum "$NAME.bin" | tee SHA256SUMS)
echo "$(stat -c %s "$OUT/$NAME.bin") bytes -> $OUT/$NAME.bin"
