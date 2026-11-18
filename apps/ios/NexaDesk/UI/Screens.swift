import SwiftUI
import UIKit
import Security

@main
struct NexaDeskApp: App {
    @StateObject private var core = NexaCore()
    @StateObject private var servers = ServerSettings()

    var body: some Scene {
        WindowGroup {
            RootView().environmentObject(core).environmentObject(core.recents).environmentObject(servers)
        }
    }
}

struct RootView: View {
    @EnvironmentObject private var core: NexaCore

    var body: some View {
        switch core.phase {
        case .idle:
            ConnectView()
        case .connecting(let text):
            ProgressView(text).toolbar { }
                .overlay(alignment: .bottom) {
                    Button("Cancel", role: .cancel) { core.disconnect() }.padding(.bottom, 40)
                }
        case .connected, .reconnecting:
            SessionView()
        }
    }
}

struct ConnectView: View {
    #if DEBUG
    @MainActor static var autoConnected = false
    #endif
    @EnvironmentObject private var core: NexaCore
    @EnvironmentObject private var servers: ServerSettings
    @EnvironmentObject private var recents: RecentHosts
    @StateObject private var lan = LanBrowser()
    @State private var renaming: RecentHost?
    @State private var newName = ""
    @AppStorage("address") private var address = ""
    @State private var secret = ""
    @State private var revealSecret = false
    @State private var resolving: String?
    @FocusState private var secretFocused: Bool

    private var isDeviceId: Bool { NexaCore.isDeviceId(address) }

