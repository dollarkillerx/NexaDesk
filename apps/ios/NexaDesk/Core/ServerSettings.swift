import Foundation
import Network

/// Which signaling server device IDs go through, as the desktop app offers it
/// (ui/NexaDesk.App/ViewModels/SettingsViewModel.cs): none, one from the published
/// list, or the person's own.
enum ServerMode: String, CaseIterable, Identifiable {
    /// Addresses, the LAN and Tailscale only.
    case none
    /// One of the servers in the project's servers.json; its key is public.
    case published
    /// An address and key the person types.
    case own

    var id: String { rawValue }
}

/// One server of the published list.
struct ServerEntry: Equatable, Identifiable {
    let endpoint: String
    let key: String
    var id: String { endpoint }
}

/// What looking at a server found (ui/NexaDesk.App/Services/ServerProbe.cs).
enum ProbeResult: Equatable {
    case testing
    case available(ms: Int)
    case unreachable
    case notSignal

    var text: String {
        switch self {
        case .testing: String(localized: "Testing…")
        case .available(let ms): String(localized: "Available · \(ms) ms")
        case .unreachable: String(localized: "Cannot connect")
        case .notSignal: String(localized: "Not a NexaDesk signaling server")
        }
    }
}

@MainActor
final class ServerSettings: ObservableObject {
    /// servers.json at the top of the project's repository, as the desktop app reads it.
    nonisolated static let listURL = URL(string: "https://raw.githubusercontent.com/dollarkillerx/NexaDesk/main/servers.json")!

    @Published var mode: ServerMode {
        didSet { UserDefaults.standard.set(mode.rawValue, forKey: "serverMode") }
    }
    /// The person's own server and its key (the key in the Keychain).
    @Published var ownServer: String {
        didSet { UserDefaults.standard.set(ownServer, forKey: "signalServer") }
    }
    @Published var ownKey: String
    /// The published server picked, with the key the list gave for it.
    @Published private(set) var published: ServerEntry?

    @Published private(set) var list: [ServerEntry] = []
    @Published private(set) var loadingList = false
    @Published private(set) var health: [String: ProbeResult] = [:]
    @Published private(set) var ownHealth: ProbeResult?
    /// Set when the Keychain would not keep a key.
    @Published var problem: String?

    init() {
        let defaults = UserDefaults.standard
        let own = defaults.string(forKey: "signalServer") ?? ""
        ownServer = own
        ownKey = SecretStore.load(.own)
        if let endpoint = defaults.string(forKey: "publishedServer") {
            let key = SecretStore.load(.published)
            published = key.isEmpty ? nil : ServerEntry(endpoint: endpoint, key: key)
        }
        // Before there was a choice, a typed server meant one's own.
        mode = defaults.string(forKey: "serverMode").flatMap(ServerMode.init) ?? (own.isEmpty ? .none : .own)
    }

    /// The server and key a device ID goes through now; `nil` with no server chosen.
    var active: (server: String, key: String)? {
        switch mode {
        case .none: nil
        case .published: published.map { ($0.endpoint, $0.key) }
        case .own: ownServer.trimmingCharacters(in: .whitespaces).isEmpty ? nil : (ownServer, ownKey)
        }
    }

    /// For the connect screen: the server in use, or that none is.
    var summary: String {
        active.map { NexaCore.normalizedServer($0.server) ?? $0.server } ?? String(localized: "Not used")
    }

    func saveOwnKey() {
        keep(SecretStore.save(ownKey.trimmingCharacters(in: .whitespacesAndNewlines), as: .own))
    }

    /// A published server is used as soon as it is picked, as on the desktop.
    func pick(_ entry: ServerEntry) {
        published = entry
        UserDefaults.standard.set(entry.endpoint, forKey: "publishedServer")
        keep(SecretStore.save(entry.key, as: .published))
    }

    private func keep(_ status: OSStatus) {
        if status != errSecSuccess {
            problem = String(localized: "The key could not be saved in the Keychain (error \(Int(status))).")
        }
    }

    // MARK: The published list

    func loadList() async {
        guard !loadingList else { return }
        loadingList = true
        defer { loadingList = false }
        list = await Self.fetchList()
        // The picked server's key follows the list: a changed key is the list's to give.
        if let picked = published, let fresh = list.first(where: { $0.endpoint == picked.endpoint }), fresh.key != picked.key {
            pick(fresh)
        }
        await withTaskGroup(of: (String, ProbeResult).self) { group in
            for entry in list {
                health[entry.endpoint] = .testing
                group.addTask { (entry.endpoint, await Self.probe(entry.endpoint)) }
            }
            for await (endpoint, result) in group { health[endpoint] = result }
        }
    }

    func testOwn() async {
        guard let server = NexaCore.normalizedServer(ownServer) else {
            ownHealth = .unreachable
            return
        }
        ownHealth = .testing
        ownHealth = await Self.probe(server)
    }

    nonisolated private static var cacheURL: URL {
        FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("NexaDesk/servers.json")
    }

    /// The file as it is now, else the one kept from the last time (ServerDirectory.cs:
    /// https only, 8 s, at most 64 KB; a file of another kind changes nothing).
    nonisolated private static func fetchList() async -> [ServerEntry] {
        var request = URLRequest(url: listURL, timeoutInterval: 8)
        request.cachePolicy = .reloadIgnoringLocalCacheData
        if let (data, response) = try? await URLSession.shared.data(for: request),
           (response as? HTTPURLResponse)?.statusCode == 200,
           let fresh = parse(data) {
            try? FileManager.default.createDirectory(at: cacheURL.deletingLastPathComponent(), withIntermediateDirectories: true)
            try? data.write(to: cacheURL, options: .atomic)
            return fresh
        }
        guard let kept = try? Data(contentsOf: cacheURL) else { return [] }
        return parse(kept) ?? []
    }

