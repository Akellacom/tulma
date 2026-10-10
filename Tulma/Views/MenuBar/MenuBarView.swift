import SwiftUI

struct MenuBarIcon: View {
    let store: AppStore

    var body: some View {
        if store.daemonFailure != nil || store.endpoint?.running == false {
            Image(systemName: "exclamationmark.triangle")
        } else {
            Image(store.hasErrors ? "MenuBarIconError" : "MenuBarIcon")
                .renderingMode(.template)
        }
    }
}

struct MenuBarView: View {
    @Environment(AppStore.self) private var store
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        Text("\(store.runningCount)/\(store.enabledCount) servers running")
        if let error = store.endpoint?.error {
            Text("Endpoint: \(error)")
        }
        Divider()
        Picker("Workspace", selection: Binding(
            get: { store.activeWorkspaceID ?? "" },
            set: { id in Task { await store.activateWorkspace(id) } }
        )) {
            ForEach(store.workspaces) { Text($0.name).tag($0.id) }
        }
        .pickerStyle(.inline)
        Divider()
        Button("Copy Endpoint URL") {
            if let url = store.endpointURL { copyToPasteboard(url) }
        }
        .disabled(store.endpointURL == nil)
        Button("Open Tulma") {
            openWindow(id: WindowID.main)
            NSApp.activate()
        }
        SettingsLink { Text("Settings…") }
            .keyboardShortcut(",")
        Divider()
        Button("Quit Tulma") { NSApp.terminate(nil) }
            .keyboardShortcut("q")
    }
}
