import SwiftUI

struct EndpointPopover: View {
    @Environment(AppStore.self) private var store

    var body: some View {
        let url = store.endpointURL ?? "http://127.0.0.1:\(store.state?.settings.port ?? 4747)/mcp"
        VStack(alignment: .leading, spacing: 14) {
            Text("Connect a client").font(.headline)
            Text("Every client sees the active workspace. Switching workspaces updates their tool list without reconnecting.")
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            snippet("Endpoint", url)
            snippet("Claude Code", "claude mcp add --transport http tulma \(url)")
            snippet("Claude Desktop (stdio bridge)", desktopConfig)
            snippet("Cursor", cursorConfig(url))
        }
        .padding(16)
        .frame(width: 480)
    }

    private var desktopConfig: String {
        """
        {
          "mcpServers": {
            "tulma": {
              "command": "\(AppStore.daemonExecutable.path)",
              "args": ["connect"]
            }
          }
        }
        """
    }

    private func cursorConfig(_ url: String) -> String {
        """
        {
          "mcpServers": {
            "tulma": { "url": "\(url)" }
          }
        }
        """
    }

    private func snippet(_ title: String, _ code: String) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                Text(title).font(.subheadline.weight(.medium))
                Spacer()
                CopyButton(text: code)
                    .buttonStyle(.borderless)
                    .controlSize(.small)
            }
            Text(code)
                .font(.caption.monospaced())
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(8)
                .background(.background.secondary, in: RoundedRectangle(cornerRadius: 6))
        }
    }
}
