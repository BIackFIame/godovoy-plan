#!/bin/sh
# Run both production checklist decoders against the same Rust-generated snapshots.
set -eu
mobile=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
: "${ANDROID_HOME:?Set ANDROID_HOME to the Android SDK}"
: "${JAVA_HOME:?Set JAVA_HOME to a JDK}"
: "${ANDROID_SERIAL:?Set ANDROID_SERIAL to a running test emulator}"
case "$ANDROID_SERIAL" in emulator-*) ;; *) echo "Select a test emulator." >&2; exit 2;; esac
adb="$ANDROID_HOME/platform-tools/adb"
platform="$ANDROID_HOME/platforms/android-${ANDROID_COMPILE_SDK:-34}/android.jar"
d8="$ANDROID_HOME/build-tools/${ANDROID_BUILD_TOOLS:-35.0.0}/lib/d8.jar"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/godovoy-contract.XXXXXXXX")
remote=
cleanup() {
    if [ -n "$remote" ]; then "$adb" -s "$ANDROID_SERIAL" shell rm -r "$remote" >/dev/null; fi
    rm -rf "$tmp"
}
trap cleanup EXIT
GP_CHECKLIST_FIXTURE="$tmp/cases.json" "${CARGO:-cargo}" test --locked --manifest-path "$mobile/core/Cargo.toml" --lib emit_native_checklist_fixture -- --ignored
swiftc -module-cache-path "$tmp/swift-cache" "$mobile/ios/App/Models.swift" "$mobile/ios/ContractTests/ChecklistFixture.swift" -o "$tmp/swift-check"
"$tmp/swift-check" "$tmp/cases.json"
main="$mobile/android/src/main/java/local/godovoyplan/mobile"
test="$mobile/android/src/test/java/local/godovoyplan/mobile"
mkdir "$tmp/classes"
"$JAVA_HOME/bin/javac" --release 8 -Xlint:-options -classpath "$platform" -d "$tmp/classes" "$main/ChecklistUpdate.java" "$main/ChecklistRows.java" "$main/MobileLimits.java" "$main/SyncStamp.java" "$test/ChecklistUpdateTest.java"
"$JAVA_HOME/bin/jar" cf "$tmp/classes.jar" -C "$tmp/classes" .
"$JAVA_HOME/bin/java" -cp "$d8" com.android.tools.r8.D8 --min-api 26 --lib "$platform" --output "$tmp/test.jar" "$tmp/classes.jar"
remote=$("$adb" -s "$ANDROID_SERIAL" shell mktemp -d /data/local/tmp/godovoy-contract.XXXXXXXX)
"$adb" -s "$ANDROID_SERIAL" push "$tmp/test.jar" "$tmp/cases.json" "$remote/" >/dev/null
"$adb" -s "$ANDROID_SERIAL" shell chmod 0444 "$remote/test.jar"
"$adb" -s "$ANDROID_SERIAL" shell "CLASSPATH='$remote/test.jar' app_process / local.godovoyplan.mobile.ChecklistUpdateTest '$remote/cases.json'"
