import AppKit
import SwiftUI
import UniformTypeIdentifiers

struct ExportSheet: View {
    @Environment(AppStore.self) private var store
    @Environment(\.dismiss) private var dismiss
    @State private var workspaceID: String
    @State private var includeSecrets = false
    @State private var text = ""
    @State private var saving = false
    @State private var copied = false

    init(workspaceID: String) {
        _workspaceID = State(initialValue: workspaceID)
    }

    private var workspace: WorkspaceView? { store.workspace(workspaceID) }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text("Export Servers").font(.title3.weight(.semibold))
                Spacer()
                Picker("From", selection: $workspaceID) {
                    ForEach(store.workspaces) { Text($0.name).tag($0.id) }
                }
                .fixedSize()
            }
            Text("An mcpServers JSON that Claude Desktop, Cursor and Tulma's Import can read.")
                .font(.callout)
                .foregroundStyle(.secondary)
            Toggle("Include secrets", isOn: $includeSecrets)
            if includeSecrets {
                Label("Keychain tokens and keys are written in plain text. Keep the file private.", systemImage: "exclamationmark.triangle")
                    .font(.callout)
                    .foregroundStyle(.orange)
            } else {
                Text("Secret values are left empty. Tokens in URLs and arguments are masked.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
            ScrollView {
                Text(text)
                    .font(.caption.monospaced())
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(8)
            }
            .background(.background.secondary, in: RoundedRectangle(cornerRadius: 6))
            HStack {
                let count = workspace?.servers.count ?? 0
                Text("\(count) server\(count == 1 ? "" : "s")")
                    .foregroundStyle(.secondary)
                Spacer()
                Button("Cancel", role: .cancel) { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button(copied ? "Copied" : "Copy") {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(text, forType: .string)
                    copied = true
                }
                .disabled(text.isEmpty)
                Button("Save…") { saving = true }
                    .keyboardShortcut(.defaultAction)
                    .disabled(text.isEmpty)
            }
        }
        .padding(20)
        .frame(width: 640, height: 520)
        .task(id: [workspaceID, includeSecrets.description]) {
            copied = false
            text = await store.exportWorkspace(workspaceID, includeSecrets: includeSecrets) ?? ""
        }
        .fileExporter(
            isPresented: $saving,
            document: JSONFile(text: text),
            contentType: .json,
            defaultFilename: "\(workspace?.slug ?? "servers").mcp.json"
        ) { result in
            switch result {
            case .success: dismiss()
            case .failure(let error): store.errorMessage = error.localizedDescription
            }
        }
    }
}

struct JSONFile: FileDocument {
    static let readableContentTypes: [UTType] = [.json]

    var text: String

    init(text: String) {
        self.text = text
    }

    init(configuration: ReadConfiguration) throws {
        text = String(decoding: configuration.file.regularFileContents ?? Data(), as: UTF8.self)
    }

    func fileWrapper(configuration: WriteConfiguration) throws -> FileWrapper {
        FileWrapper(regularFileWithContents: Data(text.utf8))
    }
}
