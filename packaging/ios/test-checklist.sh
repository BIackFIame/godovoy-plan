#!/bin/bash
# Usage on an Apple Silicon Mac: ./run.sh /absolute/path/to/godovoy-plan
set -euo pipefail
project=$(cd "${1:?Pass the full repository source directory}" && pwd)
suite=$(cd "$(dirname "$0")" && pwd)
stand=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/gp-focus.XXXXXX")
echo "iOS focus test artifacts: $stand"
[[ "$(uname -m)" == arm64 ]]
xcodebuild -showsdks > "$stand/sdks.txt"
export CARGO_TARGET_DIR="$stand/rust"
cargo_exe="${CARGO:-cargo}"
rustup target add aarch64-apple-ios-sim
"$cargo_exe" rustc --lib --release --locked --manifest-path "$project/mobile/core/Cargo.toml" --crate-type cdylib
"$cargo_exe" rustc --lib --release --locked --manifest-path "$project/mobile/core/Cargo.toml" --target aarch64-apple-ios-sim --crate-type staticlib
stage="$stand/project"
mkdir -p "$stage/RustLibs/iphonesimulator"
cp "$CARGO_TARGET_DIR/aarch64-apple-ios-sim/release/libgodovoy_mobile_core.a" "$stage/RustLibs/iphonesimulator/"
for name in App UITests GodovoyPlan.xcodeproj; do ditto "$project/mobile/ios/$name" "$stage/$name"; done
xcrun simctl list -j devices available > "$stand/devices.json"
python3 - "$stand/devices.json" > "$stand/config.txt" <<'PY'
import json, sys
data = json.load(open(sys.argv[1]))["devices"]
candidates = [(tuple(map(int, runtime.split(".iOS-")[-1].split("-"))), runtime, row["deviceTypeIdentifier"])
              for runtime, rows in data.items() if ".iOS-" in runtime
              for row in rows if row.get("isAvailable") and row["name"].startswith("iPhone") and row.get("deviceTypeIdentifier")]
_, runtime, kind = max(candidates)
print(runtime)
print(kind)
PY
runtime=$(sed -n '1p' "$stand/config.txt")
device_type=$(sed -n '2p' "$stand/config.txt")
profile_id=$(uuidgen)
phone=$(xcrun simctl create "GP-focus-${profile_id}" "$device_type" "$runtime")
# Delete only the dedicated simulator that this invocation created.
cleanup() { xcrun simctl shutdown "$phone" >/dev/null 2>&1 || true; xcrun simctl delete "$phone" || true; }
trap cleanup EXIT
xcrun simctl bootstatus "$phone" -b
xcodebuild -quiet -project "$stage/GodovoyPlan.xcodeproj" -scheme GodovoyPlan -configuration Debug \
    -destination "platform=iOS Simulator,id=$phone" -derivedDataPath "$stand/derived" \
    -parallel-testing-enabled NO ARCHS=arm64 build-for-testing
products="$stand/derived/Build/Products"
xcrun simctl install "$phone" "$products/Debug-iphonesimulator/GodovoyPlan.app"
container=$(xcrun simctl get_app_container "$phone" local.godovoyplan.mobile data)
mkdir -p "$stand/fixtures"
python3 "$suite/seed-checklist.py" "$CARGO_TARGET_DIR/release/libgodovoy_mobile_core.dylib" "$stand/fixtures/$profile_id"
profile_parent="$container/Library/Application Support/GodovoyPlan"
mkdir -p "$profile_parent"
test ! -e "$profile_parent/$profile_id"
ditto "$stand/fixtures/$profile_id" "$profile_parent/$profile_id"
python3 - "$products" "$profile_id" <<'PY'
import pathlib, plistlib, sys
folder = pathlib.Path(sys.argv[1])
[source] = folder.glob("*.xctestrun")
with source.open("rb") as f:
    config = plistlib.load(f)
if "TestConfigurations" in config:
    targets = [t for c in config["TestConfigurations"] for t in c["TestTargets"]]
else:
    targets = [t for k,t in config.items() if k != "__xctestrun_metadata__"]
[runnable] = [t for t in targets if t.get("IsUITestBundle") or "GodovoyPlanUITests" in str(t.get("TestBundlePath"))]
runnable.setdefault("EnvironmentVariables", {})["GP_FOCUS_TEST_PROFILE"] = sys.argv[2]
with (folder/"focus.xctestrun").open("wb") as f:
    plistlib.dump(config, f)
PY
xcodebuild test-without-building -xctestrun "$products/focus.xctestrun" \
    -destination "platform=iOS Simulator,id=$phone" -parallel-testing-enabled NO \
    -only-testing:GodovoyPlanUITests/CalendarUITests \
    -resultBundlePath "$stand/Focus.xcresult"
# A misspelled selector can produce a successful run with zero tests.
xcrun xcresulttool get test-results summary --path "$stand/Focus.xcresult" > "$stand/summary.json"
python3 - "$stand/summary.json" <<'PY'
import json, sys
r = json.load(open(sys.argv[1]))
assert r["totalTestCount"] == 2 and r["passedTests"] == 2 and r["failedTests"] == 0, r
print("PASS: both simulator tests completed.")
PY
