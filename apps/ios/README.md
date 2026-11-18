# NexaDesk for iOS (viewer)

The iPhone and iPad app controls a Mac or Windows PC running NexaDesk. It is a thin
SwiftUI layer over the same Rust viewer core as the desktop apps (`crates/nexa-client`,
built as a static library), so connecting, QUIC, rate control and reconnecting are
shared. It controls only; it can not be controlled.

## Build and run

Needs Xcode, the Rust iOS targets and XcodeGen:

```sh
rustup target add aarch64-apple-ios aarch64-apple-ios-sim
brew install xcodegen
cd apps/ios && xcodegen
open NexaDesk.xcodeproj
```

Xcode runs `scripts/build-rust.sh` before compiling, which builds `libnexa_client.a`
for the simulator or the device. The simulator build is signed to run locally (the
Keychain needs a signature); a device build needs your team in Signing & Capabilities.

`NexaDesk/Generated/nexa_client.pb.swift` comes from
`crates/nexa-client/proto/nexa_client.proto`; after changing the proto, run
`scripts/gen-proto.sh` (needs `brew install protobuf swift-protobuf`).

## Trying it without touching a real computer

`crates/nexa-agent/examples/demo_host.rs` is a host with a synthetic picture, the real
hardware encoder and input that is printed instead of injected:

```sh
cargo run -p nexa-agent --example demo_host --release      # listens on 127.0.0.1:47900
```

In the simulator, connect to `127.0.0.1:47900` with the password printed by the demo
host. `--lan` (with `0.0.0.0:47900`) makes it show up under "On this network";
`--signal wss://localhost:PORT --signal-ca cert.pem` registers it with a local
`nexa-signal` and prints its device ID (the server's access password goes in
`NEXA_SIGNAL_SECRET`). `--remember` keeps each viewer's pairing, to try connecting
again from Recent with no code. Never test against port 47800 on your own
Mac: that is your real NexaDesk. Debug builds also take `NEXA_TEST_ADDRESS`, `NEXA_TEST_SECRET`, `NEXA_TEST_SERVER` and `NEXA_TEST_SIGNAL_SECRET` from the launch
environment and connect at once:

```sh
SIMCTL_CHILD_NEXA_TEST_ADDRESS=127.0.0.1:47900 SIMCTL_CHILD_NEXA_TEST_SECRET=nexademohost \
  xcrun simctl launch booted app.nexadesk.ios
```

## Connecting

- **Again:** hosts connected to before are listed under Recent (and Favorites, by
  swiping right). A host paired with a one-time code connects with one tap, signed in
  by its pairing; a device ID with nothing else asks the person at the computer; any
  other fills in the address and waits for the code. Long-press to rename or remove.
  No password is kept.
- **On the same network:** hosts that announce themselves (`_nexadesk._udp`, as the
  desktop app does unless "show this computer on the LAN" is off) are listed under
  "On this network"; tap one, then enter its one-time code or password. An IP
  address can also be typed; port 47800 is assumed when none is given. Tailscale
  addresses work the same way.
- **From anywhere, by device ID:** enter the 9-digit device ID. It goes through the
  signaling server chosen under "Signaling server", which offers the desktop app's
  three choices: none (addresses, the LAN and Tailscale only), a published server
  (picked from the project's `servers.json` on GitHub, kept for when it cannot be
  fetched; its key is public), or your own (address and key; every server since
  0.1.20 asks for a key, only an HMAC proof of it is sent, and the app keeps it in the
  Keychain). Each server shows whether it answers as a signaling server and its delay
  (the median of three TCP connects); the check sends nothing and goes through
  Network.framework, since App Transport Security would turn a self-signed server down.
  With no code or password, the person at the computer is asked to allow it. A
  server with its own (self-signed) certificate is trusted on first use: the app
  shows its SHA-256 fingerprint to compare with
  `openssl x509 -noout -fingerprint -sha256 -in <certificate>` on the server, and keeps
  it in `signal-ca.pem` in the app's data (docs/DEPLOY.md §5).

## Languages

English, Simplified Chinese and Japanese, following the system language (the desktop
app's three). The strings are in `NexaDesk/Localizable.xcstrings` (English keys) and
the permission text in `NexaDesk/InfoPlist.xcstrings`. The messages for why a session
ended follow the desktop app's wording (`ui/NexaDesk.App/Localization`). To try one in
the simulator: `xcrun simctl launch booted app.nexadesk.ios -AppleLanguages "(ja)"`.

## Controls

Two ways to drive the pointer, switched with the button in the bar (remembered):

- **Trackpad** (the default): drag one finger to move the pointer drawn on the
  picture, tap to click where it is, touch and hold then move to drag (select text,
  move a window). Precise on a small screen.
- **Touch:** tap to click under the finger, drag to drag, touch and hold for a right
  click.

In both, a two-finger tap is a right click, a two-finger drag scrolls, and a pinch
zooms up to 3× (moving the fingers while pinching pans). Zoomed in, the computer
sends a larger picture, so text stays sharp, and in trackpad mode the picture
follows the pointer. Clicks give a light tap of haptic feedback.

- The controls float at the bottom of the picture, where a thumb reaches: pointer
  mode, keyboard, display, and disconnect (which asks first). ⌄ folds them into one
  small button.
- ⌘ opens the keys a phone keyboard lacks, one tap each: the shortcuts of the
  computer's system (⌘Tab, Spotlight, Mission Control… on a Mac; ⊞, Alt+Tab,
  Task Manager… on Windows; copy, paste, undo on both), F1–F12, Del, Home, End, Page
  Up/Down. No Ctrl+Alt+Del: Windows takes it only from a real keyboard.
- The keyboard button shows the on-screen keyboard, with a bar above it: Esc, Tab,
  ⌃ ⌥ ⌘ ⇧ (latched for the next key, for shortcuts such as ⌘C) and the arrows.
- A hardware keyboard always types, keyboard shown or not, and sends its keys as
  they are, shortcuts included.
- When the computer has more than one display, the display button at the right of
  the bar switches between them, one at a time.

## Not yet

Sound, the clipboard, file transfer, saved passwords, the computer's own pointer shape (the app draws an arrow), and handling the app going to the background
(the session is reconnected when it comes back, as after a network change).

## Icon

`NexaDesk/Assets.xcassets/AppIcon.appiconset/AppIcon-1024.png` is the desktop icon
(`assets/icon/AppIcon-1024.png`) as iOS wants it: full square and opaque, since iOS
rounds the corners itself. `swift scripts/make-icon.swift ../../assets/icon/AppIcon-1024.png
NexaDesk/Assets.xcassets/AppIcon.appiconset/AppIcon-1024.png` makes it again after the
logo changes. The accent colour is the logo's blue. The ⓘ button on the connect screen shows the
version and links to the source on GitHub, the desktop downloads, issues and the website.
