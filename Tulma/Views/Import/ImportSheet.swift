import SwiftUI
import UniformTypeIdentifiers

struct ImportSheet: View {
    @Environment(AppStore.self) private var store
    @Environment(\.dismiss) private var dismiss
    @State private var text: String
    @State private var workspaceID: String = ""
    @State private var preview: ImportPreview?
    @State private var parseError: String?
    @State private var selected: Set<String> = []
    @State private var replace = false
    @State private var choosingFile = false
    @State private var busy = false

    init(seed: String) {
        _text = State(initialValue: seed)
    }

    private var items: [ImportItem] { preview?.items ?? [] }
    private var importable: [ImportItem] {
        items.filter { selected.contains($0.name) && $0.status.kind != "invalid" && ($0.status.kind == "new" || replace) }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text("Import Servers").font(.title3.weight(.semibold))
                Spacer()
                Picker("Into", selection: $workspaceID) {
                    ForEach(store.workspaces) { Text($0.name).tag($0.id) }
                }
                .fixedSize()
            }
            HStack {
                Text("Paste an mcpServers JSON (Claude Desktop, Cursor, VS Code) or choose a file.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                Spacer()
                Button("Choose File…") { choosingFile = true }
            }
            TextEditor(text: $text)
                .font(.caption.monospaced())
                .frame(height: 130)
                .scrollContentBackground(.hidden)
                .padding(4)
                .background(.background.secondary, in: RoundedRectangle(cornerRadius: 6))
            if let parseError {
                Label(parseError, systemImage: "exclamationmark.triangle").foregroundStyle(.red).font(.callout)
            }
            Table(items) {
                TableColumn("") { item in
                    Toggle("Import", isOn: Binding(
                        get: { selected.contains(item.name) },
                        set: { if $0 { selected.insert(item.name) } else { selected.remove(item.name) } }
                    ))
                    .labelsHidden()
                    .disabled(item.status.kind == "invalid")
                }
                .width(24)
                TableColumn("Name") { item in Text(item.name).fontWeight(.medium) }
                    .width(min: 90, ideal: 130)
                TableColumn("Transport") { item in
                    HStack(spacing: 6) {
                        if let t = item.transport { Badge(text: t.type) }
                        Text(item.transport?.summary ?? "—")
                            .font(.caption.monospaced())
                            .lineLimit(1)
                            .truncationMode(.middle)
                    }
                }
                TableColumn("Status") { item in status(item) }
                    .width(min: 120, ideal: 180)
            }
            .frame(minHeight: 220)
            HStack {
                Picker("Existing servers with the same name", selection: $replace) {
                    Text("Skip").tag(false)
                    Text("Replace").tag(true)
                }
                .fixedSize()
                Spacer()
                Button("Cancel", role: .cancel) { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button("Import \(importable.count) Server\(importable.count == 1 ? "" : "s")") {
                    busy = true
                    Task {
                        let names = importable.map(\.name)
                        if await store.importApply(text, into: workspaceID, names: names, replace: replace) != nil {
                            store.sidebar = .workspace(workspaceID)
                            dismiss()
                        }
                        busy = false
                    }
                }
                .keyboardShortcut(.defaultAction)
                .disabled(importable.isEmpty || busy)
            }
        }
        .padding(20)
        .frame(width: 760, height: 600)
        .onAppear { workspaceID = store.targetWorkspaceID ?? store.workspaces.first?.id ?? "" }
        .task(id: [text, workspaceID]) {
            try? await Task.sleep(for: .milliseconds(250))
            guard !Task.isCancelled else { return }
            await refreshPreview()
        }
        .fileImporter(isPresented: $choosingFile, allowedContentTypes: [.json, .plainText, .data]) { result in
            guard case .success(let url) = result else { return }
            let scoped = url.startAccessingSecurityScopedResource()
            defer { if scoped { url.stopAccessingSecurityScopedResource() } }
            if let contents = try? String(contentsOf: url, encoding: .utf8) { text = contents }
        }
    }

    @ViewBuilder
    private func status(_ item: ImportItem) -> some View {
        VStack(alignment: .leading, spacing: 1) {
            switch item.status.kind {
            case "new": Text("New").foregroundStyle(.green)
            case "duplicate": Text(replace ? "Replace existing" : "Exists — skip").foregroundStyle(.orange)
            default: Text("Invalid: \(item.status.reason ?? "")").foregroundStyle(.red)
            }
            if let warning = item.warning {
                Text(warning).font(.caption2).foregroundStyle(.secondary).lineLimit(2)
            }
        }
        .font(.callout)
    }

    private func refreshPreview() async {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, !workspaceID.isEmpty else {
            preview = nil
            parseError = nil
            return
        }
        do {
            let fresh = try await store.importPreview(text, into: workspaceID)
            let previous = Set(items.map(\.name))
            preview = fresh
            parseError = nil
            for item in fresh.items where !previous.contains(item.name) && item.status.kind == "new" {
                selected.insert(item.name)
            }
            selected.formIntersection(fresh.items.filter { $0.status.kind != "invalid" }.map(\.name))
        } catch {
            preview = nil
            parseError = error.localizedDescription
        }
    }
}
