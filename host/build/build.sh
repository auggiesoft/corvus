#!/usr/bin/env bash
# Build the host libraries with the Corvus sweep API, plus their tools.
#   host/build/build.sh [linux|windows|airspy-linux|all]          (default: all)
#   linux, windows: host/hydrasdr-host (libhydrasdr, for the RFOne) -> host/build/out/linux/, out/windows/
#   airspy-linux:   host/airspyone-host (libairspy, for the Airspy R2) -> host/build/out/airspy-linux/
set -euo pipefail
B=$(cd "$(dirname "$0")" && pwd)
WHAT=${1:-all}

build() { # <target> <source dir> <image> <dockerfile> <configure+build commands>
  local target=$1 SRC=$2 image=$3 df=$4 cmds=$5 work=$B/work/$1 out=$B/out/$1
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
  build linux "$B/../hydrasdr-host" corvus-host-linux Dockerfile.linux '
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
  build windows "$B/../hydrasdr-host" corvus-host-mingw Dockerfile.mingw "
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
if [ "$WHAT" = airspy-linux ] || [ "$WHAT" = all ]; then
  build airspy-linux "$B/../airspyone-host" corvus-host-linux Dockerfile.linux '
    cmake -S . -B b -DCMAKE_BUILD_TYPE=Release -DINSTALL_UDEV_RULES=OFF
    cmake --build b -j"$(nproc)"
    mkdir -p stage
    cp -a b/libairspy/src/libairspy.so* stage/
    find b/airspy-tools -maxdepth 2 -type f -executable -name "airspy_*" -exec cp {} stage/ \;
    cp libairspy/src/airspy.h libairspy/src/airspy_commands.h stage/'
  (cd "$B/out/airspy-linux" && sha256sum libairspy.so.*.* | tee SHA256SUMS)
fi
