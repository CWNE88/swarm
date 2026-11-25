#!/bin/bash
# Builds the swarm app for Android phones (64-bit ARM, Android 8 and
# later): target/release/swarm.apk, from android/, installed with
#     adb install -r target/release/swarm.apk
# Needs the SDK (ANDROID_HOME, or ~/Android/Sdk) with a platform and its
# build tools, the NDK (ANDROID_NDK_HOME, or the newest in the SDK), a JDK,
# cargo-ndk and the Rust target:
#     rustup target add aarch64-linux-android
#     cargo install cargo-ndk
# The app's signed with the release key: the keystore at
# SWARM_RELEASE_KEYSTORE (default ~/.swarm-release/swarm.keystore, kept
# out of the repository and backed up: Android only installs an update
# signed with the same key), its password asked for, or taken from
# SWARM_RELEASE_KEYSTORE_PASS (needed under tools/build-all.sh, which
# runs this with no terminal). Make it once with
#     keytool -genkeypair -keystore ~/.swarm-release/swarm.keystore \
#       -alias swarm -keyalg RSA -keysize 4096 -validity 36500
# SWARM_DEV_APK=1 makes a debuggable development build instead, signed
# with the SDK's debug key (~/.android/debug.keystore, made if missing).
#
# As for Windows, the Opus library's own build (inside the audiopus_sys
# crate) can't cross-compile, so it's built here first — once, into
# target/android-deps — and the crate is pointed at it.
#
# Cargo builds in CARGO_TARGET_DIR if it's set (tools/build-all.sh gives
# each target one of its own), target/ otherwise.
set -euo pipefail
cd "$(dirname "$0")/.."
. tools/remap.sh
DEPS=$PWD/target/android-deps
OUT=${CARGO_TARGET_DIR:-target}
API=28
HOST=aarch64-linux-android

export ANDROID_HOME=${ANDROID_HOME:-$HOME/Android/Sdk}
if [ -z "${ANDROID_NDK_HOME:-}" ]; then
  ANDROID_NDK_HOME=$(ls -d "$ANDROID_HOME"/ndk/*/ 2>/dev/null | sort -V | tail -1)
  ANDROID_NDK_HOME=${ANDROID_NDK_HOME%/}
fi
if [ ! -d "$ANDROID_NDK_HOME" ]; then
  echo "the Android NDK is needed: set ANDROID_NDK_HOME" >&2
  exit 1
fi
export ANDROID_NDK_HOME
BIN=$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin
# The newest platform and build tools there are (the oldest Android swarm
# runs on is API, below). Slint's Android backend builds its own Java with
# them too.
PLATFORM=$(ls -d "$ANDROID_HOME"/platforms/android-*/ | sort -V | tail -1)android.jar
TOOLS=$(ls -d "$ANDROID_HOME"/build-tools/*/ | sort -V | tail -1)
export ANDROID_JAR=$PLATFORM ANDROID_D8_JAR=${TOOLS}lib/d8.jar

if [ ! -f "$DEPS/lib/libopus.a" ]; then
  SRC=$(ls -d ~/.cargo/registry/src/*/audiopus_sys-*/opus | head -1)
  rm -rf "$DEPS/opus-build" && mkdir -p "$DEPS" && cp -r "$SRC" "$DEPS/opus-build"
  (
    cd "$DEPS/opus-build"
    sh autogen.sh > /dev/null
    CC="$BIN/$HOST$API-clang" AR="$BIN/llvm-ar" RANLIB="$BIN/llvm-ranlib" CFLAGS="-O2 -fPIC" \
      ./configure --host=$HOST --prefix="$DEPS" --enable-static --disable-shared \
      --disable-doc --disable-extra-programs > /dev/null
    make -j"$(nproc)" > /dev/null
    make install > /dev/null
  )
  echo "built Opus for Android into $DEPS"
fi

# The C++ library (for the OpenH264 video codec) linked in, so the one
# file runs on its own (its ABI half is a library of its own here).
export OPUS_LIB_DIR="$DEPS/lib" OPUS_STATIC=1 CXXSTDLIB_aarch64_linux_android=c++_static \
  CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS="-C link-arg=-lc++abi"
cargo ndk -t arm64-v8a -P $API build --release -p swarm-android "$(remap_paths aarch64-linux-android)" "$@"

# The app: its Java (the activity and the service) as dex, the manifest,
# and the library, signed.
APP=$PWD/target/android-app
rm -rf "${APP:?}" && mkdir -p "$APP/classes" "$APP/apk/lib/arm64-v8a"
javac -encoding UTF-8 --release 11 -nowarn -classpath "$PLATFORM" -d "$APP/classes" android/java/com/cwne88/swarm/*.java
"$TOOLS"d8 --release --min-api $API --lib "$PLATFORM" --output "$APP/apk" $(find "$APP/classes" -name '*.class')
"$BIN/llvm-strip" -o "$APP/apk/lib/arm64-v8a/libswarm.so" "$OUT/aarch64-linux-android/release/libswarm.so"
# A development build is debuggable: `adb shell run-as com.cwne88.swarm …`
# runs as the app, with only what Android lets it do.
if [ -n "${SWARM_DEV_APK:-}" ]; then DEBUG=--debug-mode; else DEBUG=; fi
"$TOOLS"aapt2 link $DEBUG -o "$APP/unsigned.apk" --manifest android/AndroidManifest.xml -I "$PLATFORM"
(cd "$APP/apk" && zip -qr "$APP/unsigned.apk" classes.dex lib)
"$TOOLS"zipalign -p -f 4 "$APP/unsigned.apk" "$APP/aligned.apk"
if [ -n "${SWARM_DEV_APK:-}" ]; then
  KEY=$HOME/.android/debug.keystore
  if [ ! -f "$KEY" ]; then
    mkdir -p "$(dirname "$KEY")"
    keytool -genkeypair -keystore "$KEY" -storepass android -keypass android -alias androiddebugkey \
      -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=Android Debug,O=Android,C=US" > /dev/null
  fi
  "$TOOLS"apksigner sign --ks "$KEY" --ks-pass pass:android --out target/release/swarm.apk "$APP/aligned.apk"
else
  KEY=${SWARM_RELEASE_KEYSTORE:-$HOME/.swarm-release/swarm.keystore}
  if [ ! -f "$KEY" ]; then
    echo "no release keystore at $KEY: make one (see the top of this script), or set SWARM_DEV_APK=1" >&2
    exit 1
  fi
  if [ -n "${SWARM_RELEASE_KEYSTORE_PASS:-}" ]; then PASS=env:SWARM_RELEASE_KEYSTORE_PASS; else PASS=stdin; fi
  "$TOOLS"apksigner sign --ks "$KEY" --ks-pass "$PASS" --out target/release/swarm.apk "$APP/aligned.apk"
fi
echo "target/release/swarm.apk"
