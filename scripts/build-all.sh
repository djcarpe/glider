#!/usr/bin/env bash
# Build glider for every platform it ships on.
#
#   ./scripts/build-all.sh ios        -> dist/Glider.xcframework
#   ./scripts/build-all.sh android    -> dist/jniLibs/<abi>/libglider.so
#   ./scripts/build-all.sh desktop    -> dist/<target>/{glider,libglider.*}
#   ./scripts/build-all.sh all
#
# Requirements per platform:
#   ios      macOS + Xcode (Apple's SDK and codesigning are not optional and
#            not redistributable — iOS slices can only be built on a Mac)
#   android  Android NDK r26+, $ANDROID_NDK_HOME set, cargo-ndk installed
#   desktop  nothing but rustup targets
set -euo pipefail
cd "$(dirname "$0")/.."
DIST="dist"
mkdir -p "$DIST"

add_targets() { for t in "$@"; do rustup target add "$t" >/dev/null; done; }

build_ios() {
  add_targets aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios
  cargo build --release --target aarch64-apple-ios       # device
  cargo build --release --target aarch64-apple-ios-sim   # simulator, Apple silicon
  cargo build --release --target x86_64-apple-ios        # simulator, Intel

  # One simulator slice holding both architectures.
  mkdir -p "$DIST/ios-sim"
  lipo -create \
    target/aarch64-apple-ios-sim/release/libglider.a \
    target/x86_64-apple-ios/release/libglider.a \
    -output "$DIST/ios-sim/libglider.a"

  rm -rf "$DIST/Glider.xcframework"
  xcodebuild -create-xcframework \
    -library target/aarch64-apple-ios/release/libglider.a -headers include \
    -library "$DIST/ios-sim/libglider.a"                 -headers include \
    -output "$DIST/Glider.xcframework"
  echo "-> $DIST/Glider.xcframework  (drag into Xcode, or point a SwiftPM binaryTarget at it)"
}

build_android() {
  : "${ANDROID_NDK_HOME:?set ANDROID_NDK_HOME to your NDK install}"
  add_targets aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
  # Android 15 ships 16 KB pages; a .so linked for 4 KB will not load there.
  export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-Wl,-z,max-page-size=16384"
  cargo ndk \
    -t arm64-v8a \
    -t armeabi-v7a \
    -t x86_64 \
    -p 24 \
    -o "$DIST/jniLibs" \
    build --release
  echo "-> $DIST/jniLibs  (copy into src/main/jniLibs, or set it as a jniLibs.srcDir)"
}

build_desktop() {
  local targets=(
    x86_64-unknown-linux-musl      # static, runs on any Linux
    aarch64-unknown-linux-musl
    x86_64-pc-windows-msvc
    aarch64-pc-windows-msvc
    x86_64-apple-darwin
    aarch64-apple-darwin
  )
  for t in "${targets[@]}"; do
    rustup target add "$t" >/dev/null 2>&1 || { echo "skip $t (no std)"; continue; }
    if cargo build --release --target "$t" 2>/dev/null; then
      mkdir -p "$DIST/$t"
      cp target/"$t"/release/glider* "$DIST/$t"/ 2>/dev/null || true
      echo "-> $DIST/$t"
    else
      echo "skip $t (needs that platform's linker — build it on a host of that OS, or in CI)"
    fi
  done
}

case "${1:-all}" in
  ios) build_ios ;;
  android) build_android ;;
  desktop) build_desktop ;;
  all) build_desktop; build_android || true; build_ios || true ;;
  *) echo "usage: $0 [ios|android|desktop|all]"; exit 1 ;;
esac
