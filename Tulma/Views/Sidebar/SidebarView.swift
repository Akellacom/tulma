import SwiftUI

struct SidebarView: View {
    @Environment(AppStore.self) private var store
    @State private var renaming: WorkspaceView?
    @State private var newName = ""
    @State private var deleting: WorkspaceView?

    var body: some View {
        @Bindable var store = store
        List(selection: $store.sidebar) {
            Section("Workspaces") {
                ForEach(store.workspaces) { workspace in
                    row(workspace)
                        .tag(SidebarItem.workspace(workspace.id))
                        .contextMenu { menu(workspace) }
                }
            }
            Section("Activity") {
                Label("Logs", systemImage: "list.bullet.rectangle")
                    .tag(SidebarItem.logs)
            }
        }
        .toolbar {
            ToolbarItem {
                Button {
                    newName = ""
                    store.promptNewWorkspace = true
                } label: {
                    Label("New Workspace", systemImage: "plus")
                }
                .help("New workspace (⌘N)")
            }
        }
        .alert("New Workspace", isPresented: $store.promptNewWorkspace) {
            TextField("Name", text: $newName)
            Button("Create") {
                let name = newName
                Task { await store.createWorkspace(named: name) }
            }
            Button("Cancel", role: .cancel) {}
        }
        .alert("Rename Workspace", isPresented: Binding(get: { renaming != nil }, set: { if !$0 { renaming = nil } })) {
            TextField("Name", text: $newName)
            Button("Rename") {
                if let id = renaming?.id {
                    let name = newName
                    Task { await store.renameWorkspace(id, to: name) }
                }
            }
            Button("Cancel", role: .cancel) {}
        }
        .confirmationDialog(
            "Delete “\(deleting?.name ?? "")”?",
            isPresented: Binding(get: { deleting != nil }, set: { if !$0 { deleting = nil } })
        ) {
            Button("Delete Workspace", role: .destructive) {
                if let id = deleting?.id { Task { await store.deleteWorkspace(id) } }
            }
        } message: {
            Text("Its servers and their Keychain secrets are removed. This cannot be undone.")
        }
    }

    private func row(_ workspace: WorkspaceView) -> some View {
        let active = workspace.id == store.activeWorkspaceID
        return HStack {
            Label(workspace.name, systemImage: active ? "square.stack.3d.up.fill" : "square.stack.3d.up")
            Spacer()
            if active {
                Image(systemName: "dot.radiowaves.left.and.right")
                    .foregroundStyle(.green)
                    .help("Active: served on the endpoint")
            }
            Text("\(workspace.servers.count)")
                .monospacedDigit()
                .foregroundStyle(.secondary)
        }
    }

    @ViewBuilder
    private func menu(_ workspace: WorkspaceView) -> some View {
        Button("Activate") { Task { await store.activateWorkspace(workspace.id) } }
            .disabled(workspace.id == store.activeWorkspaceID)
        Button("Rename…") {
            newName = workspace.name
            renaming = workspace
        }
        Button("Duplicate") { Task { await store.duplicateWorkspace(workspace.id) } }
        Button("Export…") {
            store.exportSeed = workspace.id
            store.showExport = true
        }
        Divider()
        Button("Delete…", role: .destructive) { deleting = workspace }
            .disabled(store.workspaces.count <= 1)
    }
}
