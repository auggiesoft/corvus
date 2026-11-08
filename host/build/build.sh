#!/usr/bin/env bash
# Build host/hydrasdr-host (libhydrasdr with the Corvus sweep API, plus the tools) for Linux and/or Windows.
#   host/build/build.sh [linux|windows|all]          (default: all)
# Artefacts: host/build/out/linux/ (libhydrasdr.so*, tools) and host/build/out/windows/ (DLLs, exes).
set -euo pipefail
B=$(cd "$(dirname "$0")" && pwd)
SRC=$B/../hydrasdr-host
WHAT=${1:-all}

build() { # <target> <image> <dockerfile> <configure+build commands>
  local target=$1 image=$2 df=$3 cmds=$4 work=$B/work/$1 out=$B/out/$1
  docker image inspect "$image" >/dev/null 2>&1 || docker build -q -t "$image" -f "$B/$df" "$B" >/dev/null
  case "$work" in "$B"/work/?*) rm -rf "$work" ;; *) exit 1 ;; esac
  mkdir -p "$work"
  (cd "$SRC" && tar --exclude=./.git -cf - .) | (cd "$work" && tar -xf -)
  docker run --rm -u "$(id -u):$(id -g)" -v "$work":/src -w /src "$image" bash -ec "$cmds" \
    > "$work/build.log" 2>&1 || { tail -40 "$work/build.log"; exit 1; }
  rm -rf "$out"; mkdir -p "$out"
  cp -a "$work"/stage/. "$out"/
  cp "$work/build.log" "$out"/
  echo "$target -> $out"
}

if [ "$WHAT" = linux ] || [ "$WHAT" = all ]; then
  build linux corvus-host-linux Dockerfile.linux '
    cmake -S . -B b -DCMAKE_BUILD_TYPE=Release -DINSTALL_UDEV_RULES=OFF
    cmake --build b -j"$(nproc)"
    mkdir -p stage
    cp -a b/libhydrasdr/src/libhydrasdr.so* stage/
    find b/hydrasdr-tools -maxdepth 2 -type f -executable -name "hydrasdr_*" -exec cp {} stage/ \;
    cp libhydrasdr/src/hydrasdr.h libhydrasdr/src/hydrasdr_commands.h stage/'
  (cd "$B/out/linux" && sha256sum libhydrasdr.so.*.* | tee SHA256SUMS)
fi
if [ "$WHAT" = windows ] || [ "$WHAT" = all ]; then
  M=/usr/x86_64-w64-mingw32/sys-root/mingw
  build windows corvus-host-mingw Dockerfile.mingw "
    mingw64-cmake -S . -B b -DCMAKE_BUILD_TYPE=Release -DINSTALL_UDEV_RULES=OFF -DLIBUSB_DLL=$M/bin/libusb-1.0.dll
    cmake --build b -j\"\$(nproc)\"
    mkdir -p stage
    find b -name '*.dll' -exec cp {} stage/ \;
    cp b/libhydrasdr/src/libhydrasdr.dll.a stage/
    find b -name 'hydrasdr_*.exe' -exec cp {} stage/ \;
    cp $M/bin/libusb-1.0.dll stage/
    cp libhydrasdr/src/hydrasdr.h libhydrasdr/src/hydrasdr_commands.h stage/
    cp stage/libhydrasdr.dll stage/hydrasdr.dll   # MSVC release name, which most apps load
    x86_64-w64-mingw32-objdump -p stage/libhydrasdr.dll | grep -E 'DLL Name' > stage/libhydrasdr.dll.imports"
  (cd "$B/out/windows" && sha256sum *.dll | tee SHA256SUMS)
fi
