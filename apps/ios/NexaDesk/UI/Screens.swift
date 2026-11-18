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
    @EnvironmentObject private var core: NexaCore
    @AppStorage("address") private var address = ""
    @AppStorage("signalServer") private var signalServer = ""
    @State private var secret = ""
    @State private var revealSecret = false

    private var isDeviceId: Bool {
        let a = address.trimmingCharacters(in: .whitespaces)
        return a.count == 9 && a.allSatisfy(\.isNumber)
    }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("Device ID or host:port", text: $address)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .keyboardType(.asciiCapable)
                    HStack {
                        Group {
                            if revealSecret {
                                TextField("One-time code or password", text: $secret)
                            } else {
                                SecureField("One-time code or password", text: $secret)
                            }
                        }
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .textContentType(.oneTimeCode)
                        // Codes and passwords are ASCII; a kana or pinyin keyboard would
                        // leave them as unconfirmed input.
                        .keyboardType(.asciiCapable)
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
                    Text("A 9-digit device ID needs a signaling server; an address such as 100.64.0.2:47800 (Tailscale, LAN) does not.")
                }
                if isDeviceId {
                    Section("Signaling server") {
                        TextField("wss://server", text: $signalServer)
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                            .keyboardType(.URL)
                    }
                }
                Section {
                    Button("Connect") {
                        core.connect(address: address, signalServer: signalServer, secret: secret)
                    }
                    .disabled(address.trimmingCharacters(in: .whitespaces).isEmpty)
                }
            }
            .navigationTitle("NexaDesk")
            .onAppear {
                #if DEBUG
                // Simulator tests: `SIMCTL_CHILD_NEXA_TEST_ADDRESS=… SIMCTL_CHILD_NEXA_TEST_SECRET=…
                // xcrun simctl launch …` fills the form and connects (the simulator's typing
                // tool mangles secure fields).
                let env = ProcessInfo.processInfo.environment
                if let a = env["NEXA_TEST_ADDRESS"], let s = env["NEXA_TEST_SECRET"] {
                    address = a
                    secret = s
                    core.connect(address: a, signalServer: signalServer, secret: s)
                }
                #endif
            }
            .alert("NexaDesk", isPresented: Binding(get: { core.notice != nil }, set: { if !$0 { core.notice = nil } })) {
                Button("OK", role: .cancel) {}
            } message: {
                Text(core.notice ?? "")
            }
        }
    }
}

struct SessionView: View {
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
