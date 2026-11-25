#!/bin/bash
# Slint's text input view (vendor/i-slint-backend-android-activity) tested on
# the phone over adb: its Java and the test built into a dex, run there with
# app_process. Prints PASSED or FAILED.
set -e
cd "$(dirname "$0")/.."
SDK=${ANDROID_HOME:-$HOME/Android/Sdk}
JAR=$(ls -d "$SDK"/platforms/android-*/android.jar | sort -V | tail -1)
D8=$(ls -d "$SDK"/build-tools/*/d8 | sort -V | tail -1)
V=vendor/i-slint-backend-android-activity
OUT=target/android-input-test
rm -rf "${OUT:?}"
mkdir -p "$OUT/classes"
javac -nowarn -source 8 -target 8 -encoding UTF-8 -cp "$JAR" -d "$OUT/classes" \
    "$V/java/SlintAndroidJavaHelper.java" "$V/test/InputViewTest.java" 2>&1 | grep -v "^warning\|^Note\|^[0-9]* warning" || true
"$D8" --min-api 28 --lib "$JAR" --output "$OUT" $(find "$OUT/classes" -name '*.class')
adb push "$OUT/classes.dex" /data/local/tmp/input-test.dex >/dev/null
adb shell CLASSPATH=/data/local/tmp/input-test.dex app_process / InputViewTest
