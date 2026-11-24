import CryptoKit
import Foundation
import Network

/// A NexaDesk host announced on this network.
struct LanHost: Identifiable, Equatable {
    let id: String          // the host's device ID (TXT `id`): 32 lowercase hex
    let name: String        // TXT `n`
    let os: String          // TXT `os`
    let endpoint: NWEndpoint

    static func == (a: LanHost, b: LanHost) -> Bool { a.id == b.id && a.endpoint == b.endpoint }
}

/// Lists hosts announcing `_nexadesk._udp` (crates/nexa-net/src/discovery.rs) through
/// Bonjour. The Rust core's own mDNS needs a multicast entitlement on iOS; the system's
/// browser needs only the local network permission and NSBonjourServices.
///
/// What is heard only fills in an address: who the host is is still settled by the
/// handshake and pairing, as for a typed address.
@MainActor
final class LanBrowser: ObservableObject {
    static let service = "_nexadesk._udp"

    @Published private(set) var hosts: [LanHost] = []
    private var browser: NWBrowser?

    func start() {
        guard browser == nil else { return }
        let browser = NWBrowser(for: .bonjourWithTXTRecord(type: Self.service, domain: nil), using: .udp)
        browser.browseResultsChangedHandler = { [weak self] results, _ in
            // One host is heard once per interface it announces on: keep one of each.
            var seen = Set<String>()
            let hosts = results.compactMap(Self.host(from:))
                .filter { seen.insert($0.id).inserted }
                .sorted { $0.name.localizedStandardCompare($1.name) == .orderedAscending }
            Task { @MainActor in self?.hosts = hosts }
        }
        browser.start(queue: .main)
        self.browser = browser
    }

    func stop() {
        browser?.cancel()
        browser = nil
        hosts = []
    }

    /// A record the core would take (crates/nexa-net/src/discovery.rs): format 1 and a
    /// device ID of 32 lowercase hex digits. Anything else is not a NexaDesk host.
    nonisolated static func host(fromTXT txt: [String: String], serviceName: String, endpoint: NWEndpoint) -> LanHost? {
        guard txt["v"] == "1", let id = txt["id"], isDeviceId(id) else { return nil }
        // A display name is the peer's own text: no control characters, and short.
        let name = (txt["n"] ?? serviceName).filter { !$0.isNewline && $0.unicodeScalars.allSatisfy { !CharacterSet.controlCharacters.contains($0) } }
        return LanHost(id: id, name: String(name.prefix(64)), os: txt["os"] ?? "", endpoint: endpoint)
    }

    nonisolated static func isDeviceId(_ s: String) -> Bool {
        s.count == 32 && s.allSatisfy { ("0"..."9").contains($0) || ("a"..."f").contains($0) }
    }

    /// A host's device ID from its public key (hex), as nexa-auth's `DevicePublicKey::device_id`:
    /// the first 16 bytes of SHA-256("nexa/1 device id" ‖ key), in lowercase hex. Nil for a key
    /// that is not hex. It is how a saved host is known again when its address has changed.
    nonisolated static func deviceId(ofHostKey hex: String) -> String? {
        guard hex.count % 2 == 0, !hex.isEmpty else { return nil }
        var key = Data(capacity: hex.count / 2)
        var at = hex.startIndex
        while at < hex.endIndex {
            let next = hex.index(at, offsetBy: 2)
            guard let byte = UInt8(hex[at..<next], radix: 16) else { return nil }
            key.append(byte)
            at = next
        }
        var hasher = SHA256()
        hasher.update(data: Data("nexa/1 device id".utf8))
        hasher.update(data: key)
        return hasher.finalize().prefix(16).map { String(format: "%02x", $0) }.joined()
    }

    nonisolated private static func host(from result: NWBrowser.Result) -> LanHost? {
        guard case let .service(serviceName, _, _, _) = result.endpoint else { return nil }
        var txt: [String: String] = [:]
        if case let .bonjour(record) = result.metadata {
            txt = record.dictionary
        }
        return host(fromTXT: txt, serviceName: serviceName, endpoint: result.endpoint)
    }

    /// The host's address and port, for `ConnectRequest.address`: IPv4 if it has one,
    /// else a routable IPv6 address (a link-local one needs an interface scope the
    /// core's parser does not take).
    static func resolve(_ host: LanHost) async -> String? {
        if let v4 = await resolve(host, version: .v4) { return v4 }
        guard let any = await resolve(host, version: .any) else { return nil }
        return any.hasPrefix("[fe80") || any.hasPrefix("[FE80") ? nil : any
    }

    /// Opening a UDP "connection" resolves the service without sending anything.
    private static func resolve(_ host: LanHost, version: NWProtocolIP.Options.Version, timeout: TimeInterval = 5) async -> String? {
        await withCheckedContinuation { (done: CheckedContinuation<String?, Never>) in
            let params = NWParameters.udp
            if let ip = params.defaultProtocolStack.internetProtocol as? NWProtocolIP.Options {
                ip.version = version
            }
            let connection = NWConnection(to: host.endpoint, using: params)
            let once = Once()
            let finish: @Sendable (String?) -> Void = { address in
                if once.claim() {
                    connection.cancel()
                    done.resume(returning: address)
                }
            }
            connection.stateUpdateHandler = { state in
                switch state {
                case .ready:
                    if case let .hostPort(h, p)? = connection.currentPath?.remoteEndpoint {
                        finish("\(Self.text(of: h)):\(p.rawValue)")
                    } else {
                        finish(nil)
                    }
                case .failed, .cancelled:
                    finish(nil)
                default:
                    break
                }
            }
            connection.start(queue: .global())
            DispatchQueue.global().asyncAfter(deadline: .now() + timeout) { finish(nil) }
        }
    }

    nonisolated private static func text(of host: NWEndpoint.Host) -> String {
        switch host {
        // Without the interface ("%en0"): the core parses plain addresses.
        case .ipv4(let a): return "\(a)".split(separator: "%").first.map(String.init) ?? "\(a)"
        case .ipv6(let a): return "[\("\(a)".split(separator: "%").first.map(String.init) ?? "\(a)")]"
        case .name(let n, _): return n
        @unknown default: return "\(host)"
        }
    }
}

/// Lets exactly one of several callbacks finish an operation.
private final class Once: @unchecked Sendable {
    private let lock = NSLock()
    private var done = false
    func claim() -> Bool {
        lock.lock(); defer { lock.unlock() }
        if done { return false }
        done = true
        return true
    }
}