    nonisolated static let maxBytes = 1 << 16
    nonisolated static let maxEntries = 16

    /// The valid entries, or `nil` when it is not this kind of file (ServerList.Parse).
    nonisolated static func parse(_ data: Data) -> [ServerEntry]? {
        guard !data.isEmpty, data.count <= maxBytes,
              let root = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let items = root["endpoints"] as? [Any] else { return nil }
        var entries: [ServerEntry] = []
        for item in items {
            guard entries.count < maxEntries else { break }
            guard let o = item as? [String: Any], let endpoint = o["endpoint"] as? String,
                  let key = (o["key"] as? String)?.trimmingCharacters(in: .whitespacesAndNewlines),
                  !key.isEmpty, !key.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) }),
                  let normalized = NexaCore.normalized(NEXA_NORMALIZE_SERVER, endpoint),
                  normalized.hasPrefix("wss://"),
                  !entries.contains(where: { $0.endpoint == normalized }) else { continue }
            entries.append(ServerEntry(endpoint: normalized, key: key))
        }
        return entries
    }

    // MARK: Probe

    /// The delay is the median of three TCP connects (one round trip); whether it is a
    /// signaling server is told by its first message, the challenge it sends as soon as
    /// a WebSocket opens. Nothing is sent: no key, no device identity.
    nonisolated static func probe(_ endpoint: String) async -> ProbeResult {
        guard let url = URL(string: endpoint), let host = url.host,
              url.scheme == "wss" || url.scheme == "ws" else { return .unreachable }
        let port = url.port ?? (url.scheme == "wss" ? 443 : 80)
        var times: [Double] = []
        for _ in 0..<3 {
            guard let t = await connectTime(host: host, port: port) else { break }
            times.append(t)
        }
        guard times.count == 3 else { return .unreachable }
        let ms = max(1, Int(times.sorted()[1].rounded()))
        return await sendsChallenge(url) ? .available(ms: ms) : .notSignal
    }

    nonisolated private static func connectTime(host: String, port: Int) async -> Double? {
        guard let p = NWEndpoint.Port(rawValue: UInt16(clamping: port)) else { return nil }
        return await withCheckedContinuation { (done: CheckedContinuation<Double?, Never>) in
            let connection = NWConnection(host: NWEndpoint.Host(host), port: p, using: .tcp)
            let once = ProbeOnce()
            let start = DispatchTime.now()
            let finish: @Sendable (Double?) -> Void = { value in
                if once.claim() {
                    connection.cancel()
                    done.resume(returning: value)
                }
            }
            connection.stateUpdateHandler = { state in
                switch state {
                case .ready:
                    finish(Double(DispatchTime.now().uptimeNanoseconds - start.uptimeNanoseconds) / 1_000_000)
                case .failed, .cancelled, .waiting:
                    finish(nil)
                default:
                    break
                }
            }
            connection.start(queue: .global())
            DispatchQueue.global().asyncAfter(deadline: .now() + 5) { finish(nil) }
        }
    }

    /// Through Network.framework, not URLSession: App Transport Security turns a server
    /// with a certificate of its own down before a URLSession delegate is asked. The
    /// probe sends nothing secret, so it accepts any certificate; that certificate is
    /// checked (trust on first use) when the server is used.
    nonisolated private static func sendsChallenge(_ url: URL) async -> Bool {
        let params: NWParameters
        if url.scheme == "wss" {
            let tls = NWProtocolTLS.Options()
            sec_protocol_options_set_verify_block(tls.securityProtocolOptions, { _, _, accept in accept(true) }, .global())
            params = NWParameters(tls: tls)
        } else {
            params = .tcp
        }
        params.defaultProtocolStack.applicationProtocols.insert(NWProtocolWebSocket.Options(), at: 0)
        return await withCheckedContinuation { (done: CheckedContinuation<Bool, Never>) in
            let connection = NWConnection(to: .url(url), using: params)
            let once = ProbeOnce()
            let finish: @Sendable (Bool) -> Void = { ok in
                if once.claim() {
                    connection.cancel()
                    done.resume(returning: ok)
                }
            }
            connection.stateUpdateHandler = { state in
                switch state {
                case .ready:
                    // The server's first message: a challenge, binary, at least a protocol
                    // number and a 32-byte nonce.
                    connection.receiveMessage { data, context, _, error in
                        let meta = context?.protocolMetadata(definition: NWProtocolWebSocket.definition) as? NWProtocolWebSocket.Metadata
                        finish(error == nil && meta?.opcode == .binary && (data?.count ?? 0) >= 32)
                    }
                case .failed, .cancelled, .waiting:
                    finish(false)
                default:
                    break
                }
            }
            connection.start(queue: .global())
            DispatchQueue.global().asyncAfter(deadline: .now() + 8) { finish(false) }
        }
    }
}

private final class ProbeOnce: @unchecked Sendable {
    private let lock = NSLock()
    private var done = false
    func claim() -> Bool {
        lock.lock(); defer { lock.unlock() }
        if done { return false }
        done = true
        return true
    }
}
