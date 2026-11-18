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
    /// The host's displays, from `Connected`; one is streamed at a time.
    @Published private(set) var displays: [Nexa_Client_DisplayInfo] = []
    /// The display being streamed (`StreamChanged.display`).
    @Published private(set) var display: UInt32 = 0
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
    /// Bumped to drop the answer of a server check the person cancelled.
    private var checkGeneration: UInt64 = 0

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
            notice = String(localized: "Could not start: \(Self.lastError())")
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

    /// `s` in the core's one form (`nexa_normalize`), so this app follows the same rules
    /// as the others; `nil` if it is not one.
    nonisolated static func normalized(_ kind: UInt32, _ s: String) -> String? {
        let input = Array(s.utf8)
        var out = [UInt8](repeating: 0, count: 512)
        var written = 0
        guard nexa_normalize(kind, input, input.count, &out, out.count, &written) == NEXA_OK else { return nil }
        return String(decoding: out.prefix(written), as: UTF8.self)
    }

    /// The one-time code as the host shows it (8 characters of A–Z and 2–7, any case,
    /// spaces and hyphens ignored); `nil`: not a code, so a password.
    static func normalizedCode(_ s: String) -> String? { normalized(NEXA_NORMALIZE_CODE, s) }

    /// wss://host:port; "host", "host:port" and a browser's https:// address work too.
    /// `nil` if it can not be a server.
    static func normalizedServer(_ s: String) -> String? { normalized(NEXA_NORMALIZE_SERVER, s) }

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
            if signalServer.trimmingCharacters(in: .whitespaces).isEmpty {
                notice = String(localized: "A device ID needs a signaling server: enter yours below.")
                return
            }
            guard let server = Self.normalizedServer(signalServer) else {
                notice = String(localized: "That is not a signaling server address. Use signal.example.com, signal.example.com:8443 or wss://signal.example.com:8443.")
                return
            }
            req.signalServer = server
        }
        req.tag = tag
        req.mode = .balanced
        // A one-time code if it can be one (the desktop's rule: a fixed password may not
        // look like a code); anything else is the host's fixed password.
        let s = secret.trimmingCharacters(in: .whitespaces)
        if let code = Self.normalizedCode(s) {
            req.code = code
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
            phase = .connecting(String(localized: "Connecting…"))
        } else {
            notice = Self.lastError()
        }
    }

    func disconnect() {
        // A server check under way is no longer wanted.
        checkGeneration &+= 1
        if let session { nexa_session_close(session) }
        session = nil
        phase = .idle
        inputAllowed = false
        stream = (0, 0)
        displays = []
    }

    // MARK: Video

    /// Whether the picture now goes to `layer`; on failure the view tries again.
    @discardableResult
    func attach(layer: CAMetalLayer, widthPx: Int, heightPx: Int, scale: CGFloat) -> Bool {
        guard let session else { return false }
        let rc = nexa_session_attach_view(session, Unmanaged.passUnretained(layer).toOpaque(),
                                          NEXA_VIEW_CAMETALLAYER, UInt32(widthPx), UInt32(heightPx), Float(scale))
        if rc != NEXA_OK {
            notice = String(localized: "Video: \(Self.lastError())")
            return false
        }
        return true
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

    /// Streams another of the host's displays instead of this one (never both).
    func selectDisplay(_ id: UInt32) {
        guard id != display else { return }
        send(evenWithoutInput: true) { $0.display = .with { $0.display = id } }
    }

    private func send(evenWithoutInput: Bool = false, _ fill: (inout Nexa_Client_SessionCommand) -> Void) {
        guard let session, inputAllowed || evenWithoutInput else { return }
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
            phase = .connecting(String(localized: "Connecting…"))
        case .connected(let c):
            hostName = c.hostName
            displays = c.displays
            phase = .connected
            if let session, c.perms.input {
                inputAllowed = nexa_session_set_input_active(session, true) == NEXA_OK
            }
        case .stream(let s):
            stream = (Int(s.w), Int(s.h))
            display = s.display
        case .reconnecting(let r):
            phase = .reconnecting(r.reason.isEmpty ? String(localized: "Reconnecting…") : String(localized: "Reconnecting: \(r.reason)"))
        case .disconnected(let d):
            session = nil
            phase = .idle
            inputAllowed = false
            if d.reason == .disconnectSignalServer, let last = lastConnect, Self.isDeviceId(last.address),
               let server = Self.normalizedServer(last.server) {
                // A self-hosted server's own certificate is the usual cause: offer to trust it.
                // Looking can take up to half a minute when the server is down: say so.
                phase = .connecting(String(localized: "Checking the signaling server…"))
                checkServer(server, otherwise: Messages.disconnected(d))
            } else if d.reason != .disconnectClosedLocally {
                notice = Messages.disconnected(d)
            }
        case .warning(let w):
            notice = w.text
        default:
            break
        }
    }

    // MARK: Signaling server trust (signal-ca.pem in the data directory)

    private func checkServer(_ server: String, otherwise message: String) {
        checkGeneration &+= 1
        let generation = checkGeneration
        Task.detached {
            let answer = Self.serverCertificate(server)
            await MainActor.run {
                // Cancelled, or another connection started meanwhile.
                guard generation == self.checkGeneration, self.session == nil else { return }
                self.phase = .idle
                switch answer {
                case .success(let (trusted, fingerprint)) where !trusted:
                    self.trustPrompt = TrustPrompt(server: server, fingerprint: fingerprint)
                case .success:
                    self.notice = message
                case .failure(let e):
                    self.notice = "\(message)\n\n(\(e.text))"
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
                    self.notice = String(localized: "Could not trust the server: \(error)")
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
