import Foundation
import Security

/// Signaling server keys, kept in the Keychain (only on this device, readable once it
/// has been unlocked after a restart) rather than in UserDefaults with the addresses.
enum SecretStore {
    enum Account: String {
        /// The key of the person's own server.
        case own = "access-password"
        /// The key the published list gave for the server picked from it.
        case published = "published-key"
    }

    private static let service = "cc.nyxnet.nexadesk.signal"

    private static func query(_ account: Account) -> [String: Any] {
        [kSecClass as String: kSecClassGenericPassword,
         kSecAttrService as String: service,
         kSecAttrAccount as String: account.rawValue]
    }

    static func load(_ account: Account) -> String {
        var q = query(account)
        q[kSecReturnData as String] = true
        q[kSecMatchLimit as String] = kSecMatchLimitOne
        var item: CFTypeRef?
        guard SecItemCopyMatching(q as CFDictionary, &item) == errSecSuccess, let data = item as? Data else { return "" }
        return String(decoding: data, as: UTF8.self)
    }

    /// Keeps `secret`, or forgets it when it is empty. Returns the Keychain's error, if any.
    @discardableResult
    static func save(_ secret: String, as account: Account) -> OSStatus {
        SecItemDelete(query(account) as CFDictionary)
        guard !secret.isEmpty else { return errSecSuccess }
        var q = query(account)
        q[kSecValueData as String] = Data(secret.utf8)
        q[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        return SecItemAdd(q as CFDictionary, nil)
    }
}
