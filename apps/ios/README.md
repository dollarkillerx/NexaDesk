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
`nexa-signal` and prints its device ID. Never test against port 47800 on your own
Mac: that is your real NexaDesk. Debug builds also take `NEXA_TEST_ADDRESS` and `NEXA_TEST_SECRET` from the launch
environment and connect at once:

```sh
SIMCTL_CHILD_NEXA_TEST_ADDRESS=127.0.0.1:47900 SIMCTL_CHILD_NEXA_TEST_SECRET=nexademohost \
  xcrun simctl launch booted app.nexadesk.ios
```

## Connecting

- **On the same network:** hosts that announce themselves (`_nexadesk._udp`, as the
  desktop app does unless "show this computer on the LAN" is off) are listed under
  "On this network"; tap one, then enter its one-time code or password. An IP
  address can also be typed; port 47800 is assumed when none is given. Tailscale
  addresses work the same way.
- **From anywhere, through your own signaling server:** enter the 9-digit device ID
  and the server (`signal.example.com` or `wss://host:port`, as set on the computer).
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

- Tap: click there. Two-finger tap: right click.
- Drag with one finger: press, move, release (move windows, select text).
- Drag with two fingers: scroll.
- The on-screen keyboard types text; the bar above it has Esc, Tab, ⌃ ⌥ ⌘ ⇧ (latched
  for the next key, for shortcuts such as ⌘C) and the arrows.
- A hardware keyboard sends its keys as they are, shortcuts included.

## Not yet

Sound, the clipboard, file transfer, saved connections and known hosts, choosing the
display, zoom, a trackpad-style pointer, and handling the app going to the background
(the session is reconnected when it comes back, as after a network change).
