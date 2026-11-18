import SwiftUI

/// Keys and shortcuts a phone keyboard does not have, one tap each: the shortcuts of
/// the host's system, F1–F12 and the navigation keys. Opened from the session's
/// controls; it stays open, so several can be sent in a row.
///
/// No Ctrl+Alt+Del: Windows takes it only from a real keyboard, so an injected one
/// does nothing. Ctrl+Shift+Esc opens the Task Manager instead.
struct ShortcutsView: View {
    @EnvironmentObject private var core: NexaCore
    @Environment(\.dismiss) private var dismiss
    private let feedback = UIImpactFeedbackGenerator(style: .light)

    struct Shortcut: Identifiable {
        let title: String
        let detail: LocalizedStringKey?
        let keys: [UInt32]
        /// Sent as written, past the keymap rules. This device counts as a Mac, so the
        /// Mac→Windows rules would turn ⊞ (the gui key, ⌘ to them) plus a letter into
        /// Ctrl plus the letter. The others stay on the rules: Ctrl+V needs the paste
        /// tag, which makes it wait for the clipboard.
        var exact = false
        var id: String { title }
    }

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 20) {
                    group("Shortcuts", items: core.hostOs == 2 ? Self.mac : Self.windows, minWidth: 104)
                    group("Function keys", items: Self.functionKeys, minWidth: 64)
                    group("Navigation", items: Self.navigation, minWidth: 64)
                }
                .padding(16)
            }
            .navigationTitle("Keys")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { dismiss() }
                }
            }
        }
        .presentationDetents([.medium, .large])
        .presentationDragIndicator(.visible)
    }

    private func group(_ title: LocalizedStringKey, items: [Shortcut], minWidth: CGFloat) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title).font(.footnote.weight(.semibold)).foregroundStyle(.secondary)
            LazyVGrid(columns: [GridItem(.adaptive(minimum: minWidth), spacing: 8)], spacing: 8) {
                ForEach(items) { item in
                    Button {
                        feedback.impactOccurred()
                        core.combo(item.keys, exact: item.exact)
                    } label: {
                        VStack(spacing: 2) {
                            Text(item.title).font(.system(size: 16, weight: .medium)).lineLimit(1).minimumScaleFactor(0.7)
                            if let detail = item.detail {
                                Text(detail).font(.caption2).foregroundStyle(.secondary).lineLimit(1).minimumScaleFactor(0.7)
                            }
                        }
                        .frame(maxWidth: .infinity, minHeight: 44)
                    }
                    .buttonStyle(.bordered)
                }
            }
        }
    }

    // MARK: Keys (USB HID usages, keyboard page)

    private static let ctrl: UInt32 = 0xE0, shift: UInt32 = 0xE1, alt: UInt32 = 0xE2, gui: UInt32 = 0xE3
    private static func letter(_ c: Character) -> UInt32 { 0x04 + UInt32(c.asciiValue! - 97) }

    static let windows: [Shortcut] = [
        Shortcut(title: "⊞", detail: "Start", keys: [gui]),
        Shortcut(title: "Alt+Tab", detail: "Switch apps", keys: [alt, HID.tab]),
        Shortcut(title: "⊞+D", detail: "Desktop", keys: [gui, letter("d")], exact: true),
        Shortcut(title: "⊞+E", detail: "Files", keys: [gui, letter("e")], exact: true),
        Shortcut(title: "⊞+L", detail: "Lock", keys: [gui, letter("l")], exact: true),
        Shortcut(title: "Ctrl+Shift+Esc", detail: "Task Manager", keys: [ctrl, shift, HID.escape]),
        Shortcut(title: "Alt+F4", detail: "Close", keys: [alt, 0x3D]),
        Shortcut(title: "⊞+Shift+S", detail: "Screenshot", keys: [gui, shift, letter("s")], exact: true),
        Shortcut(title: "Ctrl+C", detail: "Copy", keys: [ctrl, letter("c")]),
        Shortcut(title: "Ctrl+V", detail: "Paste", keys: [ctrl, letter("v")]),
        Shortcut(title: "Ctrl+X", detail: "Cut", keys: [ctrl, letter("x")]),
        Shortcut(title: "Ctrl+Z", detail: "Undo", keys: [ctrl, letter("z")]),
        Shortcut(title: "Ctrl+A", detail: "Select all", keys: [ctrl, letter("a")]),
        Shortcut(title: "Ctrl+S", detail: "Save", keys: [ctrl, letter("s")]),
        Shortcut(title: "Ctrl+F", detail: "Find", keys: [ctrl, letter("f")]),
    ]

    static let mac: [Shortcut] = [
        Shortcut(title: "⌘Tab", detail: "Switch apps", keys: [gui, HID.tab]),
        Shortcut(title: "⌘Space", detail: "Spotlight", keys: [gui, 0x2C]),
        Shortcut(title: "⌃↑", detail: "Mission Control", keys: [ctrl, HID.up]),
        Shortcut(title: "⌃⌘Q", detail: "Lock", keys: [ctrl, gui, letter("q")]),
        Shortcut(title: "⌥⌘Esc", detail: "Force Quit", keys: [alt, gui, HID.escape]),
        Shortcut(title: "⌘W", detail: "Close", keys: [gui, letter("w")]),
        Shortcut(title: "⌘Q", detail: "Quit", keys: [gui, letter("q")]),
        Shortcut(title: "⇧⌘4", detail: "Screenshot", keys: [shift, gui, 0x21]),
        Shortcut(title: "⌘C", detail: "Copy", keys: [gui, letter("c")]),
        Shortcut(title: "⌘V", detail: "Paste", keys: [gui, letter("v")]),
        Shortcut(title: "⌘X", detail: "Cut", keys: [gui, letter("x")]),
        Shortcut(title: "⌘Z", detail: "Undo", keys: [gui, letter("z")]),
        Shortcut(title: "⌘A", detail: "Select all", keys: [gui, letter("a")]),
        Shortcut(title: "⌘S", detail: "Save", keys: [gui, letter("s")]),
        Shortcut(title: "⌘F", detail: "Find", keys: [gui, letter("f")]),
    ]

    static let functionKeys: [Shortcut] = (1...12).map {
        Shortcut(title: "F\($0)", detail: nil, keys: [0x3A + UInt32($0 - 1)])
    }

    static let navigation: [Shortcut] = [
        Shortcut(title: "Esc", detail: nil, keys: [HID.escape]),
        Shortcut(title: "Tab", detail: nil, keys: [HID.tab]),
        Shortcut(title: "Del", detail: nil, keys: [0x4C]),
        Shortcut(title: "Ins", detail: nil, keys: [0x49]),
        Shortcut(title: "Home", detail: nil, keys: [0x4A]),
        Shortcut(title: "End", detail: nil, keys: [0x4D]),
        Shortcut(title: "PgUp", detail: nil, keys: [0x4B]),
        Shortcut(title: "PgDn", detail: nil, keys: [0x4E]),
        Shortcut(title: "←", detail: nil, keys: [HID.left]),
        Shortcut(title: "↑", detail: nil, keys: [HID.up]),
        Shortcut(title: "↓", detail: nil, keys: [HID.down]),
        Shortcut(title: "→", detail: nil, keys: [HID.right]),
        Shortcut(title: "PrtSc", detail: nil, keys: [0x46]),
    ]
}
