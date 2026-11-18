import Foundation

/// A host connected to before (ui/NexaDesk.App/Services/Abstractions.cs RecentDevice).
/// No secret is kept: a host paired with a one-time code signs in with its key, which
/// the core keeps.
struct RecentHost: Codable, Equatable, Identifiable {
    /// host:port or a 9-digit device ID, as connected to.
    var address: String
    /// The host's own name.
    var name: String
    /// Its public key (hex), once connected; what its pairing is filed under.
    var hostKey: String
    /// 0 unknown, 1 Windows, 2 macOS, 3 Linux.
    var os: UInt32
    var lastConnected: Date
    var favorite = false
    /// A name of the person's own; empty: the host's.
    var label = ""

    var id: String { address + "|" + hostKey }

    var title: String {
        if !label.isEmpty { return label }
        return name.isEmpty ? shownAddress : name
    }

    /// A device ID in groups of three, as the computer shows it.
    var shownAddress: String {
        guard NexaCore.isDeviceId(address) else { return address }
        let d = Array(address)
        return "\(String(d[0..<3])) \(String(d[3..<6])) \(String(d[6..<9]))"
    }

    var symbol: String {
        switch os {
        case 1: "pc"
        case 2: "desktopcomputer"
        default: "display"
        }
    }
}

@MainActor
final class RecentHosts: ObservableObject {
    private static let key = "recentHosts"
    /// Recent ones kept besides the favorites, which are kept until removed.
    static let limit = 20

    @Published private(set) var hosts: [RecentHost] = []

    init() {
        if let data = UserDefaults.standard.data(forKey: Self.key),
           let saved = try? JSONDecoder().decode([RecentHost].self, from: data) {
            hosts = saved
        }
    }

    var favorites: [RecentHost] { hosts.filter(\.favorite) }
    var others: [RecentHost] { hosts.filter { !$0.favorite } }

    /// After a connection: first in the list, keeping the person's name and star for it.
    func remember(address: String, name: String, hostKey: String, os: UInt32) {
        let same: (RecentHost) -> Bool = { $0.address == address || (!hostKey.isEmpty && $0.hostKey == hostKey) }
        let before = hosts.first(where: same)
        hosts.removeAll(where: same)
        var entry = RecentHost(address: address, name: name, hostKey: hostKey, os: os, lastConnected: Date())
        entry.favorite = before?.favorite ?? false
        entry.label = before?.label ?? ""
        hosts.insert(entry, at: 0)
        var kept = 0
        hosts.removeAll { h in
            guard !h.favorite else { return false }
            kept += 1
            return kept > Self.limit
        }
        save()
    }

    func toggleFavorite(_ host: RecentHost) {
        update(host) { $0.favorite.toggle() }
    }

    func rename(_ host: RecentHost, to label: String) {
        update(host) { $0.label = label.trimmingCharacters(in: .whitespacesAndNewlines) }
    }

    func remove(_ host: RecentHost) {
        hosts.removeAll { $0.id == host.id }
        save()
    }

    private func update(_ host: RecentHost, _ change: (inout RecentHost) -> Void) {
        guard let at = hosts.firstIndex(where: { $0.id == host.id }) else { return }
        change(&hosts[at])
        save()
    }

    private func save() {
        if let data = try? JSONEncoder().encode(hosts) {
            UserDefaults.standard.set(data, forKey: Self.key)
        }
    }
}
