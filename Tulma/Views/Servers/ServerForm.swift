import SwiftUI

/// Editable copy of a server; arguments are edited one per line.
struct ServerDraft: Equatable {
    var name = ""
    var enabled = true
    var autoRestart = true
    var kind = "stdio"
    var command = ""
    var argsText = ""
    var env: [KeyValue] = []
    var cwd = ""
    var url = ""
    var headers: [KeyValue] = []

    init() {}

    init(_ server: ServerView) {
        name = server.name
        enabled = server.enabled
        autoRestart = server.autoRestart
        switch server.transport {
        case .stdio(let t):
            kind = "stdio"
            command = t.command
            argsText = t.args.joined(separator: "\n")
            env = t.env
            cwd = t.cwd ?? ""
        case .http(let t):
            kind = "http"
            url = t.url
            headers = t.headers
        }
    }

    var payload: ServerPayload {
        let transport: TransportView = kind == "http"
            ? .http(HttpTransport(url: url.trimmingCharacters(in: .whitespaces), headers: clean(headers)))
            : .stdio(StdioTransport(
                command: command.trimmingCharacters(in: .whitespaces),
                args: argsText.split(separator: "\n", omittingEmptySubsequences: true).map(String.init),
                env: clean(env),
                cwd: cwd.isEmpty ? nil : cwd
            ))
        return ServerPayload(name: name.trimmingCharacters(in: .whitespaces), enabled: enabled, autoRestart: autoRestart, transport: transport)
    }

    var validationMessage: String? {
        if name.trimmingCharacters(in: .whitespaces).isEmpty { return "Name is required." }
        if kind == "stdio", command.trimmingCharacters(in: .whitespaces).isEmpty { return "Command is required." }
        if kind == "http", !(url.hasPrefix("http://") || url.hasPrefix("https://")) { return "URL must start with http:// or https://." }
        return nil
    }

    private func clean(_ pairs: [KeyValue]) -> [KeyValue] {
        pairs.filter { !$0.key.trimmingCharacters(in: .whitespaces).isEmpty }
    }
}

struct ServerForm: View {
    @Binding var draft: ServerDraft

    var body: some View {
        Section("Server") {
            TextField("Name", text: $draft.name)
            Toggle("Enabled", isOn: $draft.enabled)
            Toggle("Restart automatically if it exits", isOn: $draft.autoRestart)
            Picker("Transport", selection: $draft.kind) {
                Text("Local process (stdio)").tag("stdio")
                Text("Remote (Streamable HTTP)").tag("http")
            }
        }
        if draft.kind == "stdio" {
            Section("Command") {
                TextField("Command", text: $draft.command, prompt: Text("npx"))
                    .font(.body.monospaced())
                LabeledContent("Arguments") {
                    TextEditor(text: $draft.argsText)
                        .font(.body.monospaced())
                        .multilineTextAlignment(.leading)
                        .frame(minHeight: 60, maxHeight: 140)
                        .scrollContentBackground(.hidden)
                        .padding(4)
                        .background(.background.secondary, in: RoundedRectangle(cornerRadius: 6))
                }
                Text("One argument per line. ${VAR} is expanded from the variables below.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                TextField("Working directory", text: $draft.cwd, prompt: Text("optional"))
                    .font(.body.monospaced())
            }
            Section("Environment") {
                KeyValueEditor(rows: $draft.env, keyPrompt: "NAME")
            }
        } else {
            Section("Endpoint") {
                TextField("URL", text: $draft.url, prompt: Text("https://example.com/mcp"))
                    .font(.body.monospaced())
            }
            Section("Headers") {
                KeyValueEditor(rows: $draft.headers, keyPrompt: "Header")
            }
        }
    }
}

struct KeyValueEditor: View {
    @Binding var rows: [KeyValue]
    let keyPrompt: String
    @State private var revealed: Set<UUID> = []

    var body: some View {
        ForEach($rows) { $row in
            HStack(spacing: 6) {
                TextField("Key", text: $row.key, prompt: Text(keyPrompt))
                    .font(.body.monospaced())
                    .frame(maxWidth: 220)
                    .labelsHidden()
                let hidden = (row.secret || isSensitiveKey(row.key)) && !revealed.contains(row.id)
                Group {
                    if hidden {
                        SecureField("Value", text: $row.value)
                    } else {
                        TextField("Value", text: $row.value)
                    }
                }
                .font(.body.monospaced())
                .labelsHidden()
                if row.secret || isSensitiveKey(row.key) {
                    Button {
                        if revealed.contains(row.id) { revealed.remove(row.id) } else { revealed.insert(row.id) }
                    } label: {
                        Image(systemName: revealed.contains(row.id) ? "eye.slash" : "eye")
                    }
                    .buttonStyle(.borderless)
                    .help("Stored in Keychain")
                }
                Button(role: .destructive) {
                    rows.removeAll { $0.id == row.id }
                } label: {
                    Image(systemName: "minus.circle")
                }
                .buttonStyle(.borderless)
            }
        }
        Button {
            rows.append(KeyValue())
        } label: {
            Label("Add", systemImage: "plus")
        }
        .buttonStyle(.borderless)
    }
}

struct AddServerSheet: View {
    @Environment(AppStore.self) private var store
    @Environment(\.dismiss) private var dismiss
    @State private var draft = ServerDraft()
    @State private var busy = false

    var body: some View {
        VStack(spacing: 0) {
            Form {
                ServerForm(draft: $draft)
            }
            .formStyle(.grouped)
            HStack {
                if let message = draft.validationMessage, !draft.name.isEmpty {
                    Text(message).font(.caption).foregroundStyle(.secondary)
                }
                Spacer()
                Button("Cancel", role: .cancel) { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button("Add Server") {
                    guard let workspace = store.targetWorkspaceID else { return }
                    busy = true
                    Task {
                        if await store.addServer(to: workspace, draft.payload) != nil { dismiss() }
                        busy = false
                    }
                }
                .keyboardShortcut(.defaultAction)
                .disabled(draft.validationMessage != nil || busy)
            }
            .padding()
        }
        .frame(width: 560, height: 560)
    }
}
