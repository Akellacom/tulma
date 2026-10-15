import SwiftUI

@main
struct TulmaApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate

    var body: some Scene {
        Window("Tulma", id: WindowID.main) {
            MainView()
                .environment(delegate.store)
        }
        .defaultSize(width: 1120, height: 700)
        .commands { AppCommands(store: delegate.store) }

        MenuBarExtra {
            MenuBarView()
                .environment(delegate.store)
        } label: {
            MenuBarIcon(store: delegate.store)
        }

        Settings {
            SettingsView()
                .environment(delegate.store)
        }
    }
}

enum WindowID {
    static let main = "main"
}

struct AppCommands: Commands {
    let store: AppStore

    var body: some Commands {
        CommandGroup(replacing: .newItem) {
            Button("New Workspace…") { store.promptNewWorkspace = true }
                .keyboardShortcut("n")
            Button("Add Server…") { store.showAddServer = true }
                .keyboardShortcut("n", modifiers: [.command, .shift])
            Button("Browse Catalog…") { store.showCatalog = true }
                .keyboardShortcut("b", modifiers: [.command, .shift])
            Button("Import…") { store.showImport = true }
                .keyboardShortcut("i")
            Button("Export…") { store.showExport = true }
                .keyboardShortcut("e", modifiers: [.command, .shift])
        }
    }
}
