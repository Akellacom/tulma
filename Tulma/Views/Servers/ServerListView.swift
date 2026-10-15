import SwiftUI
import UniformTypeIdentifiers

struct ServerListView: View {
    @Environment(AppStore.self) private var store
    let workspace: WorkspaceView
    @State private var dropTargeted = false

    private var isActive: Bool { workspace.id == store.activeWorkspaceID }

    var body: some View {
        @Bindable var store = store
        List(selection: $store.selectedServer) {
            ForEach(workspace.servers) { server in
                ServerRow(server: server, status: store.status(server.id), workspaceActive: isActive)
                    .tag(server.id)
            }
        }
        .overlay {
            if workspace.servers.isEmpty {
                ContentUnavailableView {
                    Label("No servers", systemImage: "server.rack")
                } description: {
                    Text("Pick a server from a catalog, add one by hand, or import an mcpServers config from Claude Desktop or Cursor. You can also drop a .json file here.")
                } actions: {
                    Button("Browse Catalog…") { store.showCatalog = true }
                        .buttonStyle(.borderedProminent)
                    Button("Import…") { store.showImport = true }
                    Button("Add Server…") { store.showAddServer = true }
                }
            }
        }
        .safeAreaInset(edge: .top, spacing: 0) { header }
        .safeAreaInset(edge: .bottom, spacing: 0) { EndpointFooter() }
        .navigationTitle(workspace.name)
        .toolbar {
            ToolbarItemGroup {
                Button { store.showCatalog = true } label: { Label("Browse Catalog", systemImage: "square.grid.2x2") }
                    .help("Browse the MCP Registry and Archestra catalogs (⇧⌘B)")
                Button { store.showAddServer = true } label: { Label("Add Server", systemImage: "plus.rectangle") }
                    .help("Add server (⇧⌘N)")
                Button { store.showImport = true } label: { Label("Import", systemImage: "square.and.arrow.down") }
                    .help("Import mcpServers JSON (⌘I)")
                Button {
                    store.exportSeed = workspace.id
                    store.showExport = true
                } label: {
                    Label("Export", systemImage: "square.and.arrow.up")
                }
                .help("Export mcpServers JSON (⇧⌘E)")
                .disabled(workspace.servers.isEmpty)
            }
        }
        .dropDestination(for: URL.self) { urls, _ in
            guard let url = urls.first(where: { $0.pathExtension.lowercased() == "json" }),
                  let text = try? String(contentsOf: url, encoding: .utf8)
            else { return false }
            store.importSeed = text
            store.showImport = true
            return true
        } isTargeted: { dropTargeted = $0 }
        .overlay {
            if dropTargeted {
                RoundedRectangle(cornerRadius: 10)
                    .strokeBorder(.tint, style: StrokeStyle(lineWidth: 2, dash: [6]))
                    .padding(6)
            }
        }
    }

    private var header: some View {
        HStack(spacing: 8) {
            if isActive {
                Label("Active", systemImage: "dot.radiowaves.left.and.right")
                    .font(.callout.weight(.medium))
                    .foregroundStyle(.green)
                Text("\(store.runningCount) of \(store.enabledCount) running")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            } else {
                Text("Not served on the endpoint")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                Spacer()
                Button("Activate") { Task { await store.activateWorkspace(workspace.id) } }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.small)
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .background(.bar)
    }
}

struct ServerRow: View {
    @Environment(AppStore.self) private var store
    let server: ServerView
    let status: ServerStatus
    let workspaceActive: Bool

    var body: some View {
        HStack(spacing: 8) {
            StatusDot(state: workspaceActive ? status.state : .stopped, enabled: server.enabled)
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text(server.name).fontWeight(.medium)
                    Badge(text: server.transport.kind)
                }
                Text(subtitle)
                    .font(.caption)
                    .foregroundStyle(status.state == .error && workspaceActive ? AnyShapeStyle(.red) : AnyShapeStyle(.secondary))
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            Spacer()
            Toggle("Enabled", isOn: Binding(
                get: { server.enabled },
                set: { value in Task { await store.setEnabled(server.id, value) } }
            ))
            .labelsHidden()
            .toggleStyle(.switch)
            .controlSize(.mini)
        }
        .padding(.vertical, 2)
        .opacity(server.enabled ? 1 : 0.6)
    }

    private var subtitle: String {
        guard workspaceActive, server.enabled else { return server.transport.summary }
        switch status.state {
        case .running:
            let enabledTools = max(status.toolCount - server.disabledTools.count, 0)
            return "\(enabledTools) tools" + (status.serverVersion.map { " · v\($0)" } ?? "")
        case .error where status.needsAuth: return "Sign-in required"
        case .error: return status.error ?? "Error"
        case .starting: return status.restartAttempt > 0 ? "Restarting (attempt \(status.restartAttempt))…" : "Starting…"
        case .stopped: return server.transport.summary
        }
    }
}

struct EndpointFooter: View {
    @Environment(AppStore.self) private var store
    @State private var showConnect = false

    var body: some View {
        HStack(spacing: 8) {
            Circle()
                .fill(store.endpoint?.running == true ? Color.green : Color.red)
                .frame(width: 7, height: 7)
            if let url = store.endpointURL {
                Text(url)
                    .font(.callout.monospaced())
                    .textSelection(.enabled)
                    .lineLimit(1)
                if let clients = store.endpoint?.clients, clients > 0 {
                    Text("· \(clients) session\(clients == 1 ? "" : "s")")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                CopyButton(text: url)
                    .labelStyle(.iconOnly)
                    .buttonStyle(.borderless)
                    .help("Copy endpoint URL")
            } else {
                Text(store.endpoint?.error ?? "Endpoint is not running")
                    .font(.callout)
                    .foregroundStyle(.red)
                    .lineLimit(2)
                Spacer()
                Button("Retry") { Task { await store.restartEndpoint() } }
                    .controlSize(.small)
                    .help("Try to bind the configured port again")
            }
            Button("Connect a client…") { showConnect = true }
                .controlSize(.small)
                .popover(isPresented: $showConnect, arrowEdge: .top) {
                    EndpointPopover()
                        .environment(store)
                }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .background(.bar)
    }
}
