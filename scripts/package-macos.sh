#!/usr/bin/env bash
# Builds NexaDesk.app: the Rust agent and libnexa_client in release, the Avalonia UI
# with NativeAOT, one bundle, ad-hoc signed. macOS only.
#
#   scripts/package-macos.sh [output dir]      (default: dist/)
#   NEXA_NOTARY_PROFILE=<profile> scripts/package-macos.sh
#       a release: adds the updater with Velopack (vpk, the same version as the Velopack
#       package), notarizes, and writes <output dir>/macos/ (NexaDesk-macOS.dmg,
#       NexaDesk-osx-Portable.zip, the .nupkg and releases.osx.json the updater reads), all
#       for the GitHub release. The profile is a notarytool keychain profile
#       (`xcrun notarytool store-credentials <profile> ...`).
#
# Result: <output dir>/NexaDesk.app
#   Contents/MacOS/NexaDesk               the UI (NativeAOT)
#   Contents/MacOS/nexa-agent             the host side, started by the UI with --consent ui
#   Contents/MacOS/nexa-mcp               the MCP server an AI agent starts (docs/MCP.md)
#   Contents/MacOS/libnexa_client.dylib   the viewer library
#   Contents/MacOS/lib*.dylib             Avalonia's native parts (AvaloniaNative, Skia, HarfBuzz)
#   Contents/Resources/AppIcon.icns       the app icon (assets/icon, from scripts/make-app-icon.sh)
#
# Signed with one identity (a Developer ID, scripts/make-signing-identity.sh, or
# NEXA_SIGN_IDENTITY), every build is the same app to macOS and keeps its privacy grants.
# Ad-hoc signing, the fallback, ties them to this exact build: after rebuilding they must be
# given again. A Developer ID build has the hardened runtime and a secure timestamp, which
# notarization needs; it runs on other Macs without a Gatekeeper warning once notarized.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/dist}"
mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
APP="$OUT/NexaDesk.app"
PUBLISH="$OUT/publish"
VERSION="$(sed -n 's:.*<Version>\(.*\)</Version>.*:\1:p' "$ROOT/ui/NexaDesk.App/NexaDesk.App.csproj" | head -1)"
REPO="https://github.com/dollarkillerx/NexaDesk"
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

step "Rust: nexa-agent, libnexa_client and nexa-mcp (release)"
cargo build --release --manifest-path "$ROOT/Cargo.toml" -p nexa-agent -p nexa-client -p nexa-mcp
AGENT="$ROOT/target/release/nexa-agent"
MCP="$ROOT/target/release/nexa-mcp"
CLIENT="$ROOT/target/release/libnexa_client.dylib"
[ -x "$AGENT" ] || { echo "missing $AGENT" >&2; exit 1; }
[ -x "$MCP" ] || { echo "missing $MCP" >&2; exit 1; }
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
cp "$MCP" "$APP/Contents/MacOS/nexa-mcp"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleLocalizations</key>
  <array><string>en</string><string>zh-Hans</string><string>ja</string></array>
  <key>CFBundleDisplayName</key><string>NexaDesk</string>
  <key>CFBundleExecutable</key><string>NexaDesk</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
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
  <!-- English here; each language's wording is in Resources/<lang>.lproj/InfoPlist.strings. -->
  <key>NSScreenCaptureUsageDescription</key>
  <string>NexaDesk records the screen so that, after you turn on Allow remote control and accept a connection, the other person can see this Mac.</string>
  <key>NSAccessibilityUsageDescription</key>
  <string>NexaDesk needs Accessibility to capture every key while you control another computer (including Cmd+Tab), and, once you allow it, to carry out the other person's keyboard and mouse input on this Mac.</string>
  <key>NSLocalNetworkUsageDescription</key>
  <string>NexaDesk uses the local network to find and connect to other computers on your LAN, and to accept their connections.</string>
  <key>NSBonjourServices</key>
  <array><string>_nexadesk._udp</string></array>
</dict>
</plist>
PLIST
printf 'APPL????' > "$APP/Contents/PkgInfo"
# App icon (Finder, Dock, Cmd+Tab). Regenerate with scripts/make-app-icon.sh.
cp "$ROOT/assets/icon/AppIcon.icns" "$APP/Contents/Resources/AppIcon.icns"
plutil -lint "$APP/Contents/Info.plist" >/dev/null
# Permission prompts in the person's language (same languages as the app's own text).
for lproj in "$ROOT/ui/NexaDesk.App/Localization/InfoPlist"/*.lproj; do
  cp -R "$lproj" "$APP/Contents/Resources/"
  plutil -lint "$APP/Contents/Resources/$(basename "$lproj")/InfoPlist.strings" >/dev/null
done

# One identity for every build, so macOS keeps Screen Recording, Accessibility and
# Keychain "Always Allow" across rebuilds: NEXA_SIGN_IDENTITY, else a Developer ID
# Application certificate, else "NexaDesk Local Signing" from
# scripts/make-signing-identity.sh, else an Apple Development certificate, else ad-hoc.
IDENTITY="${NEXA_SIGN_IDENTITY:-}"
KEYCHAIN_ARGS=()
if [ -n "${NEXA_SIGN_KEYCHAIN:-}" ]; then KEYCHAIN_ARGS=(--keychain "$NEXA_SIGN_KEYCHAIN"); fi
if [ -z "$IDENTITY" ]; then
  IDENTITY=$(security find-identity -v -p codesigning ${NEXA_SIGN_KEYCHAIN:+"$NEXA_SIGN_KEYCHAIN"} 2>/dev/null \
    | sed -n 's/^ *[0-9]*) \([0-9A-F]\{40\}\) "Developer ID Application: .*/\1/p' | head -1)
