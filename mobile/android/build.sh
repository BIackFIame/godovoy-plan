#!/bin/sh
set -eu
source_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project=$(CDPATH= cd -- "$source_dir/../.." && pwd)
build="${MOBILE_BUILD_DIR:?Set MOBILE_BUILD_DIR to a dedicated build directory}"
mkdir -p "$build"; build=$(cd "$build" && pwd)
sdk="${ANDROID_HOME:?Set ANDROID_HOME to the Android SDK}"
ndk="${ANDROID_NDK_HOME:?Set ANDROID_NDK_HOME to the NDK}"
java="${JAVA_HOME:?Set JAVA_HOME to JDK 17 or newer}"
bt="$sdk/build-tools/${ANDROID_BUILD_TOOLS:-35.0.0}"
platform="$sdk/platforms/android-${ANDROID_COMPILE_SDK:-34}/android.jar"
ndk_bin=$(find "$ndk/toolchains/llvm/prebuilt" -mindepth 1 -maxdepth 1 -type d | head -1)/bin
cargo_bin="${CARGO:-cargo}"
target_dir="${CARGO_TARGET_DIR:-$build/target}"
export CARGO_TARGET_DIR="$target_dir"
package=$(mktemp -d "$build/apk.XXXXXX")
mkdir -p "$package/classes" "$package/dex" "$package/gen" "$package/lib" "$package/res/drawable-nodpi"
cp -R "$source_dir/src/main/res/." "$package/res/"
cp "$project/assets/app-icon.png" "$package/res/drawable-nodpi/app_icon.png"
for target in aarch64-linux-android x86_64-linux-android armv7-linux-androideabi; do
  linker="$ndk_bin/${target}26-clang"
  case "$target" in
    aarch64*) abi=arm64-v8a; export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$linker";;
    x86_64*) abi=x86_64; export CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER="$linker";;
    armv7*) abi=armeabi-v7a; linker="$ndk_bin/armv7a-linux-androideabi26-clang"; export CARGO_TARGET_ARMV7_LINUX_ANDROIDEABI_LINKER="$linker";;
  esac
  "$cargo_bin" build --manifest-path "$project/mobile/core/Cargo.toml" --lib --release --locked --target "$target" ${CARGO_OFFLINE:+--offline}
  libs="$package/lib/$abi"; mkdir -p "$libs"
  cp "$target_dir/$target/release/libgodovoy_mobile_core.so" "$libs/"
  "$ndk_bin/llvm-strip" --strip-unneeded "$libs/libgodovoy_mobile_core.so"
  "$linker" -O2 -Wall -Werror -shared -fPIC -fvisibility=hidden -Wl,-z,max-page-size=16384,-z,relro,-z,now -Wl,-soname,libgodovoy_android.so "$source_dir/src/main/c/bridge.c" -L"$libs" -lgodovoy_mobile_core -o "$libs/libgodovoy_android.so"
done
"$bt/aapt2" compile --dir "$package/res" -o "$package/res.zip"
"$bt/aapt2" link --manifest "$source_dir/src/main/AndroidManifest.xml" -I "$platform" -o "$package/app.apk" --java "$package/gen" "$package/res.zip"
find "$source_dir/src/main/java" "$package/gen" -name '*.java' -print0 | xargs -0 "$java/bin/javac" -encoding UTF-8 --release 8 -Xlint:-options -classpath "$platform" -d "$package/classes"
"$java/bin/jar" cf "$package/classes.jar" -C "$package/classes" .
"$java/bin/java" -cp "$bt/lib/d8.jar" com.android.tools.r8.D8 --min-api 26 --lib "$platform" --output "$package/dex" "$package/classes.jar"
(cd "$package" && zip -q -0 app.apk lib/*/*.so)
(cd "$package/dex" && zip -q -0 ../app.apk classes*.dex)
"$bt/zipalign" -P 16 -f 4 "$package/app.apk" "$package/aligned.apk"
if [ -n "${ANDROID_SIGNING_KEYSTORE:-}" ]; then
  key="$ANDROID_SIGNING_KEYSTORE"
  alias="${ANDROID_SIGNING_ALIAS:?Set ANDROID_SIGNING_ALIAS}"
  : "${ANDROID_SIGNING_PASSWORD:?Set ANDROID_SIGNING_PASSWORD}"
else
  key="$build/local-testing.p12"; alias=localtesting
  ANDROID_SIGNING_PASSWORD=android-test-local; export ANDROID_SIGNING_PASSWORD
  if [ ! -f "$key" ]; then
    (umask 077; "$java/bin/keytool" -genkeypair -alias "$alias" -keyalg RSA -keysize 3072 -validity 3650 -storetype PKCS12 -keystore "$key" -storepass:env ANDROID_SIGNING_PASSWORD -dname "CN=Godovoy Plan local test")
  fi
fi
"$java/bin/java" -jar "$bt/lib/apksigner.jar" sign --ks "$key" --ks-key-alias "$alias" --ks-pass env:ANDROID_SIGNING_PASSWORD --out "$build/GodovoyPlan-Android.apk" "$package/aligned.apk"
"$java/bin/java" -jar "$bt/lib/apksigner.jar" verify --verbose --print-certs "$build/GodovoyPlan-Android.apk"
echo "$build/GodovoyPlan-Android.apk"
