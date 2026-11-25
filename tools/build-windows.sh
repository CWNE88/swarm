#!/bin/bash
# Builds target/release/swarm.exe for Windows (10 and later) from Linux.
# Needs MinGW-w64 and the Rust target (see .cargo/config.toml).
#
# The Opus library's own build (inside the audiopus_sys crate) can't
# cross-compile, so it's built here first — once, into
# target/windows-deps — and the crate is pointed at it. Without its
# "hardening" (fortified memcpy and friends), which MinGW only has with an
# extra library.
# Cargo builds in CARGO_TARGET_DIR if it's set (tools/build-all.sh gives
# each target one of its own), target/ otherwise.
set -euo pipefail
cd "$(dirname "$0")/.."
. tools/remap.sh
DEPS=$PWD/target/windows-deps
OUT=${CARGO_TARGET_DIR:-target}
HOST=x86_64-w64-mingw32

# Without nasm, openh264 quietly builds without its assembly, and video
# encoding takes 2.6 times as long.
if ! command -v nasm > /dev/null; then
  echo "nasm is needed (apt install nasm), or video will be slow" >&2
  exit 1
fi

if [ ! -f "$DEPS/lib/libopus.a" ]; then
  SRC=$(ls -d ~/.cargo/registry/src/*/audiopus_sys-*/opus | head -1)
  rm -rf "$DEPS/opus-build" && mkdir -p "$DEPS" && cp -r "$SRC" "$DEPS/opus-build"
  (
    cd "$DEPS/opus-build"
    sh autogen.sh > /dev/null
    CFLAGS="-O2 -U_FORTIFY_SOURCE -D_FORTIFY_SOURCE=0" ./configure --host=$HOST --prefix="$DEPS" --enable-static --disable-shared \
      --disable-doc --disable-extra-programs --disable-hardening > /dev/null
    make -j"$(nproc)" > /dev/null
    make install > /dev/null
  )
  echo "built Opus for Windows into $DEPS"
fi

OPUS_LIB_DIR="$DEPS/lib" OPUS_STATIC=1 cargo build --release --target x86_64-pc-windows-gnu \
  "$(remap_paths x86_64-pc-windows-gnu)" "$@"
cp "$OUT/x86_64-pc-windows-gnu/release/swarm.exe" target/release/swarm.exe
echo "target/release/swarm.exe"
