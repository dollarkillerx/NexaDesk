#!/bin/sh
# Builds nexa-signal for Linux x86_64 on macOS: one static (musl) binary that runs on
# any distribution, with no Docker and no cross toolchain beyond what rustup provides.
#
#   scripts/build-signal-linux.sh   →  target/x86_64-unknown-linux-musl/release/nexa-signal
#
# Also leaves dist/signal/nexa-signal-linux-x86_64 and its .sha256: the two files a release
# carries for deploy/install.sh (it refuses a download without a matching checksum).
#
# ring's C and assembly are compiled by Xcode's clang without libc headers (ring's
# RING_CORE_NOSTDLIBINC mode), archived with rustup's llvm-ar and linked by rust-lld.
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
TARGET=x86_64-unknown-linux-musl

rustup target add "$TARGET" >/dev/null
rustup component add llvm-tools >/dev/null
BIN="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p')/bin"

export CC_x86_64_unknown_linux_musl=clang
export AR_x86_64_unknown_linux_musl="$BIN/llvm-ar"
# --target=…-gnu: clang's own headers defer to the system's under musl, and there is
# none; the objects are the same either way.
export CFLAGS_x86_64_unknown_linux_musl="--target=x86_64-unknown-linux-gnu -ffreestanding -DRING_CORE_NOSTDLIBINC=1"
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld

cargo build --release --locked -p nexa-signal --target "$TARGET" --manifest-path "$ROOT/Cargo.toml"
OUT="$ROOT/dist/signal"
mkdir -p "$OUT"
cp "$ROOT/target/$TARGET/release/nexa-signal" "$OUT/nexa-signal-linux-x86_64"
(cd "$OUT" && shasum -a 256 nexa-signal-linux-x86_64 >nexa-signal-linux-x86_64.sha256)
echo "$ROOT/target/$TARGET/release/nexa-signal"
echo "$OUT/nexa-signal-linux-x86_64 (+ .sha256)"