fi
if [ -z "$IDENTITY" ] && security find-certificate -c "NexaDesk Local Signing" ${NEXA_SIGN_KEYCHAIN:+"$NEXA_SIGN_KEYCHAIN"} >/dev/null 2>&1; then
  IDENTITY="NexaDesk Local Signing"
fi
# Or an Apple Development certificate, if this Mac has one (Xcode's "Sign to Run Locally").
if [ -z "$IDENTITY" ]; then
  IDENTITY=$(security find-identity -v -p codesigning ${NEXA_SIGN_KEYCHAIN:+"$NEXA_SIGN_KEYCHAIN"} 2>/dev/null \
    | sed -n 's/^ *[0-9]*) \([0-9A-F]\{40\}\) "Apple Development: .*/\1/p' | head -1)
fi
DEVELOPER_ID=0
if [ -n "$IDENTITY" ]; then
  step "Signing as \"$IDENTITY\""
  SIGN=(--sign "$IDENTITY" "${KEYCHAIN_ARGS[@]+"${KEYCHAIN_ARGS[@]}"}")
  if security find-identity -v -p codesigning ${NEXA_SIGN_KEYCHAIN:+"$NEXA_SIGN_KEYCHAIN"} 2>/dev/null \
      | grep -F "$IDENTITY" | grep -q '"Developer ID Application: '; then
    DEVELOPER_ID=1
  fi
else
  step "Ad-hoc signing (scripts/make-signing-identity.sh keeps permissions across rebuilds)"
  SIGN=(--sign -)
fi
# Developer ID: the hardened runtime with no exceptions (NativeAOT makes no code at run
# time; every library in the bundle carries this same signature) and Apple's timestamp.
if [ "$DEVELOPER_ID" = 1 ]; then
  SIGN+=(--options runtime --timestamp)
else
  SIGN+=(--timestamp=none)
fi
# Inside out: libraries and the helper first, then the bundle, which seals them.
for lib in "$APP/Contents/MacOS"/*.dylib; do
  codesign --force "${SIGN[@]}" "$lib"
done
codesign --force "${SIGN[@]}" --identifier app.nexadesk.agent "$APP/Contents/MacOS/nexa-agent"
codesign --force "${SIGN[@]}" --identifier app.nexadesk.mcp "$APP/Contents/MacOS/nexa-mcp"
codesign --force "${SIGN[@]}" "$APP"
codesign --verify --strict --deep "$APP"
codesign -dr - "$APP" 2>&1 | sed -n 's/^designated => /identity: /p'

PROFILE="${NEXA_NOTARY_PROFILE:-}"
if [ -n "$PROFILE" ]; then
  [ "$DEVELOPER_ID" = 1 ] || { echo "a release needs a Developer ID Application identity" >&2; exit 1; }
  VPK="$(command -v vpk || echo "$HOME/.dotnet/tools/vpk")"
  [ -x "$VPK" ] || { echo "missing vpk (dotnet tool install -g vpk)" >&2; exit 1; }
  RELEASES="$OUT/macos"
  rm -rf "$RELEASES"
  mkdir -p "$RELEASES"
  step "Previous release (for delta updates)"
  "$VPK" download github --repoUrl "$REPO" -o "$RELEASES" -c osx || echo "  none found; full package only"

  # Velopack adds its updater (UpdateMac) to the bundle and signs it and the bundle; the
  # rest keeps the signatures above (--signDisableDeep). Then notarizes and staples.
  step "Velopack: updater, signature, notarization (notarytool profile \"$PROFILE\")"
  "$VPK" pack -u NexaDesk -v "$VERSION" -p "$APP" -e NexaDesk -o "$RELEASES" -c osx \
    --packTitle NexaDesk --packAuthors NexaDesk \
    --signAppIdentity "$IDENTITY" --signEntitlements "$ROOT/assets/macos/NexaDesk.entitlements" --signDisableDeep \
    --notaryProfile "$PROFILE" --noInst ${NEXA_SIGN_KEYCHAIN:+--keychain "$NEXA_SIGN_KEYCHAIN"}
  # The bundle as published, with the updater and the notarization ticket.
  rm -rf "$APP"
  ditto -x -k "$RELEASES/NexaDesk-osx-Portable.zip" "$OUT"
  xcrun stapler validate "$APP"

  DMG="$RELEASES/NexaDesk-macOS.dmg"
  step "Disk image: $DMG"
  STAGE="$(mktemp -d)"
  ditto "$APP" "$STAGE/NexaDesk.app"
  ln -s /Applications "$STAGE/Applications"
  hdiutil create -volname NexaDesk -srcfolder "$STAGE" -fs HFS+ -format UDZO -ov "$DMG" >/dev/null
  rm -rf "$STAGE"
  codesign --force --sign "$IDENTITY" "${KEYCHAIN_ARGS[@]+"${KEYCHAIN_ARGS[@]}"}" --timestamp "$DMG"
  xcrun notarytool submit "$DMG" --keychain-profile "$PROFILE" --wait
  xcrun stapler staple "$DMG"
  spctl --assess --type execute -vv "$APP" 2>&1 | sed 's/^/  /'
  spctl --assess --type open --context context:primary-signature -vv "$DMG" 2>&1 | sed 's/^/  /'
  ls -lh "$RELEASES"
fi

step "Done"
du -sh "$APP" | sed 's/^/bundle size: /'
for f in "$APP/Contents/MacOS"/*; do du -h "$f"; done | sed 's/^/  /'
echo "Open it with: open \"$APP\""
