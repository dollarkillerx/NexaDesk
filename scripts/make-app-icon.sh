#!/usr/bin/env bash
# Regenerates NexaDesk's app icon from scripts/make-app-icon.swift. macOS only.
#
#   scripts/make-app-icon.sh
#
# Writes assets/icon/AppIcon-1024.png (the master) and assets/icon/AppIcon.icns (every size
# macOS asks for, 16 to 1024 px), which scripts/package-macos.sh puts in the bundle. Both
# are committed, so packaging does not need to run this.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/assets/icon"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$OUT"

swift "$ROOT/scripts/make-app-icon.swift" "$OUT/AppIcon-1024.png"

SET="$WORK/AppIcon.iconset"
mkdir -p "$SET"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$OUT/AppIcon-1024.png" --out "$SET/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z "$double" "$double" "$OUT/AppIcon-1024.png" --out "$SET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$SET" -o "$OUT/AppIcon.icns"

echo "wrote $OUT/AppIcon-1024.png and $OUT/AppIcon.icns"
