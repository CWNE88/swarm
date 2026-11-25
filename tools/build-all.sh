#!/bin/bash
# Builds every release binary into target/release/, the one folder they're
# looked for in:
#   swarm          x86-64 Linux
#   swarm-arm      64-bit ARM Linux (the Raspberry Pi 4)
#   swarm.exe      Windows 10 and later (tools/build-windows.sh)
#   swarm.apk      the Android app (tools/build-android.sh)
# then, with INSTALL=1, puts the app on the phone adb is connected to
# (`adb install -r`, which ends the app if it's running there).
# Usage: tools/build-all.sh               all of them
#        tools/build-all.sh linux arm     only those (linux, arm, windows,
#                                         android)
#
# The targets are built at once, each in a build folder of its own
# (target/ for Linux, target/build-<name> for the others): rustc takes
# one core for much of a big crate, so one target after another leaves
# most of the machine idle, and builds sharing a folder wait on each
# other's lock. Each one's output goes to target/build-logs/<name>.log;
# at the end, which failed (with the end of its log), and each binary
# with the time it was built.
set -uo pipefail
cd "$(dirname "$0")/.."
WANTED=${*:-linux arm windows android}
wants() { [[ " $WANTED " == *" $1 "* ]]; }
. tools/remap.sh
LOGS=target/build-logs
mkdir -p "$LOGS" target/release

build() {
  case $1 in
    linux) cargo build --release "$(remap_paths x86_64-unknown-linux-gnu)" ;;
    arm)
      CARGO_TARGET_DIR=target/build-arm cargo build --release --target aarch64-unknown-linux-gnu \
        "$(remap_paths aarch64-unknown-linux-gnu)" &&
        cp target/build-arm/aarch64-unknown-linux-gnu/release/swarm target/release/swarm-arm ;;
    windows) CARGO_TARGET_DIR=target/build-windows tools/build-windows.sh ;;
    android) CARGO_TARGET_DIR=target/build-android tools/build-android.sh ;;
  esac
}

# The Linux binaries must run on glibc 2.36 (Debian 12) and older, not
# only on the build machine's: none may ask for a newer glibc version
# (cli/src/glibc.rs keeps std's pidfd functions from doing so).
old_enough() {
  local bin
  case $1 in linux) bin=swarm ;; arm) bin=swarm-arm ;; *) return 0 ;; esac
  local newest
  newest=$(objdump -T "target/release/$bin" | grep -o 'GLIBC_2\.[0-9]*' | sort -Vu | tail -1)
  if [ "$(printf '%s\n' "$newest" GLIBC_2.36 | sort -V | tail -1)" != GLIBC_2.36 ]; then
    echo "== $1: needs $newest, newer than glibc 2.36:"
    objdump -T "target/release/$bin" | grep "$newest"
    return 1
  fi
}

declare -A PIDS
for name in linux arm windows android; do
  if wants $name; then
    echo "== $name (log: $LOGS/$name.log)"
    build $name > "$LOGS/$name.log" 2>&1 &
    PIDS[$name]=$!
  fi
done

FAILED=()
for name in linux arm windows android; do
  [ -n "${PIDS[$name]:-}" ] || continue
  if wait "${PIDS[$name]}"; then
    echo "== $name: built"
    old_enough "$name" || FAILED+=("$name")
  else
    echo "== $name: FAILED"
    tail -20 "$LOGS/$name.log"
    FAILED+=("$name")
  fi
done

if [ -n "${PIDS[android]:-}" ] && [[ " ${FAILED[*]} " != *" android "* ]] && [ "${INSTALL:-0}" = 1 ]; then
  adb install -r target/release/swarm.apk
fi

echo "== built"
for f in swarm swarm-arm swarm.exe swarm.apk; do
  [ -e "target/release/$f" ] && ls -l --time-style=+%Y-%m-%d_%H:%M "target/release/$f" | awk '{print $6, $7}'
done
[ ${#FAILED[@]} -eq 0 ] || { echo "failed: ${FAILED[*]}"; exit 1; }
