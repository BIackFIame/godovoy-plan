#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")"
project_dir="$PWD"
build_dir="${GP_BUILD_DIR:-$project_dir/build}"
export DEVELOPER_DIR="${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}"
export IPHONEOS_DEPLOYMENT_TARGET=16.0
build_rust=1
core_only=0
simulator_id=""
while (($#)); do
    case "$1" in
        --prebuilt) build_rust=0; shift ;;
        --core-only) core_only=1; shift ;;
        --test) simulator_id="${2:?Укажите UDID симулятора}"; shift 2 ;;
        *) echo "Сценарий принимает --prebuilt, --core-only и --test UDID." >&2; exit 2 ;;
    esac
done
if ((build_rust)); then
    test -f ../core/Cargo.toml || { echo "Для архива Xcode запустите ./build.sh --prebuilt. Исходное ядро находится в полном архиве проекта." >&2; exit 1; }
    export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$build_dir/rust}"
    targets=(aarch64-apple-ios aarch64-apple-ios-sim)
    platforms=(iphoneos iphonesimulator)
    for i in 0 1; do
        target="${targets[i]}"
        "${CARGO:-cargo}" rustc --lib --release --locked --manifest-path ../core/Cargo.toml --target "$target" --crate-type staticlib
        destination="$project_dir/RustLibs/${platforms[i]}"
        mkdir -p "$destination"
        cp "$CARGO_TARGET_DIR/$target/release/libgodovoy_mobile_core.a" "$destination/"
    done
fi
for platform in iphoneos iphonesimulator; do
    test -s "RustLibs/$platform/libgodovoy_mobile_core.a" || { echo "Отсутствует библиотека Rust для $platform." >&2; exit 1; }
done
if ((core_only)); then exit 0; fi
xcodebuild -showsdks >/dev/null
for platform in iphoneos iphonesimulator; do
    if [[ "$platform" == iphoneos ]]; then
        destination="generic/platform=iOS"
    else
        destination="generic/platform=iOS Simulator"
    fi
    xcodebuild build -project GodovoyPlan.xcodeproj -scheme GodovoyPlan -configuration Release \
        -derivedDataPath "$build_dir/Xcode" -sdk "$platform" -destination "$destination" \
        ARCHS=arm64 ONLY_ACTIVE_ARCH=NO CODE_SIGNING_ALLOWED=NO
    test -x "$build_dir/Xcode/Build/Products/Release-$platform/GodovoyPlan.app/GodovoyPlan"
done
if [[ -n "$simulator_id" ]]; then
    xcrun simctl bootstatus "$simulator_id" -b
    xcodebuild test -project GodovoyPlan.xcodeproj -scheme GodovoyPlan -configuration Debug \
        -destination "platform=iOS Simulator,id=$simulator_id" -parallel-testing-enabled NO -derivedDataPath "$build_dir/Xcode"
fi
