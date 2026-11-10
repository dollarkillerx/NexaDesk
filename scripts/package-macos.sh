#!/usr/bin/env bash
# Builds NexaDesk.app: the Rust agent and libnexa_client in release, the Avalonia UI
# with NativeAOT, one bundle, ad-hoc signed. macOS only.
#
#   scripts/package-macos.sh [output dir]      (default: dist/)
#
# Result: <output dir>/NexaDesk.app
#   Contents/MacOS/NexaDesk               the UI (NativeAOT)
#   Contents/MacOS/nexa-agent             the host side, started by the UI with --consent ui
#   Contents/MacOS/libnexa_client.dylib   the viewer library
#   Contents/MacOS/lib*.dylib             Avalonia's native parts (AvaloniaNative, Skia, HarfBuzz)
#
# Ad-hoc signing ties macOS privacy grants (Screen Recording, Accessibility) to this exact
# build: after rebuilding, macOS treats it as a new app and the grants must be given again.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/dist}"
mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
APP="$OUT/NexaDesk.app"
PUBLISH="$OUT/publish"
VERSION="0.1.0"
BUNDLE_ID="app.nexadesk.NexaDesk"

case "$(uname -m)" in
  arm64) RID="osx-arm64" ;;
  x86_64) RID="osx-x64" ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

if command -v brew >/dev/null 2>&1 && [ -d "$(brew --prefix rustup 2>/dev/null)/bin" ]; then
  export PATH="$(brew --prefix rustup)/bin:$PATH"
fi
export PATH="$HOME/.cargo/bin:$PATH"

step() { printf '\n==> %s\n' "$*"; }

step "Rust: nexa-agent and libnexa_client (release)"
cargo build --release --manifest-path "$ROOT/Cargo.toml" -p nexa-agent -p nexa-client
AGENT="$ROOT/target/release/nexa-agent"
CLIENT="$ROOT/target/release/libnexa_client.dylib"
[ -x "$AGENT" ] || { echo "missing $AGENT" >&2; exit 1; }
[ -f "$CLIENT" ] || { echo "missing $CLIENT" >&2; exit 1; }

step "UI: NativeAOT publish ($RID)"
rm -rf "$PUBLISH"
dotnet publish "$ROOT/ui/NexaDesk.App/NexaDesk.App.csproj" -c Release -r "$RID" \
  -p:PublishAot=true -p:StripSymbols=true -o "$PUBLISH" -nologo

step "Bundle: $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$PUBLISH/NexaDesk" "$APP/Contents/MacOS/NexaDesk"
for lib in "$PUBLISH"/*.dylib; do
  [ "$(basename "$lib")" = "libnexa_client.dylib" ] && continue
  cp "$lib" "$APP/Contents/MacOS/"
done
cp "$CLIENT" "$APP/Contents/MacOS/libnexa_client.dylib"
cp "$AGENT" "$APP/Contents/MacOS/nexa-agent"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>zh_CN</string>
  <key>CFBundleDisplayName</key><string>NexaDesk</string>
  <key>CFBundleExecutable</key><string>NexaDesk</string>
  <key>CFBundleIdentifier</key><string>${BUNDLE_ID}</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>NexaDesk</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSPrincipalClass</key><string>NSApplication</string>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
  <key>NSScreenCaptureUsageDescription</key>
  <string>NexaDesk 需要录制屏幕，才能在你开启“允许别人控制本机”并同意连接后，把这台 Mac 的画面发给对方。</string>
  <key>NSAccessibilityUsageDescription</key>
  <string>NexaDesk 需要辅助功能权限：控制别人时捕获全部按键（包括 Cmd+Tab）；被控制时，在你同意后执行对方的键盘和鼠标操作。</string>
  <key>NSLocalNetworkUsageDescription</key>
  <string>NexaDesk 需要访问本地网络，才能连接局域网中的其他电脑，或接受它们的连接。</string>
</dict>
</plist>
PLIST
printf 'APPL????' > "$APP/Contents/PkgInfo"
plutil -lint "$APP/Contents/Info.plist" >/dev/null

step "Ad-hoc signing"
# Inside out: libraries and the helper first, then the bundle, which seals them.
for lib in "$APP/Contents/MacOS"/*.dylib; do
  codesign --force --sign - --timestamp=none "$lib"
done
codesign --force --sign - --timestamp=none --identifier app.nexadesk.agent "$APP/Contents/MacOS/nexa-agent"
codesign --force --sign - --timestamp=none "$APP"
codesign --verify --strict --deep "$APP"

step "Done"
du -sh "$APP" | sed 's/^/bundle size: /'
for f in "$APP/Contents/MacOS"/*; do du -h "$f"; done | sed 's/^/  /'
echo "Open it with: open \"$APP\""
