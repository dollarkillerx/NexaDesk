#!/bin/sh
# Regenerates NexaDesk/Generated/nexa_client.pb.swift from the C ABI's messages.
# Needs protoc and protoc-gen-swift (Homebrew: protobuf, swift-protobuf).
set -eu
HERE="$(cd "$(dirname "$0")/.." && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
protoc --proto_path="$ROOT/crates/nexa-client/proto" --swift_out="$HERE/NexaDesk/Generated" \
  --swift_opt=Visibility=Internal nexa_client.proto