    var body: some View {
        NavigationStack {
            Form {
                if !recents.favorites.isEmpty {
                    Section {
                        ForEach(recents.favorites) { recentRow($0) }
                    } header: {
                        Text("Favorites")
                    }
                }
                if !recents.others.isEmpty {
                    Section {
                        ForEach(recents.others) { recentRow($0) }
                    } header: {
                        Text("Recent")
                    } footer: {
                        if recents.favorites.isEmpty {
                            Text("Swipe right to add one to Favorites, left to remove it.")
                        }
                    }
                }

                Section {
                    if lan.hosts.isEmpty {
                        Text("Looking for computers sharing NexaDesk on this network…")
                            .font(.footnote).foregroundStyle(.secondary)
                    }
                    ForEach(lan.hosts) { host in
                        Button {
                            pick(host)
                        } label: {
                            HStack {
                                Image(systemName: host.os.lowercased().contains("win") ? "pc" : "desktopcomputer")
                                    .foregroundStyle(.secondary)
                                Text(host.name).foregroundStyle(.primary)
                                Spacer()
                                if resolving == host.id { ProgressView() }
                            }
                        }
                    }
                } header: {
                    Text("On this network")
                }

                Section {
                    TextField("Device ID, IP or host:port", text: $address)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .keyboardType(.asciiCapable)
                        .submitLabel(.next)
                        .onSubmit { secretFocused = true }
                    HStack {
                        Group {
                            if revealSecret {
                                TextField(isDeviceId ? "One-time code, password, or empty to ask" : "One-time code or password", text: $secret)
                            } else {
                                SecureField(isDeviceId ? "One-time code, password, or empty to ask" : "One-time code or password", text: $secret)
                            }
                        }
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .textContentType(.oneTimeCode)
                        // Codes and passwords are ASCII; a kana or pinyin keyboard would
                        // leave them as unconfirmed input.
                        .keyboardType(.asciiCapable)
                        .focused($secretFocused)
                        .submitLabel(.go)
                        .onSubmit { if canConnect { connect() } }
                        Button {
                            revealSecret.toggle()
                        } label: {
                            Image(systemName: revealSecret ? "eye.slash" : "eye")
                        }
                        .buttonStyle(.borderless)
                        .accessibilityLabel(revealSecret ? "Hide" : "Show")
                    }
                } header: {
                    Text("Computer")
                } footer: {
                    Text("On the same network (or Tailscale), an IP address is enough; port \(String(NexaCore.defaultPort)) is assumed. From anywhere else, use the 9-digit device ID with a signaling server.")
                }

                // Only device IDs go through it: one row, with the choices on a page of
                // their own (as the desktop app's three).
                Section {
                    NavigationLink {
                        ServerSettingsView()
                    } label: {
                        LabeledContent("Signaling server", value: servers.summary)
                    }
                } footer: {
                    Text("Needed only to connect by 9-digit device ID.")
                }
            }
            // At the bottom, where the thumb is, and above the keyboard while typing.
            .safeAreaInset(edge: .bottom) {
                Button { connect() } label: {
                    Text("Connect").font(.headline).frame(maxWidth: .infinity, minHeight: 50)
                }
                .buttonStyle(.borderedProminent)
                .buttonBorderShape(.roundedRectangle(radius: 14))
                .disabled(!canConnect)
                .padding(.horizontal, 20)
                .padding(.vertical, 8)
                .background(.bar)
            }
            .navigationTitle(Text(verbatim: "NexaDesk"))
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) {
                    NavigationLink {
                        AboutView()
                    } label: {
                        Image(systemName: "info.circle")
                    }
                    .accessibilityLabel("About NexaDesk")
                }
            }
            .onAppear {
                lan.start()
                #if DEBUG
                // Simulator tests: `SIMCTL_CHILD_NEXA_TEST_ADDRESS=… SIMCTL_CHILD_NEXA_TEST_SECRET=…
                // xcrun simctl launch …` fills the form and connects (the simulator's typing
                // tool mangles secure fields). NEXA_TEST_SERVER sets the signaling server.
                // Once per launch: this screen reappears after every failed attempt, and
                // connecting again each time hammered the host until it rate-limited.
                let env = ProcessInfo.processInfo.environment
                if let server = env["NEXA_TEST_SERVER"] {
                    servers.mode = .own
                    servers.ownServer = server
                    servers.ownKey = env["NEXA_TEST_SIGNAL_SECRET"] ?? ""
                }
                if !ConnectView.autoConnected, let a = env["NEXA_TEST_ADDRESS"] {
                    ConnectView.autoConnected = true
                    let s = env["NEXA_TEST_SECRET"] ?? ""
                    address = a
                    secret = s
                    connect()
                }
                #endif
            }
            .onDisappear { lan.stop() }
            .alert("Rename", isPresented: Binding(get: { renaming != nil }, set: { if !$0 { renaming = nil } })) {
                TextField(renaming?.name ?? "", text: $newName)
                Button("Save") {
                    if let host = renaming { recents.rename(host, to: newName) }
                }
                Button("Cancel", role: .cancel) {}
            } message: {
                Text("Empty uses the computer's own name.")
            }
            .alert(Text(verbatim: "NexaDesk"), isPresented: Binding(get: { core.notice != nil }, set: { if !$0 { core.notice = nil } })) {
                Button("OK", role: .cancel) {}
            } message: {
                Text(core.notice ?? "")
            }
            .alert(
                "Trust this signaling server?",
                isPresented: Binding(get: { core.trustPrompt != nil }, set: { if !$0 { core.trustPrompt = nil } }),
                presenting: core.trustPrompt
            ) { prompt in
                Button("Trust and connect") { core.trust(prompt) }
                Button("Cancel", role: .cancel) {}
            } message: { prompt in
                Text("\(prompt.server) uses a certificate this device does not know. Trust it only if this fingerprint matches the server's certificate (on the server: openssl x509 -noout -fingerprint -sha256 -in <certificate>):\n\n\(prompt.fingerprint)")
            }
        }
    }

    private var canConnect: Bool { !address.trimmingCharacters(in: .whitespaces).isEmpty }

    private func connect() {
        let server = servers.active
        if isDeviceId, server == nil {
            core.notice = String(localized: "A device ID needs a signaling server: choose one under Signaling server.")
            return
        }
        core.connect(address: address, signalServer: server?.server ?? "", serverPassword: server?.key ?? "", secret: secret)
    }

    // MARK: Recent and favorite hosts

    private func recentRow(_ host: RecentHost) -> some View {
        let paired = core.pairedKeys.contains(host.hostKey)
        return Button {
            open(host)
        } label: {
            HStack(spacing: 12) {
                Image(systemName: host.symbol)
                    .font(.title3)
                    .foregroundStyle(Color.accentColor)
                    .frame(width: 28)
                VStack(alignment: .leading, spacing: 2) {
                    Text(host.title).foregroundStyle(.primary).lineLimit(1)
                    HStack(spacing: 4) {
                        Text(host.shownAddress)
                        if paired {
                            Image(systemName: "lock.fill").imageScale(.small)
                                .accessibilityLabel("Paired")
                        }
                    }
                    .font(.footnote).foregroundStyle(.secondary).lineLimit(1)
                }
                Spacer(minLength: 8)
                Text(host.lastConnected, format: .relative(presentation: .named))
                    .font(.caption).foregroundStyle(.secondary)
            }
            .frame(minHeight: 44)
            .contentShape(Rectangle())
        }
        .tint(.primary)
        .swipeActions(edge: .leading) {
            Button {
                recents.toggleFavorite(host)
            } label: {
                Label(host.favorite ? "Unfavorite" : "Favorite", systemImage: host.favorite ? "star.slash" : "star")
            }
            .tint(.yellow)
        }
        .swipeActions(edge: .trailing) {
            Button(role: .destructive) {
                recents.remove(host)
            } label: {
                Label("Remove", systemImage: "trash")
            }
        }
        .contextMenu {
            Button {
                recents.toggleFavorite(host)
            } label: {
                Label(host.favorite ? "Unfavorite" : "Favorite", systemImage: host.favorite ? "star.slash" : "star")
            }
            Button {
                newName = host.label
                renaming = host
            } label: {
                Label("Rename", systemImage: "pencil")
            }
            Button(role: .destructive) {
                recents.remove(host)
            } label: {
                Label("Remove", systemImage: "trash")
            }
        }
    }

    /// One tap: a paired host signs in with its pairing; a device ID with nothing else
    /// asks the person at the computer; anything else needs its code, so the form is
    /// filled and the code field waits.
    private func open(_ host: RecentHost) {
        let isId = NexaCore.isDeviceId(host.address)
        let server = servers.active
        if isId, server == nil {
            core.notice = String(localized: "A device ID needs a signaling server: choose one under Signaling server.")
            return
        }
        if core.pairedKeys.contains(host.hostKey) {
            UIImpactFeedbackGenerator(style: .light).impactOccurred()
            // Filled in, should the computer have dropped the pairing and want a code.
            address = host.address
            core.connect(address: host.address, signalServer: server?.server ?? "", serverPassword: server?.key ?? "",
                         secret: "", knownHost: host.hostKey)
        } else if isId {
            UIImpactFeedbackGenerator(style: .light).impactOccurred()
            core.connect(address: host.address, signalServer: server?.server ?? "", serverPassword: server?.key ?? "",
                         secret: "")
        } else {
            address = host.address
            secret = ""
            secretFocused = true
        }
    }

    /// A host heard on the LAN: its address goes in the form, then the code or password.
    private func pick(_ host: LanHost) {
        resolving = host.id
        Task {
            let resolved = await LanBrowser.resolve(host)
            resolving = nil
            if let resolved {
                // Paired before at this address: no code needed (the desktop's UseAddress).
                if let known = recents.hosts.first(where: { $0.address == resolved && core.pairedKeys.contains($0.hostKey) }) {
                    open(known)
                    return
                }
                address = resolved
                secretFocused = true
            } else {
                core.notice = String(localized: "\(host.name) could not be reached on this network.")
            }
        }
    }
}

