#!/bin/sh
# Regenerates the README screenshots in docs/images (2x, made-up devices and addresses).
#   - the app: ui/NexaDesk.Screenshots renders the real views headless;
#   - the web page: the nexa-web demo, captured with headless Google Chrome.
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT="$ROOT/docs/images"
mkdir -p "$OUT"

dotnet run -c Release --project "$ROOT/ui/NexaDesk.Screenshots" -- "$OUT"

CHROME="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
if [ ! -x "$CHROME" ]; then
    echo "Google Chrome not found; skipping the web page" >&2
    exit 0
fi
cargo build -q -p nexa-web --example demo --manifest-path "$ROOT/Cargo.toml"
"$ROOT/target/debug/examples/demo" 127.0.0.1:47811 >/dev/null &
DEMO=$!
PROFILE=$(mktemp -d)
trap 'kill $DEMO 2>/dev/null; wait $DEMO 2>/dev/null || true; rm -rf "$PROFILE"' EXIT
sleep 1
"$CHROME" --headless=new --user-data-dir="$PROFILE" --lang=zh-CN --accept-lang=zh-CN \
    --hide-scrollbars --force-device-scale-factor=2 --window-size=1100,540 \
    --virtual-time-budget=3000 --screenshot="$OUT/web.png" http://127.0.0.1:47811/ 2>/dev/null
echo "$OUT/web.png"
