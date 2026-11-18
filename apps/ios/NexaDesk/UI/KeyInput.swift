import SwiftUI
import UIKit

/// USB HID usages (keyboard page 0x07) the app sends itself.
enum HID {
    static let enter: UInt32 = 0x28
    static let escape: UInt32 = 0x29
    static let backspace: UInt32 = 0x2A
    static let tab: UInt32 = 0x2B
    static let right: UInt32 = 0x4F
    static let left: UInt32 = 0x50
    static let down: UInt32 = 0x51
    static let up: UInt32 = 0x52
    static let leftCtrl: UInt32 = 0xE0
    static let leftShift: UInt32 = 0xE1
    static let leftAlt: UInt32 = 0xE2
    static let leftGui: UInt32 = 0xE3
}

/// A view that takes the keyboard while the session is shown.
///
/// - The on-screen keyboard types text (`insertText`), so any input method works;
///   Return and Delete become the Enter and Backspace keys.
/// - A hardware keyboard sends its keys as they are: UIKit reports each key's HID
///   usage, which is what the host injects, so shortcuts, arrows and modifiers work.
final class KeyInputUIView: UIView, UIKeyInput {
    weak var core: NexaCore?
    /// Modifiers latched on the keyboard bar, sent around the next key.
    var latched: [UInt32] = []
    var onLatchUsed: (() -> Void)?

    override var canBecomeFirstResponder: Bool { true }
    var hasText: Bool { true }
    var autocorrectionType: UITextAutocorrectionType = .no
    var autocapitalizationType: UITextAutocapitalizationType = .none
    var smartQuotesType: UITextSmartQuotesType = .no
    var smartDashesType: UITextSmartDashesType = .no
    var spellCheckingType: UITextSpellCheckingType = .no
    var keyboardType: UIKeyboardType = .asciiCapable

    func insertText(_ text: String) {
        guard let core else { return }
        if text == "\n" {
            withLatched { core.tapKey(HID.enter) }
        } else if !latched.isEmpty, text.count == 1, let usage = Self.usage(of: text) {
            // ⌘C from the bar: a key with modifiers, not text.
            withLatched { core.tapKey(usage) }
        } else {
            core.text(text)
        }
    }

    func deleteBackward() {
        withLatched { core?.tapKey(HID.backspace) }
    }

    /// Presses the latched modifiers around `body`, then releases them.
    func withLatched(_ body: () -> Void) {
        let mods = latched
        mods.forEach { core?.key($0, down: true) }
        body()
        mods.reversed().forEach { core?.key($0, down: false) }
        if !mods.isEmpty {
            latched = []
            onLatchUsed?()
        }
    }

    /// Letters and digits by HID usage, for shortcuts typed with a latched modifier.
    static func usage(of text: String) -> UInt32? {
        guard let c = text.lowercased().unicodeScalars.first, text.unicodeScalars.count == 1 else { return nil }
        switch c.value {
        case 0x61...0x7A: return 0x04 + (c.value - 0x61)  // a–z
        case 0x31...0x39: return 0x1E + (c.value - 0x31)  // 1–9
        case 0x30: return 0x27                            // 0
        case 0x20: return 0x2C                            // space
        default: return nil
        }
    }

    // MARK: Hardware keyboard

    override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        var handled = false
        for press in presses {
            if let key = press.key, let core {
                core.key(UInt32(key.keyCode.rawValue), down: true)
                handled = true
            }
        }
        if !handled { super.pressesBegan(presses, with: event) }
    }

    override func pressesEnded(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        var handled = false
        for press in presses {
            if let key = press.key, let core {
                core.key(UInt32(key.keyCode.rawValue), down: false)
                handled = true
            }
        }
        if !handled { super.pressesEnded(presses, with: event) }
    }

    override func pressesCancelled(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        pressesEnded(presses, with: event)
    }
}

/// Owns the key view so the session screen can show and hide the keyboard.
@MainActor
final class KeyInputController: ObservableObject {
    let view = KeyInputUIView()
    @Published var latched: Set<UInt32> = []

    init() {
        view.onLatchUsed = { [weak self] in self?.latched = [] }
    }

    func toggleKeyboard() {
        if view.isFirstResponder { view.resignFirstResponder() } else { view.becomeFirstResponder() }
    }

    func toggle(_ modifier: UInt32) {
        if latched.contains(modifier) { latched.remove(modifier) } else { latched.insert(modifier) }
        view.latched = Array(latched)
    }

    func tap(_ usage: UInt32) {
        view.withLatched { view.core?.tapKey(usage) }
    }
}

struct KeyInputHost: UIViewRepresentable {
    let controller: KeyInputController
    let core: NexaCore

    func makeUIView(context: Context) -> KeyInputUIView {
        controller.view.core = core
        DispatchQueue.main.async { controller.view.becomeFirstResponder() }
        return controller.view
    }

    func updateUIView(_ view: KeyInputUIView, context: Context) {}
}

/// Keys a phone keyboard does not have.
struct KeyboardBar: View {
    @ObservedObject var keys: KeyInputController

    var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 6) {
                key("esc") { keys.tap(HID.escape) }
                key("tab") { keys.tap(HID.tab) }
                modifier("⌃", HID.leftCtrl)
                modifier("⌥", HID.leftAlt)
                modifier("⌘", HID.leftGui)
                modifier("⇧", HID.leftShift)
                key("←") { keys.tap(HID.left) }
                key("↓") { keys.tap(HID.down) }
                key("↑") { keys.tap(HID.up) }
                key("→") { keys.tap(HID.right) }
                key("⌨︎") { keys.toggleKeyboard() }
            }
            .padding(.horizontal, 8)
        }
        .padding(.vertical, 6)
        .background(.ultraThinMaterial)
    }

    private func key(_ title: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Text(title).font(.system(size: 15, weight: .medium)).frame(minWidth: 38, minHeight: 32)
        }
        .buttonStyle(.bordered)
    }

    private func modifier(_ title: String, _ usage: UInt32) -> some View {
        let on = keys.latched.contains(usage)
        return Button { keys.toggle(usage) } label: {
            Text(title).font(.system(size: 15, weight: .semibold)).frame(minWidth: 38, minHeight: 32)
        }
        .buttonStyle(.bordered)
        .tint(on ? .accentColor : .secondary)
    }
}