struct SessionView: View {
    /// "Studio Display (3840×2160)", or "Display 2 (…)" when the host gives no name.
    static func title(of d: Nexa_Client_DisplayInfo) -> String {
        let name = d.name.isEmpty ? String(localized: "Display \(Int(d.id) + 1)") : d.name
        return "\(name) (\(d.w)×\(d.h))"
    }

    @EnvironmentObject private var core: NexaCore
    @StateObject private var keys = KeyInputController()
    @AppStorage("pointerMode") private var mode: PointerMode = .trackpad
    /// The controls folded into one small button, leaving the whole picture free.
    @AppStorage("controlsCollapsed") private var collapsed = false
    @State private var confirmDisconnect = false
    @State private var showShortcuts = false
    /// What the gestures do now, shown for a moment after switching modes.
    @State private var hint: String?
    @State private var hintGeneration = 0

    var body: some View {
        VStack(spacing: 0) {
            ZStack {
                VideoView(core: core, mode: mode,
                          stream: CGSize(width: core.stream.w, height: core.stream.h))
                    .ignoresSafeArea(.container, edges: [.horizontal, .bottom])
                // Takes the keyboard; it has no size of its own.
                KeyInputHost(controller: keys, core: core).frame(width: 0, height: 0)
                VStack(spacing: 0) {
                    status
                    Spacer(minLength: 0)
                    controls
                }
            }
            if core.inputAllowed && keys.softKeyboard {
                KeyboardBar(keys: keys)
            }
        }
        .background(Color.black)
        .preferredColorScheme(.dark)
        .sheet(isPresented: $showShortcuts) { ShortcutsView() }
        .confirmationDialog(
            Text("Disconnect from \(core.hostName)?"),
            isPresented: $confirmDisconnect,
            titleVisibility: .visible
        ) {
            Button("Disconnect", role: .destructive) { core.disconnect() }
        }
    }

