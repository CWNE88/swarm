#!/bin/bash
# Builds the native desktop client and packages it as target/release/Swarm.app.
# Needs the macOS Command Line Tools, Rust and the build dependencies in README.md.
# Usage: tools/build-macos.sh
#        tools/build-macos.sh --package-only /path/to/swarm
set -euo pipefail

binary=
if [[ $# -eq 2 && $1 == --package-only ]]; then
  binary=$2
  [[ $binary = /* ]] || binary=$PWD/$binary
elif [[ $# -ne 0 ]]; then
  echo "Usage: $0 [--package-only /path/to/swarm]" >&2
  exit 2
fi

if [[ $(uname -s) != Darwin ]]; then
  echo "Swarm.app must be packaged on macOS." >&2
  exit 1
fi

cd "$(dirname "$0")/.."
output=${CARGO_TARGET_DIR:-target}/release
minimum_supported_os=14.0
if [[ -z $binary ]]; then
  . tools/remap.sh
  host=$(rustc -vV | sed -n 's/^host: //p')
  MACOSX_DEPLOYMENT_TARGET=${MACOSX_DEPLOYMENT_TARGET:-$minimum_supported_os} \
    OPUS_STATIC=1 cargo build --locked --release -p swarm-cli "$(remap_paths "$host")"
  binary=$output/swarm
fi

if ! /usr/bin/otool -hv "$binary" | awk '
  /^MH_MAGIC/ { headers++; if ($5 != "EXECUTE") bad = 1 }
  END { exit !headers || bad }
'; then
  echo "Expected a Mach-O executable: $binary" >&2
  exit 1
fi

min_os=$(/usr/bin/vtool -show-build "$binary" | awk '
  $1 == "cmd" { legacy = ($2 == "LC_VERSION_MIN_MACOSX"); macos = 0 }
  $1 == "platform" { macos = ($2 == "MACOS") }
  (macos && $1 == "minos") || (legacy && $1 == "version") { print $2 }
' | sort -u)
if [[ ! $min_os =~ ^[0-9]+\.[0-9]+(\.[0-9]+)?$ ]]; then
  echo "Expected a macOS executable with one minimum system version: $binary" >&2
  exit 1
fi
if [[ ${min_os%%.*} -lt ${minimum_supported_os%%.*} ]]; then
  min_os=$minimum_supported_os
fi
version=$(awk -F '"' '/^version[[:space:]]*=/ { print $2; exit }' cli/Cargo.toml)

mkdir -p "$output"
# Stage outside synced folders, whose Finder metadata can invalidate a
# freshly signed bundle while it is still being assembled.
stage=$(mktemp -d "${TMPDIR:-/tmp}/swarm-app.XXXXXX")
app=$stage/Swarm.app
bundle=$output/Swarm.app
backup=$stage/previous.app
lock=$output/.swarm-app.lock
locked=0
cleanup() {
  if [[ ( -e $backup || -L $backup ) && ! -e $bundle && ! -L $bundle ]]; then
    if ! mv "$backup" "$bundle"; then
      echo "Previous app is still at $backup" >&2
      rmdir "$lock"
      return 1
    fi
  fi
  rm -rf "$stage"
  if [[ $locked -eq 1 ]]; then
    rmdir "$lock"
  fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
install -m 755 "$binary" "$app/Contents/MacOS/swarm"
install -m 644 LICENCE "$app/Contents/Resources/LICENCE"
cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDisplayName</key>
  <string>Swarm</string>
  <key>CFBundleExecutable</key>
  <string>swarm</string>
  <key>CFBundleIdentifier</key>
  <string>com.cwne88.swarm</string>
  <key>CFBundleName</key>
  <string>Swarm</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>$version</string>
  <key>CFBundleVersion</key>
  <string>$version</string>
  <key>LSMinimumSystemVersion</key>
  <string>$min_os</string>
  <key>NSCameraUsageDescription</key>
  <string>Swarm uses the camera for video calls, previews and scanning Swarm QR codes.</string>
  <key>NSCameraUseContinuityCameraDeviceType</key>
  <true/>
  <key>NSMicrophoneUsageDescription</key>
  <string>Swarm uses the microphone for voice and video calls.</string>
  <key>NSScreenCaptureUsageDescription</key>
  <string>Swarm shares the display you choose with the people in your call.</string>
  <key>NSLocalNetworkUsageDescription</key>
  <string>Swarm uses your local network to find and communicate with nearby peers.</string>
  <key>NSBonjourServices</key>
  <array>
    <string>_swarm._udp</string>
  </array>
</dict>
</plist>
EOF

/usr/bin/plutil -lint "$app/Contents/Info.plist" >/dev/null
# Finder and File Provider can attach metadata to files in Documents.
# This is a freshly generated bundle: remove extended attributes that
# codesign rejects before signing it.
/usr/bin/xattr -cr "$app"
/usr/bin/codesign --force --sign - "$app"
/usr/bin/codesign --verify --strict "$app"

if ! mkdir "$lock"; then
  echo "The app output is locked: $lock" >&2
  exit 1
fi
locked=1
if [[ -e $bundle || -L $bundle ]]; then
  mv "$bundle" "$backup"
fi
mv "$app" "$output"
printf '%s\n' "$bundle"
