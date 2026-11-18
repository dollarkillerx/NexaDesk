#!/bin/sh
# Builds libnexa_client.a for the platform Xcode is building, and puts it where the
# app links it from. Run by Xcode as a pre-build step (project.yml).
#
# Xcode's environment (SDKROOT, the iOS deployment variables) would make cargo build
# its host-side build scripts for iOS too, so cargo runs in a clean one.
set -eu
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
case "${PLATFORM_NAME:-iphonesimulator}" in
  iphonesimulator) TARGET=aarch64-apple-ios-sim ;;
  iphoneos) TARGET=aarch64-apple-ios ;;
  *) echo "error: unsupported platform ${PLATFORM_NAME}"; exit 1 ;;
esac
TARGET_DIR="$ROOT/target/ios"
env -i \
  HOME="$HOME" \
  PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
  IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-17.0}" \
  CARGO_TARGET_DIR="$TARGET_DIR" \
  sh -c "cd '$ROOT' && cargo rustc -q -p nexa-client --lib --crate-type staticlib --release --target $TARGET"
mkdir -p "${BUILT_PRODUCTS_DIR:-$ROOT/target/ios/out}"
cp "$TARGET_DIR/$TARGET/release/libnexa_client.a" "${BUILT_PRODUCTS_DIR:-$ROOT/target/ios/out}/libnexa_client.a"
