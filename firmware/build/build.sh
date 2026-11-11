#!/usr/bin/env bash
# Build the Corvus firmware for one board inside the GCC 4.7-2013q3 container.
#   firmware/build/build.sh [board]        boards: rfone (default), airspy (Airspy R2 and Mini)
# Output: firmware/build/out/<board>/corvus-fw-<version>-<board>.bin, plus the .elf, .map and log.
set -euo pipefail
B=$(cd "$(dirname "$0")" && pwd)
BOARD=${1:-rfone}
SRC=$B/../$BOARD
case "$BOARD" in
  rfone)  IMG=fw_bin/hydrasdr_rfone_fw; HDR=m0/m0.hdr ;;
  airspy) IMG=airspy_rom_to_ram/airspy_rom_to_ram; HDR=airspy_m0/airspy_m0.hdr ;;
  *) echo "unknown board: $BOARD (have: rfone, airspy)" >&2; exit 1 ;;
esac
WORK=$B/work/$BOARD
OUT=$B/out/$BOARD
IMAGE=corvus-fw-gcc47
TC=gcc-arm-none-eabi-4_7-2013q3-20130916-linux.tar.bz2
TC_URL=https://launchpad.net/gcc-arm-embedded/4.7/4.7-2013-q3-update/+download/$TC
# The Internet Archive's copy of the same file (identical MD5), for when Launchpad is unreachable.
TC_MIRROR=https://web.archive.org/web/2020id_/$TC_URL
TC_MD5=c35b662e371f369619cf202692a4d10b

if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  mkdir -p "$B/dl"
  [ -f "$B/dl/$TC" ] || curl -fL --retry 2 --connect-timeout 20 -o "$B/dl/$TC" "$TC_URL" ||
    curl -fL --retry 3 --connect-timeout 30 -o "$B/dl/$TC" "$TC_MIRROR"
  echo "$TC_MD5  $B/dl/$TC" | md5sum -c --quiet -
  LOG=$(mktemp)
  docker build --progress=plain -t "$IMAGE" -f "$B/Dockerfile" "$B/dl" > "$LOG" 2>&1 || { tail -40 "$LOG"; exit 1; }
  rm -f "$LOG"
fi

rm -rf "$WORK"; mkdir -p "$WORK"
(cd "$SRC" && tar -cf - .) | (cd "$WORK" && tar -xf -)
docker run --rm -u "$(id -u):$(id -g)" -v "$WORK":/src -w /src "$IMAGE" bash -ec '
  arm-none-eabi-gcc --version | head -1
  make -C libopencm3 -j"$(nproc)" > libopencm3-build.log 2>&1 || { tail -30 libopencm3-build.log; exit 1; }
  make -j"$(nproc)" > fw-build.log 2>&1 || { tail -40 fw-build.log; exit 1; }
'
VER=$(tr -d '\r' < "$SRC/$HDR" | sed -n 's/.*_GIT_TAG "\(.*\)".*/\1/p')
NAME=corvus-fw-$VER-$BOARD
rm -rf "$OUT"; mkdir -p "$OUT"
cp "$WORK/$IMG.bin" "$OUT/$NAME.bin"
cp "$WORK/$IMG.elf" "$OUT/$NAME.elf"
cp "$WORK/$IMG.map" "$OUT/$NAME.map"
cp "$WORK"/fw-build.log "$OUT"/
(cd "$OUT" && sha256sum "$NAME.bin" | tee SHA256SUMS)
echo "$(stat -c %s "$OUT/$NAME.bin") bytes -> $OUT/$NAME.bin"
