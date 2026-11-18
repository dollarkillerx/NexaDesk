import SwiftUI

/// The app's name, version, and where the project lives: the source on GitHub, the
/// desktop downloads, issues, and the website in the person's language.
struct AboutView: View {
    static let repository = URL(string: "https://github.com/dollarkillerx/NexaDesk")!
    static let releases = URL(string: "https://github.com/dollarkillerx/NexaDesk/releases/latest")!
    static let issues = URL(string: "https://github.com/dollarkillerx/NexaDesk/issues")!

    /// The website's page in the app's language (the READMEs link the same ones).
    static var website: URL {
        let language = Bundle.main.preferredLocalizations.first ?? "en"
        let path = language.hasPrefix("zh") ? "" : language.hasPrefix("ja") ? "ja/" : "en/"
        return URL(string: "https://nexadesk-bq9.pages.dev/\(path)")!
    }

    private var version: String {
        let info = Bundle.main.infoDictionary
        let short = info?["CFBundleShortVersionString"] as? String ?? "?"
        let build = info?["CFBundleVersion"] as? String ?? "?"
        return "\(short) (\(build))"
    }

    var body: some View {
        List {
            Section {
                VStack(spacing: 10) {
                    Image("Logo")
                        .resizable()
                        .frame(width: 88, height: 88)
                        .clipShape(RoundedRectangle(cornerRadius: 20, style: .continuous))
                        .accessibilityHidden(true)
                    Text(verbatim: "NexaDesk").font(.title2.weight(.semibold))
                    Text("Version \(version)").font(.subheadline).foregroundStyle(.secondary)
                    Text("Controls a Mac or Windows PC running NexaDesk. Open source; no account needed.")
                        .font(.footnote).foregroundStyle(.secondary).multilineTextAlignment(.center)
                }
                .frame(maxWidth: .infinity)
                .padding(.vertical, 8)
                .listRowBackground(Color.clear)
            }

            Section {
                link("Source code on GitHub", detail: "github.com/dollarkillerx/NexaDesk", symbol: "chevron.left.forwardslash.chevron.right", url: Self.repository)
                link("Download for Mac and Windows", detail: "GitHub Releases", symbol: "arrow.down.circle", url: Self.releases)
                link("Report a problem", detail: "GitHub Issues", symbol: "exclamationmark.bubble", url: Self.issues)
                link("Website", detail: Self.website.host() ?? "", symbol: "globe", url: Self.website)
            }
        }
        .navigationTitle("About")
        .navigationBarTitleDisplayMode(.inline)
    }

    private func link(_ title: LocalizedStringKey, detail: String, symbol: String, url: URL) -> some View {
        Link(destination: url) {
            HStack(spacing: 12) {
                Image(systemName: symbol).frame(width: 24).foregroundStyle(Color.accentColor)
                VStack(alignment: .leading, spacing: 2) {
                    Text(title).foregroundStyle(.primary)
                    Text(verbatim: detail).font(.footnote).foregroundStyle(.secondary)
                }
                Spacer()
                Image(systemName: "arrow.up.right").font(.footnote).foregroundStyle(.tertiary)
            }
            .frame(minHeight: 44)
        }
    }
}
