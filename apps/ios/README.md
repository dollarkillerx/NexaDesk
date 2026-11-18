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
host. Debug builds also take `NEXA_TEST_ADDRESS` and `NEXA_TEST_SECRET` from the launch
environment and connect at once:

```sh
SIMCTL_CHILD_NEXA_TEST_ADDRESS=127.0.0.1:47900 SIMCTL_CHILD_NEXA_TEST_SECRET=nexademohost \
  xcrun simctl launch booted app.nexadesk.ios
```

## Controls

- Tap: click there. Two-finger tap: right click.
- Drag with one finger: press, move, release (move windows, select text).
- Drag with two fingers: scroll.
- The on-screen keyboard types text; the bar above it has Esc, Tab, ⌃ ⌥ ⌘ ⇧ (latched
  for the next key, for shortcuts such as ⌘C) and the arrows.
- A hardware keyboard sends its keys as they are, shortcuts included.

## Not yet

Sound, the clipboard, file transfer, saved connections and the host list, choosing the
display, zoom, a trackpad-style pointer, and handling the app going to the background
(the session is reconnected when it comes back, as after a network change).
