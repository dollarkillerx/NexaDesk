import Foundation
import QuartzCore
import SwiftProtobuf

/// The Rust viewer core (libnexa_client) behind its C ABI. One per process.
/// Events arrive on the core's threads and are handed to the main actor.
@MainActor
final class NexaCore: ObservableObject {
    enum Phase: Equatable {
        case idle
        case connecting(String)
        case connected
        case reconnecting(String)
    }

    @Published private(set) var phase: Phase = .idle
    @Published private(set) var hostName = ""
    @Published private(set) var stream: (w: Int, h: Int) = (0, 0)
    @Published private(set) var inputAllowed = false
    @Published var notice: String?
    /// A self-hosted signaling server whose certificate is not trusted yet.
    @Published var trustPrompt: TrustPrompt?

    struct TrustPrompt: Identifiable, Equatable {
        let server: String
        let fingerprint: String
        var id: String { server + fingerprint }
    }

    /// The port a NexaDesk host listens on unless set otherwise.
    static let defaultPort = 47800
    private var lastConnect: (address: String, server: String, secret: String)?

    private var client: OpaquePointer?
    private var session: OpaquePointer?
    private var tag: UInt64 = 0

    init() {
        var cfg = Nexa_Client_ClientConfig()
        cfg.deviceName = ProcessInfo.processInfo.hostName
        let support = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        cfg.dataDir = support.appendingPathComponent("NexaDesk").path
        cfg.downloadsDir = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0].path
        let bytes = (try? cfg.serializedData()) ?? Data()
        let user = Unmanaged.passUnretained(self).toOpaque()
        var out: OpaquePointer?
        let rc = bytes.withUnsafeBytes { raw in
            nexa_init(raw.bindMemory(to: UInt8.self).baseAddress, raw.count, nexaEvent, user, &out)
        }
        if rc == NEXA_OK {
            client = out
        } else {
            notice = "Could not start: \(Self.lastError())"
        }
    }

    // MARK: Sessions

    static func isDeviceId(_ s: String) -> Bool {
        let t = s.filter { !$0.isWhitespace }
        return t.count == 9 && t.allSatisfy(\.isNumber)
    }

    /// host:port as the core takes it; a bare address gets the default port.
    static func normalizedAddress(_ s: String) -> String {
        let a = s.trimmingCharacters(in: .whitespaces)
        if isDeviceId(a) { return a.filter { !$0.isWhitespace } }
        if a.hasPrefix("[") { return a.contains("]:") ? a : "\(a):\(defaultPort)" }
        switch a.filter({ $0 == ":" }).count {
        case 0: return "\(a):\(defaultPort)"          // 192.168.1.20, mac.local
        case 1: return a                                // 192.168.1.20:47800
        default: return "[\(a)]:\(defaultPort)"         // a bare IPv6 address
        }
    }

    /// wss://host[:port], as the core and the agent take it; "host" alone works too.
    static func normalizedServer(_ s: String) -> String {
        let t = s.trimmingCharacters(in: .whitespaces)
        if t.isEmpty { return t }
        let rest = t.lowercased().hasPrefix("wss://") ? String(t.dropFirst(6)) : t
        return "wss://" + rest.trimmingCharacters(in: CharacterSet(charactersIn: "/"))
    }

    /// `address` is host[:port] (the LAN, Tailscale) or a 9-digit device ID, which
    /// goes through `signalServer`. With an ID and no secret, the person at the host is
    /// asked to allow the connection.
    func connect(address: String, signalServer: String, secret: String) {
        guard let client, session == nil else { return }
        tag &+= 1
        var req = Nexa_Client_ConnectRequest()
        req.address = Self.normalizedAddress(address)
        let isId = Self.isDeviceId(req.address)
        if isId {
            req.signalServer = Self.normalizedServer(signalServer)
            if req.signalServer.isEmpty {
                notice = "A device ID needs a signaling server: enter yours below."
                return
            }
        }
        req.tag = tag
        req.mode = .balanced
        // A one-time code is six digits; anything else is the host's fixed password.
        let s = secret.trimmingCharacters(in: .whitespaces)
        if s.count == 6, s.allSatisfy(\.isNumber) {
            req.code = s
        } else if !s.isEmpty {
            req.password = s
        } else if isId {
            req.request = true
        }
        lastConnect = (address, signalServer, secret)
        let bytes = (try? req.serializedData()) ?? Data()
        var out: OpaquePointer?
        let rc = bytes.withUnsafeBytes { raw in
            nexa_connect(client, raw.bindMemory(to: UInt8.self).baseAddress, raw.count, &out)
        }
        if rc == NEXA_OK {
            session = out
            phase = .connecting("Connecting…")
        } else {
            notice = Self.lastError()
        }
    }

    func disconnect() {
        if let session { nexa_session_close(session) }
        session = nil
        phase = .idle
        inputAllowed = false
        stream = (0, 0)
    }

    // MARK: Video

    func attach(layer: CAMetalLayer, widthPx: Int, heightPx: Int, scale: CGFloat) {
        guard let session else { return }
        let rc = nexa_session_attach_view(session, Unmanaged.passUnretained(layer).toOpaque(),
                                          NEXA_VIEW_CAMETALLAYER, UInt32(widthPx), UInt32(heightPx), Float(scale))
        if rc != NEXA_OK { notice = "Video: \(Self.lastError())" }
    }

    func resize(widthPx: Int, heightPx: Int, scale: CGFloat) {
        guard let session else { return }
        _ = nexa_session_resize_view(session, UInt32(widthPx), UInt32(heightPx), Float(scale))
    }

    func detach() {
        guard let session else { return }
        _ = nexa_session_detach_view(session)
    }

    // MARK: Input (coordinates in view pixels, as passed to attach)

    func move(x: Double, y: Double) {
        send { $0.move = .with { $0.x = x; $0.y = y } }
    }

    func button(_ button: UInt32, down: Bool) {
        send { $0.button = .with { $0.button = button; $0.down = down } }
    }

    /// `dx`/`dy` in 1/120 notches; + is right/up.
    func wheel(dx: Int32, dy: Int32) {
        send { $0.wheel = .with { $0.dx = dx; $0.dy = dy } }
    }

    func text(_ utf8: String) {
        send { $0.text = .with { $0.utf8 = utf8 } }
    }

    /// A USB HID usage on the keyboard page (0x07).
    func key(_ usage: UInt32, down: Bool) {
        send { $0.key = .with { $0.usage = usage; $0.down = down } }
    }

    func tapKey(_ usage: UInt32) {
        key(usage, down: true)
        key(usage, down: false)
    }

    private func send(_ fill: (inout Nexa_Client_SessionCommand) -> Void) {
        guard let session, inputAllowed else { return }
        var cmd = Nexa_Client_SessionCommand()
        fill(&cmd)
        guard let bytes = try? cmd.serializedData() else { return }
        _ = bytes.withUnsafeBytes { raw in
            nexa_session_command(session, raw.bindMemory(to: UInt8.self).baseAddress, raw.count)
        }
    }

    // MARK: Events

    fileprivate func handle(_ event: Nexa_Client_Event) {
        // Events of an earlier session (tag) are stale.
        if event.tag != 0, event.tag != tag { return }
        switch event.body {
        case .connecting:
            phase = .connecting("Connecting…")
        case .connected(let c):
            hostName = c.hostName
            phase = .connected
            if let session, c.perms.input {
                inputAllowed = nexa_session_set_input_active(session, true) == NEXA_OK
            }
        case .stream(let s):
            stream = (Int(s.w), Int(s.h))
        case .reconnecting(let r):
            phase = .reconnecting(r.reason.isEmpty ? "Reconnecting…" : "Reconnecting: \(r.reason)")
        case .disconnected(let d):
            session = nil
            phase = .idle
            inputAllowed = false
            if d.reason == .disconnectSignalServer, let last = lastConnect, Self.isDeviceId(last.address) {
                // A self-hosted server's own certificate is the usual cause: offer to trust it.
                checkServer(Self.normalizedServer(last.server), otherwise: d.message)
            } else {
                notice = d.message.isEmpty ? "Disconnected" : d.message
            }
        case .warning(let w):
            notice = w.text
        default:
            break
        }
    }

    // MARK: Signaling server trust (signal-ca.pem in the data directory)

    private func checkServer(_ server: String, otherwise message: String) {
        Task.detached {
            let answer = Self.serverCertificate(server)
            await MainActor.run {
                switch answer {
                case .success(let (trusted, fingerprint)) where !trusted:
                    self.trustPrompt = TrustPrompt(server: server, fingerprint: fingerprint)
                case .success:
                    self.notice = message.isEmpty ? "The signaling server refused" : message
                case .failure(let e):
                    self.notice = "\(message.isEmpty ? "Signaling server" : message) (\(e.text))"
                }
            }
        }
    }

    /// Trusts the server the person looked at, then connects again.
    func trust(_ prompt: TrustPrompt) {
        trustPrompt = nil
        Task.detached {
            let error = Self.trustServer(prompt.server, fingerprint: prompt.fingerprint)
            await MainActor.run {
                if let error {
                    self.notice = "Could not trust the server: \(error)"
                } else if let last = self.lastConnect {
                    self.connect(address: last.address, signalServer: last.server, secret: last.secret)
                }
            }
        }
    }

    struct CoreError: Error { let text: String }

    nonisolated private static func serverCertificate(_ server: String) -> Result<(Bool, String), CoreError> {
        let url = Array(server.utf8)
        var buf = [UInt8](repeating: 0, count: 512)
        var written = 0
        guard nexa_signal_certificate(url, url.count, &buf, buf.count, &written) == NEXA_OK else {
            return .failure(CoreError(text: lastError()))
        }
        let lines = String(decoding: buf.prefix(written), as: UTF8.self).split(separator: "\n")
        guard lines.count == 2 else { return .failure(CoreError(text: "unexpected answer")) }
        return .success((lines[0] == "trusted", String(lines[1])))
    }

    nonisolated private static func trustServer(_ server: String, fingerprint: String) -> String? {
        let url = Array(server.utf8)
        let fp = Array(fingerprint.utf8)
        return nexa_signal_trust(url, url.count, fp, fp.count) == NEXA_OK ? nil : lastError()
    }

    nonisolated static func lastError() -> String {
        var buf = [UInt8](repeating: 0, count: 1024)
        var written = 0
        guard nexa_last_error(&buf, buf.count, &written) == NEXA_OK else { return "unknown error" }
        return String(decoding: buf.prefix(written), as: UTF8.self)
    }
}

/// The core's event callback: decodes on the calling thread, handles on the main actor.
private func nexaEvent(user: UnsafeMutableRawPointer?, pb: UnsafePointer<UInt8>?, len: Int) {
    guard let user, let pb else { return }
    let data = Data(bytes: pb, count: len)
    guard let event = try? Nexa_Client_Event(serializedBytes: data) else { return }
    let core = Unmanaged<NexaCore>.fromOpaque(user).takeUnretainedValue()
    Task { @MainActor in core.handle(event) }
}