    @ViewBuilder private var status: some View {
        if case .reconnecting(let text) = core.phase {
            Text(text).font(.footnote).padding(8)
                .background(.thinMaterial, in: Capsule()).padding(.top, 8)
        } else if let hint {
            Text(hint).font(.footnote).multilineTextAlignment(.center)
                .padding(.horizontal, 12).padding(.vertical, 8)
                .background(.thinMaterial, in: RoundedRectangle(cornerRadius: 12))
                .padding(.top, 8).padding(.horizontal, 16)
                .transition(.opacity)
                .allowsHitTesting(false)
        }
    }

    /// Floating over the picture at the bottom, where a thumb reaches; every target is
    /// at least 44 points. Disconnecting is at the far end and asks first, so a stray
    /// tap does not end the session.
    @ViewBuilder private var controls: some View {
        if collapsed {
            HStack {
                Spacer()
                Button { withAnimation(.snappy) { collapsed = false } } label: {
                    Image(systemName: "ellipsis")
                        .font(.title3.weight(.semibold))
                        .frame(width: 44, height: 44)
                        .background(.ultraThinMaterial, in: Circle())
                }
                .accessibilityLabel("Show controls")
            }
            .padding(.horizontal, 16).padding(.bottom, 8)
        } else {
            HStack(spacing: 4) {
                if core.inputAllowed {
                    control(mode == .trackpad ? "rectangle.and.hand.point.up.left" : "hand.tap",
                            mode == .trackpad ? "Trackpad mode" : "Touch mode") { switchMode() }
                    control(keys.softKeyboard ? "keyboard.chevron.compact.down" : "keyboard",
                            keys.softKeyboard ? "Hide keyboard" : "Show keyboard") { keys.toggleKeyboard() }
                    control("command", "Keys") { showShortcuts = true }
                }
                if core.displays.count > 1 {
                    // One display at a time: picking another replaces the picture.
                    Menu {
                        ForEach(core.displays, id: \.id) { d in
                            Button {
                                core.selectDisplay(d.id)
                            } label: {
                                if d.id == core.display {
                                    Label(Self.title(of: d), systemImage: "checkmark")
                                } else {
                                    Text(Self.title(of: d))
                                }
                            }
                        }
                    } label: {
                        Image(systemName: "display.2").frame(width: 44, height: 44)
                    }
                    .accessibilityLabel("Display")
                }
                control("chevron.down", "Hide controls") { withAnimation(.snappy) { collapsed = true } }
                Divider().frame(height: 24).padding(.horizontal, 4)
                control("xmark", "Disconnect", tint: .red) { confirmDisconnect = true }
            }
            .font(.title3)
            .padding(.horizontal, 6)
            .background(.ultraThinMaterial, in: Capsule())
            .padding(.bottom, 8)
            .transition(.move(edge: .bottom).combined(with: .opacity))
        }
    }

    private func control(_ symbol: String, _ label: LocalizedStringKey, tint: Color? = nil,
                         action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Image(systemName: symbol).frame(width: 44, height: 44)
        }
        .tint(tint)
        .foregroundStyle(tint ?? .accentColor)
        .accessibilityLabel(label)
    }

    private func switchMode() {
        mode = mode == .trackpad ? .touch : .trackpad
        UISelectionFeedbackGenerator().selectionChanged()
        let text = mode == .trackpad
            ? String(localized: "Trackpad: drag to move the pointer, tap to click, touch and hold to drag. Two fingers: tap for a right click, drag to scroll, pinch to zoom.")
            : String(localized: "Touch: tap to click there, drag to drag, touch and hold for a right click. Two fingers: drag to scroll, pinch to zoom.")
        hintGeneration += 1
        let generation = hintGeneration
        withAnimation { hint = text }
        Task {
            try? await Task.sleep(for: .seconds(4))
            if generation == hintGeneration { withAnimation { hint = nil } }
        }
    }
}
