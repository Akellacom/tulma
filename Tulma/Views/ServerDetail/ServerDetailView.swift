import SwiftUI

struct ServerDetailView: View {
    let server: ServerView
    @State private var tab: Tab = .overview

    enum Tab: String, CaseIterable, Identifiable {
        case overview = "Overview", tools = "Tools", log = "Log"
        var id: String { rawValue }
    }

    var body: some View {
        VStack(spacing: 0) {
            Picker("Section", selection: $tab) {
                ForEach(Tab.allCases) { Text($0.rawValue).tag($0) }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .frame(maxWidth: 320)
            .padding(.vertical, 10)
            Divider()
            switch tab {
            case .overview: ServerOverview(server: server)
            case .tools: ServerToolsView(server: server)
            case .log: ServerLogView(server: server)
            }
        }
        .navigationTitle(server.name)
    }
}

struct ServerOverview: View {
    @Environment(AppStore.self) private var store
    let server: ServerView
    @State private var draft: ServerDraft
    @State private var confirmDelete = false
    @State private var saving = false

    init(server: ServerView) {
        self.server = server
        _draft = State(initialValue: ServerDraft(server))
    }

    private var status: ServerStatus { store.status(server.id) }
    private var inActiveWorkspace: Bool {
        store.activeWorkspace?.servers.contains { $0.id == server.id } ?? false
    }
    private var dirty: Bool { draft != ServerDraft(server) }

    var body: some View {
        Form {
            Section("Status") {
                LabeledContent("State") {
                    HStack(spacing: 6) {
                        StatusDot(state: status.state, enabled: server.enabled)
                        Text(inActiveWorkspace ? status.state.label : "Inactive workspace")
                        if let pid = status.pid, status.state == .running {
                            Text("pid \(String(pid))").foregroundStyle(.secondary).monospacedDigit()
                        }
                    }
                }
                if let name = status.serverName, status.state == .running {
                    LabeledContent("Server", value: [name, status.serverVersion].compactMap { $0 }.joined(separator: " "))
                    LabeledContent("Exposes") {
                        Text("\(status.toolCount) tools · \(status.promptCount) prompts · \(status.resourceCount) resources")
                    }
                }
                if let error = status.error, status.state == .error {
                    Text(error)
                        .font(.callout.monospaced())
                        .foregroundStyle(.red)
                        .textSelection(.enabled)
                }
                HStack {
                    Button("Start") { Task { await store.start(server.id) } }
                        .disabled(!inActiveWorkspace || status.state == .running || status.state == .starting)
                    Button("Stop") { Task { await store.stop(server.id) } }
                        .disabled(status.state == .stopped)
                    Button("Restart") { Task { await store.restart(server.id) } }
                        .disabled(!inActiveWorkspace)
                    Spacer()
                    Text("Tools are exposed as \(server.slug)__<tool>")
                        .font(.caption.monospaced())
                        .foregroundStyle(.secondary)
                }
            }
            ServerForm(draft: $draft)
            Section {
                HStack {
                    Button("Delete Server…", role: .destructive) { confirmDelete = true }
                    Spacer()
                    if let message = draft.validationMessage {
                        Text(message).font(.caption).foregroundStyle(.secondary)
                    }
                    Button("Revert") { draft = ServerDraft(server) }
                        .disabled(!dirty)
                    Button("Save") {
                        saving = true
                        Task {
                            await store.updateServer(server.id, draft.payload)
                            saving = false
                        }
                    }
                    .keyboardShortcut("s")
                    .buttonStyle(.borderedProminent)
                    .disabled(!dirty || draft.validationMessage != nil || saving)
                }
            }
        }
        .formStyle(.grouped)
        .onChange(of: server) { _, updated in
            if !dirty || saving { draft = ServerDraft(updated) }
        }
        .confirmationDialog("Delete “\(server.name)”?", isPresented: $confirmDelete) {
            Button("Delete Server", role: .destructive) { Task { await store.deleteServer(server.id) } }
        } message: {
            Text("The server stops and its Keychain secrets are removed.")
        }
    }
}
