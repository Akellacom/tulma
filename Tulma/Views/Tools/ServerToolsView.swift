import SwiftUI

struct ServerToolsView: View {
    @Environment(AppStore.self) private var store
    let server: ServerView
    @State private var query = ""

    var body: some View {
        let tools = store.tools[server.id]
        let all = tools?.tools ?? []
        let filtered = query.isEmpty ? all : all.filter {
            $0.name.localizedCaseInsensitiveContains(query) || ($0.description ?? "").localizedCaseInsensitiveContains(query)
        }
        VStack(spacing: 0) {
            HStack {
                Text("\(all.filter(\.enabled).count) of \(all.count) enabled")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                if tools?.live == false, !all.isEmpty {
                    Label("Last known list — server is not running", systemImage: "clock.arrow.circlepath")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                Button("Enable All") { setAll(all, true) }
                    .disabled(all.allSatisfy(\.enabled))
                Button("Disable All") { setAll(all, false) }
                    .disabled(!all.contains(where: \.enabled))
            }
            .controlSize(.small)
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
            List(filtered) { tool in
                ToolRow(serverID: server.id, tool: tool)
            }
            .searchable(text: $query, placement: .toolbar, prompt: "Filter tools")
            .overlay {
                if all.isEmpty {
                    ContentUnavailableView(
                        "No tools yet",
                        systemImage: "wrench.and.screwdriver",
                        description: Text("Start the server to discover its tools.")
                    )
                }
            }
            .opacity(tools?.live == false ? 0.65 : 1)
        }
        .task(id: store.status(server.id).state) { await store.loadTools(server.id) }
    }

    private func setAll(_ tools: [ToolView], _ enabled: Bool) {
        Task {
            for tool in tools where tool.enabled != enabled {
                await store.setTool(server.id, tool.name, enabled: enabled)
            }
        }
    }
}

struct ToolRow: View {
    @Environment(AppStore.self) private var store
    let serverID: String
    let tool: ToolView

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Toggle("Enabled", isOn: Binding(
                get: { tool.enabled },
                set: { value in Task { await store.setTool(serverID, tool.name, enabled: value) } }
            ))
            .labelsHidden()
            .toggleStyle(.switch)
            .controlSize(.mini)
            VStack(alignment: .leading, spacing: 3) {
                Text(tool.title ?? tool.name).fontWeight(.medium)
                Text(tool.exposedName)
                    .font(.caption.monospaced())
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
                if let description = tool.description, !description.isEmpty {
                    Text(description)
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .lineLimit(4)
                }
                DisclosureGroup("Input schema") {
                    Text(tool.inputSchema.pretty)
                        .font(.caption.monospaced())
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(6)
                        .background(.background.secondary, in: RoundedRectangle(cornerRadius: 6))
                }
                .font(.caption)
            }
        }
        .padding(.vertical, 3)
        .opacity(tool.enabled ? 1 : 0.55)
    }
}
