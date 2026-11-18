import SwiftUI

@main
struct NexaDeskApp: App {
    @StateObject private var core = NexaCore()

    var body: some Scene {
        WindowGroup {
            RootView().environmentObject(core)
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
    @StateObject private var lan = LanBrowser()
    @AppStorage("address") private var address = ""
    @AppStorage("signalServer") private var signalServer = ""
    @State private var secret = ""
    @State private var revealSecret = false
    @State private var resolving: String?
    @FocusState private var secretFocused: Bool

    private var isDeviceId: Bool { NexaCore.isDeviceId(address) }
    /// An example address, the same in every language.
    private static let serverPlaceholder: String = "signal.example.com"

    var body: some View {
        NavigationStack {
            Form {
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
                    Text("On the same network (or Tailscale), an IP address is enough; port \(String(NexaCore.defaultPort)) is assumed. From anywhere else, use the 9-digit device ID with your signaling server.")
                }

                Section {
                    TextField(Self.serverPlaceholder, text: $signalServer)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .keyboardType(.URL)
                } header: {
                    Text("Signaling server")
                } footer: {
                    Text("Your own nexa-signal server, as set on the computer (wss://host[:port]). Needed only for device IDs. A server with its own certificate is trusted on first use, after you compare its fingerprint.")
                }

                Section {
                    Button("Connect") {
                        core.connect(address: address, signalServer: signalServer, secret: secret)
                    }
                    .disabled(address.trimmingCharacters(in: .whitespaces).isEmpty)
                }
            }
            .navigationTitle(Text(verbatim: "NexaDesk"))
            .onAppear {
                lan.start()
                #if DEBUG
                // Simulator tests: `SIMCTL_CHILD_NEXA_TEST_ADDRESS=… SIMCTL_CHILD_NEXA_TEST_SECRET=…
                // xcrun simctl launch …` fills the form and connects (the simulator's typing
                // tool mangles secure fields). NEXA_TEST_SERVER sets the signaling server.
                // Once per launch: this screen reappears after every failed attempt, and
                // connecting again each time hammered the host until it rate-limited.
                let env = ProcessInfo.processInfo.environment
                if let server = env["NEXA_TEST_SERVER"] { signalServer = server }
                if !ConnectView.autoConnected, let a = env["NEXA_TEST_ADDRESS"] {
                    ConnectView.autoConnected = true
                    let s = env["NEXA_TEST_SECRET"] ?? ""
                    address = a
                    secret = s
                    core.connect(address: a, signalServer: signalServer, secret: s)
                }
                #endif
            }
            .onDisappear { lan.stop() }
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

    /// A host heard on the LAN: its address goes in the form, then the code or password.
    private func pick(_ host: LanHost) {
        resolving = host.id
        Task {
            let resolved = await LanBrowser.resolve(host)
            resolving = nil
            if let resolved {
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

    var body: some View {
        VStack(spacing: 0) {
            ZStack(alignment: .top) {
                VideoView(core: core)
                    .ignoresSafeArea(edges: [.horizontal, .bottom])
                // Takes the keyboard; it has no size of its own.
                KeyInputHost(controller: keys, core: core).frame(width: 0, height: 0)
                if case .reconnecting(let text) = core.phase {
                    Text(text).font(.footnote).padding(8)
                        .background(.thinMaterial, in: Capsule()).padding(.top, 8)
                }
            }
            HStack {
                Button { core.disconnect() } label: { Image(systemName: "xmark.circle.fill") }
                    .font(.title2).padding(.leading, 12)
                Text(core.hostName).font(.footnote).foregroundStyle(.secondary).lineLimit(1)
                Spacer(minLength: 0)
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
                        Label("Display", systemImage: "display.2")
                            .labelStyle(.iconOnly)
                            .font(.title3)
                    }
                    .padding(.trailing, 12)
                }
            }
            .padding(.top, 4)
            if core.inputAllowed {
                KeyboardBar(keys: keys)
            }
        }
        .background(Color.black)
        .preferredColorScheme(.dark)
    }
}
