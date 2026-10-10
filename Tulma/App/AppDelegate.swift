import AppKit

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    static let hideDockKey = "hideDockIcon"

    let store = AppStore()

    func applicationDidFinishLaunching(_ notification: Notification) {
        Self.applyDockPreference()
        Task { await store.launch() }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        false
    }

    /// The endpoint lives as long as the app; quitting must stop the daemon and its children first.
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        Task {
            await store.shutdown()
            sender.reply(toApplicationShouldTerminate: true)
        }
        return .terminateLater
    }

    static func applyDockPreference() {
        let hidden = UserDefaults.standard.bool(forKey: hideDockKey)
        NSApp.setActivationPolicy(hidden ? .accessory : .regular)
    }
}
