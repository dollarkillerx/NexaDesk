#!/bin/sh
# Builds the command-line host, nexa-term-host, for Linux and Windows on macOS:
#
#   scripts/build-term-host.sh   →  dist/term-host/nexa-term-host-linux-x86_64
#                                    dist/term-host/nexa-term-host-windows-x86_64.exe
#
# Linux: one static (musl) binary, built like nexa-signal (see build-signal-linux.sh).
# Windows: x86_64-pc-windows-gnu, linked by Homebrew's mingw-w64 (brew install mingw-w64).
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT="$ROOT/dist/term-host"
mkdir -p "$OUT"

rustup target add x86_64-unknown-linux-musl x86_64-pc-windows-gnu >/dev/null
rustup component add llvm-tools >/dev/null
BIN="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p')/bin"

# Linux
export CC_x86_64_unknown_linux_musl=clang
export AR_x86_64_unknown_linux_musl="$BIN/llvm-ar"
export CFLAGS_x86_64_unknown_linux_musl="--target=x86_64-unknown-linux-gnu -ffreestanding -DRING_CORE_NOSTDLIBINC=1"
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
cargo build --release --locked -p nexa-term-host --target x86_64-unknown-linux-musl --manifest-path "$ROOT/Cargo.toml"
cp "$ROOT/target/x86_64-unknown-linux-musl/release/nexa-term-host" "$OUT/nexa-term-host-linux-x86_64"

# Windows
if command -v x86_64-w64-mingw32-gcc >/dev/null; then
  export CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc
  export AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar
  export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
  cargo build --release --locked -p nexa-term-host --target x86_64-pc-windows-gnu --manifest-path "$ROOT/Cargo.toml"
  cp "$ROOT/target/x86_64-pc-windows-gnu/release/nexa-term-host.exe" "$OUT/nexa-term-host-windows-x86_64.exe"
else
  echo "skipping Windows: x86_64-w64-mingw32-gcc not found (brew install mingw-w64)" >&2
fi
ls -l "$OUT"
