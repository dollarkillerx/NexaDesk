#!/usr/bin/env bash
# Rebuilds the browser's half of pairing (crates/nexa-web-pake) as WebAssembly and puts
# it where nexa-web embeds it: crates/nexa-web/web/pake.wasm. Run after changing that
# crate; the result is committed, so ordinary builds do not need the wasm32 target.
#
#   scripts/build-web-pake.sh
#
# Needs: rustup target add wasm32-unknown-unknown
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if command -v brew >/dev/null 2>&1 && [ -d "$(brew --prefix rustup 2>/dev/null)/bin" ]; then
  export PATH="$(brew --prefix rustup)/bin:$PATH"
fi
export PATH="$HOME/.cargo/bin:$PATH"

cargo build --manifest-path "$ROOT/Cargo.toml" -p nexa-web-pake --target wasm32-unknown-unknown --release
cp "$ROOT/target/wasm32-unknown-unknown/release/nexa_web_pake.wasm" "$ROOT/crates/nexa-web/web/pake.wasm"
echo "wrote crates/nexa-web/web/pake.wasm ($(wc -c < "$ROOT/crates/nexa-web/web/pake.wasm") bytes)"
