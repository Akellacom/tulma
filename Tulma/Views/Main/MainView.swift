import SwiftUI

struct MainView: View {
    @Environment(AppStore.self) private var store

    var body: some View {
        @Bindable var store = store
        NavigationSplitView {
            SidebarView()
                .navigationSplitViewColumnWidth(min: 190, ideal: 220, max: 300)
        } content: {
            content
                .navigationSplitViewColumnWidth(min: 320, ideal: 380)
        } detail: {
            detail
        }
        .sheet(isPresented: $store.showImport) {
            ImportSheet(seed: store.importSeed)
                .environment(store)
                .onDisappear { store.importSeed = "" }
        }
        .sheet(isPresented: $store.showExport) {
            ExportSheet(workspaceID: store.exportSeed ?? store.targetWorkspaceID ?? "")
                .environment(store)
                .onDisappear { store.exportSeed = nil }
        }
        .sheet(isPresented: $store.showAddServer) {
            AddServerSheet()
                .environment(store)
        }
        .alert(
            "Something went wrong",
            isPresented: Binding(get: { store.errorMessage != nil }, set: { if !$0 { store.errorMessage = nil } })
        ) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(store.errorMessage ?? "")
        }
        .alert(
            "Daemon stopped",
            isPresented: Binding(get: { store.daemonFailure != nil }, set: { if !$0 { store.daemonFailure = nil } })
        ) {
            Button("Restart") { Task { await store.restartDaemon() } }
            Button("Dismiss", role: .cancel) {}
        } message: {
            Text(store.daemonFailure ?? "")
        }
    }

    @ViewBuilder
    private var content: some View {
        switch store.sidebar {
        case .workspace(let id):
            if let workspace = store.workspace(id) {
                ServerListView(workspace: workspace)
            } else {
                ContentUnavailableView("Workspace not found", systemImage: "square.stack.3d.up.slash")
            }
        case .logs:
            LogTableView()
        case nil:
            ProgressView("Starting Tulma…")
        }
    }

    @ViewBuilder
    private var detail: some View {
        switch store.sidebar {
        case .logs:
            if let entry = store.selectedLog {
                LogEntryDetail(entry: entry)
            } else {
                ContentUnavailableView("Select a log entry", systemImage: "list.bullet.rectangle")
            }
        default:
            if let id = store.selectedServer, let server = store.server(id) {
                ServerDetailView(server: server)
                    .id(server.id)
            } else {
                ContentUnavailableView("Select a server", systemImage: "server.rack")
            }
        }
    }
}
