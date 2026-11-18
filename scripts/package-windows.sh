#!/usr/bin/env bash
# Builds the Windows (x64) installer of NexaDesk on macOS, unsigned, with Velopack (the
# same way MySSH ships for Windows).
#
#   scripts/package-windows.sh [version]      (default: the app's <Version>)
#
# Needs: mingw-w64 (brew install mingw-w64), the .NET 10 SDK, and vpk
# (dotnet tool install -g vpk).
#
# Result in dist/windows/:
#   NexaDesk-win-Setup.exe       per-user install to %LocalAppData%\NexaDesk, Start menu and
#                                desktop shortcuts, no administrator rights needed
#   NexaDesk-win-Portable.zip    the same app, unpacked anywhere
#   *.nupkg, releases.win.json   what the app's updater reads from the GitHub release
#
# The UI is a self-contained JIT build with ReadyToRun: NativeAOT cannot cross-compile to
# Windows. Rust is cross-compiled with mingw (x86_64-pc-windows-gnu); the binaries only
# import system DLLs. Without a code-signing certificate, SmartScreen shows "Windows
# protected your PC" on first run (More info → Run anyway).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="${1:-$(sed -n 's:.*<Version>\(.*\)</Version>.*:\1:p' "$ROOT/ui/NexaDesk.App/NexaDesk.App.csproj" | head -1)}"
OUT="$ROOT/dist/windows"
TARGET=x86_64-pc-windows-gnu
VPK="$(command -v vpk || echo "$HOME/.dotnet/tools/vpk")"

if command -v brew >/dev/null 2>&1 && [ -d "$(brew --prefix rustup 2>/dev/null)/bin" ]; then
  export PATH="$(brew --prefix rustup)/bin:$PATH"
fi
export PATH="$HOME/.cargo/bin:$PATH"
command -v x86_64-w64-mingw32-gcc >/dev/null || { echo "missing mingw-w64 (brew install mingw-w64)" >&2; exit 1; }
[ -x "$VPK" ] || { echo "missing vpk (dotnet tool install -g vpk)" >&2; exit 1; }

step() { printf '\n==> %s\n' "$*"; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
PUB="$WORK/publish"

step "Rust: nexa-agent.exe, nexa_client.dll and nexa-mcp.exe ($TARGET, release)"
rustup target add "$TARGET" >/dev/null
export CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc
export AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
cargo build --release --manifest-path "$ROOT/Cargo.toml" -p nexa-agent -p nexa-client -p nexa-mcp --target "$TARGET"
BIN="$ROOT/target/$TARGET/release"

step "UI: publish $VERSION (win-x64, self-contained, ReadyToRun)"
dotnet publish "$ROOT/ui/NexaDesk.App/NexaDesk.App.csproj" -c Release -r win-x64 --self-contained \
  -p:PublishAot=false -p:PublishReadyToRun=true -p:Version="$VERSION" \
  -o "$PUB" --artifacts-path "$WORK/artifacts" -nologo
rm -f "$PUB"/*.pdb
cp "$BIN/nexa-agent.exe" "$BIN/nexa_client.dll" "$BIN/nexa-mcp.exe" "$PUB/"
# Symbols only (half the size); exports and code are untouched.
x86_64-w64-mingw32-strip --strip-all "$PUB/nexa-agent.exe" "$PUB/nexa_client.dll" "$PUB/nexa-mcp.exe"
test -f "$PUB/NexaDesk.exe"

rm -rf "$OUT"
mkdir -p "$OUT"
step "Previous release (for delta updates)"
"$VPK" "[win]" download github --repoUrl https://github.com/dollarkillerx/NexaDesk -o "$OUT" -c win \
  || echo "  none found; full package only"

step "Pack (unsigned)"
"$VPK" "[win]" pack -u NexaDesk -v "$VERSION" -p "$PUB" -e NexaDesk.exe -o "$OUT" -c win -r win-x64 \
  --packTitle NexaDesk --packAuthors NexaDesk --icon "$ROOT/assets/icon/AppIcon.ico"

step "Done"
ls -lh "$OUT"
