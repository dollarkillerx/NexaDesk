import SwiftUI

/// Which signaling server device IDs go through: the desktop app's three choices
/// (none, a published one, one's own), on a page of their own so the connect screen
/// stays short.
struct ServerSettingsView: View {
    @EnvironmentObject private var settings: ServerSettings
    @State private var revealKey = false

    var body: some View {
        Form {
            Section {
                Picker(selection: $settings.mode) {
                    choice("No server", detail: "Addresses, the LAN and Tailscale only").tag(ServerMode.none)
                    choice("A published server", detail: "Pick one from the project's list on GitHub").tag(ServerMode.published)
                    choice("My own server", detail: "Address and key").tag(ServerMode.own)
                } label: {
                    EmptyView()
                }
                .pickerStyle(.inline)
                .labelsHidden()
            } footer: {
                Text("You can connect without a server: directly on the same local network, and across networks with Tailscale on both devices. A signaling server is needed only to connect by 9-digit device ID.")
            }

            switch settings.mode {
            case .none:
                EmptyView()
            case .published:
                publishedSection
            case .own:
                ownSection
            }
        }
        .navigationTitle("Signaling server")
        .navigationBarTitleDisplayMode(.inline)
        .animation(.default, value: settings.mode)
        .task(id: settings.mode) {
            if settings.mode == .published, settings.list.isEmpty { await settings.loadList() }
        }
        .onDisappear { settings.saveOwnKey() }
    }

    private func choice(_ title: LocalizedStringKey, detail: LocalizedStringKey) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(title)
            Text(detail).font(.footnote).foregroundStyle(.secondary)
        }
        .padding(.vertical, 2)
    }

    // MARK: Published

    private var publishedSection: some View {
        Section {
            if settings.list.isEmpty {
                if settings.loadingList {
                    HStack(spacing: 10) {
                        ProgressView()
                        Text("Loading the list…").foregroundStyle(.secondary)
                    }
                } else {
                    Text("No servers are published right now, or the list could not be reached.")
                        .foregroundStyle(.secondary)
                }
            }
            ForEach(settings.list) { entry in
                Button {
                    settings.pick(entry)
                    UISelectionFeedbackGenerator().selectionChanged()
                } label: {
                    HStack {
                        VStack(alignment: .leading, spacing: 2) {
                            Text(entry.endpoint).foregroundStyle(.primary)
                            if let health = settings.health[entry.endpoint] {
                                Text(health.text).font(.footnote).foregroundStyle(color(of: health))
                            }
                        }
                        Spacer()
                        if settings.published?.endpoint == entry.endpoint {
                            Image(systemName: "checkmark").foregroundStyle(Color.accentColor)
                        }
                    }
                    .contentShape(Rectangle())
                }
                .frame(minHeight: 44)
            }
            if !settings.list.isEmpty {
                Button {
                    Task { await settings.loadList() }
                } label: {
                    Label("Refresh", systemImage: "arrow.clockwise")
                }
                .disabled(settings.loadingList)
            }
        } header: {
            Text("Choose a published server")
        } footer: {
            if !settings.list.isEmpty {
                Text("\(settings.list.count) published. The one you pick is used at once. The keys in this list are public.")
            }
        }
    }

    // MARK: Own

    private var ownSection: some View {
        Section {
            TextField(String("signal.example.com"), text: $settings.ownServer)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .keyboardType(.URL)
                .textContentType(.URL)
            HStack {
                Group {
                    if revealKey {
                        TextField("The server's key (required)", text: $settings.ownKey)
                    } else {
                        SecureField("The server's key (required)", text: $settings.ownKey)
                    }
                }
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .keyboardType(.asciiCapable)
                Button {
                    revealKey.toggle()
                } label: {
                    Image(systemName: revealKey ? "eye.slash" : "eye")
                }
                .buttonStyle(.borderless)
                .accessibilityLabel(revealKey ? "Hide" : "Show")
            }
            HStack {
                Button("Test") { Task { await settings.testOwn() } }
                    .disabled(settings.ownServer.trimmingCharacters(in: .whitespaces).isEmpty || settings.ownHealth == .testing)
                Spacer()
                if let health = settings.ownHealth {
                    Text(health.text).font(.footnote).foregroundStyle(color(of: health))
                }
            }
        } header: {
            Text("My own server")
        } footer: {
            Text("Use the form wss://server[:port], for example wss://signal.example.com. A signaling server always needs its key: enter the one its owner gave you. A server with its own certificate is trusted on first use, after you compare its fingerprint.")
        }
    }

    private func color(of health: ProbeResult) -> Color {
        switch health {
        case .available: .green
        case .testing: .secondary
        case .unreachable, .notSignal: .orange
        }
    }
}
