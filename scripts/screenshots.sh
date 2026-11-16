#!/bin/sh
# Regenerates the README screenshots in docs/images (2x, made-up devices and addresses).
#   - the app: ui/NexaDesk.Screenshots renders the real views headless;
#   - the web pages: the nexa-web demo, driven in headless Google Chrome by
#     scripts/web-screenshots.mjs (connect page, a session, the terminal).
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
LOG=$(mktemp)
# A plain shell for the terminal shot, so nothing of this Mac shows.
NEXA_DEMO_SHELL=/bin/sh "$ROOT/target/debug/examples/demo" 127.0.0.1:47811 >"$LOG" &
DEMO=$!
trap 'kill $DEMO 2>/dev/null; wait $DEMO 2>/dev/null || true; rm -f "$LOG"' EXIT
for _ in 1 2 3 4 5 6 7 8 9 10; do
    grep -q "one-time code" "$LOG" && break
    sleep 0.5
done
node "$ROOT/scripts/web-screenshots.mjs" https://127.0.0.1:47811/ "$LOG" "$OUT"
