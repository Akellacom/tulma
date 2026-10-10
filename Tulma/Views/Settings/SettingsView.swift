import ServiceManagement
import SwiftUI

struct SettingsView: View {
    @Environment(AppStore.self) private var store
    @AppStorage(AppDelegate.hideDockKey) private var hideDock = false
    @State private var draft = DaemonSettings(port: 4747, callTimeoutSecs: 120, logRetentionDays: 14)
    @State private var launchAtLogin = SMAppService.mainApp.status == .enabled

    var body: some View {
        Form {
            Section("Endpoint") {
                TextField("Port", value: $draft.port, format: .number.grouping(.never))
                    .frame(maxWidth: 220)
                Text("Clients connect to http://127.0.0.1:\(String(draft.port))/mcp. Changing the port restarts the endpoint, and clients must be updated.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Section("Tool calls") {
                Stepper("Timeout: \(draft.callTimeoutSecs) s", value: $draft.callTimeoutSecs, in: 5...3600, step: 5)
            }
            Section("Logs") {
                Stepper("Keep logs for \(draft.logRetentionDays) days", value: $draft.logRetentionDays, in: 1...365)
            }
            Section("App") {
                Toggle("Launch at login", isOn: $launchAtLogin)
                    .onChange(of: launchAtLogin) { _, enabled in setLoginItem(enabled) }
                Toggle("Hide Dock icon", isOn: $hideDock)
                    .onChange(of: hideDock) { _, _ in
                        AppDelegate.applyDockPreference()
                        NSApp.activate()
                    }
                Text("Tulma stays in the menu bar. Closing the window keeps the endpoint running; Quit stops it.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            HStack {
                Spacer()
                Button("Revert") { if let s = store.state?.settings { draft = s } }
                    .disabled(draft == store.state?.settings)
                Button("Apply") { Task { await store.updateSettings(draft) } }
                    .buttonStyle(.borderedProminent)
                    .disabled(draft == store.state?.settings || !(1...65535).contains(draft.port))
            }
        }
        .formStyle(.grouped)
        .frame(width: 480)
        .fixedSize(horizontal: false, vertical: true)
        .onAppear { if let s = store.state?.settings { draft = s } }
        .onChange(of: store.state?.settings) { _, s in if let s { draft = s } }
    }

    private func setLoginItem(_ enabled: Bool) {
        do {
            if enabled { try SMAppService.mainApp.register() } else { try SMAppService.mainApp.unregister() }
        } catch {
            store.errorMessage = "Launch at login: \(error.localizedDescription)"
            launchAtLogin = SMAppService.mainApp.status == .enabled
        }
    }
}
