import SwiftUI
import UniformTypeIdentifiers

struct CatalogSheet: View {
    @Environment(AppStore.self) private var store
    @Environment(\.dismiss) private var dismiss
    @State private var source: CatalogSource = .official
    @State private var query = ""
    @State private var entries: [CatalogEntry] = []
    @State private var nextCursor: String?
    @State private var loading = false
    @State private var loadError: String?
    @State private var selection: CatalogEntry.ID?
    @State private var workspaceID = ""

    private struct SearchKey: Hashable {
        var source: CatalogSource
        var query: String
    }

    private var selected: CatalogEntry? { entries.first { $0.id == selection } }

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            HStack(spacing: 0) {
                results
                    .frame(width: 330)
                Divider()
                Group {
                    if let entry = selected {
                        CatalogDetail(entry: entry, workspaceID: workspaceID) { dismiss() }
                            .id(entry.id)
                    } else {
                        ContentUnavailableView("Select a server", systemImage: "square.grid.2x2")
                    }
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .frame(width: 960, height: 640)
        .onAppear { workspaceID = store.targetWorkspaceID ?? store.activeWorkspaceID ?? "" }
        .task(id: SearchKey(source: source, query: query)) {
            try? await Task.sleep(for: .milliseconds(query.isEmpty ? 0 : 350))
            guard !Task.isCancelled else { return }
            await load(reset: true)
        }
    }

    private var header: some View {
        HStack(spacing: 10) {
            Text("Browse Catalog").font(.title3.weight(.semibold))
            Picker("Source", selection: $source) {
                ForEach(CatalogSource.allCases) { Text($0.title).tag($0) }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .fixedSize()
            TextField("Search servers", text: $query)
                .textFieldStyle(.roundedBorder)
                .frame(maxWidth: .infinity)
            Picker("Add to", selection: $workspaceID) {
                ForEach(store.workspaces) { Text($0.name).tag($0.id) }
            }
            .fixedSize()
            Button("Done") { dismiss() }
                .keyboardShortcut(.cancelAction)
        }
        .padding(12)
    }

    private var results: some View {
        List(selection: $selection) {
            ForEach(entries) { entry in
                CatalogRow(entry: entry).tag(entry.id)
            }
            if nextCursor != nil {
                HStack {
                    Spacer()
                    if loading {
                        ProgressView().controlSize(.small)
                    } else {
                        Button("Load More") { Task { await load(reset: false) } }
                            .buttonStyle(.borderless)
                    }
                    Spacer()
                }
            }
        }
        .overlay {
            if let loadError {
                ContentUnavailableView {
                    Label("Catalog unavailable", systemImage: "wifi.exclamationmark")
                } description: {
                    Text(loadError)
                } actions: {
                    Button("Retry") { Task { await load(reset: true) } }
                }
            } else if entries.isEmpty {
                if loading {
                    ProgressView()
                } else {
                    ContentUnavailableView.search(text: query)
                }
            }
        }
    }

    private func load(reset: Bool) async {
        let cursor = reset ? nil : nextCursor
        loading = true
        defer { loading = false }
        do {
            let page = try await store.searchCatalog(source, query: query, cursor: cursor)
            guard !Task.isCancelled else { return }
            loadError = nil
            if reset {
                entries = page.entries
                selection = page.entries.first?.id
            } else {
                let known = Set(entries.map(\.id))
                entries += page.entries.filter { !known.contains($0.id) }
            }
            nextCursor = page.nextCursor
        } catch {
            guard !Task.isCancelled else { return }
            if reset { entries = [] }
            nextCursor = nil
            loadError = error.localizedDescription
        }
    }
}

private struct CatalogRow: View {
    let entry: CatalogEntry

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack(spacing: 6) {
                Text(entry.title).fontWeight(.medium).lineLimit(1)
                ForEach(kinds, id: \.self) { Badge(text: $0) }
                Spacer(minLength: 0)
                if let stars = entry.stars {
                    Label(stars.formatted(.number.notation(.compactName)), systemImage: "star")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .labelStyle(.titleAndIcon)
                }
            }
            if !entry.description.isEmpty {
                Text(entry.description)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
            }
        }
        .padding(.vertical, 2)
        .opacity(entry.installable ? 1 : 0.55)
    }

    private var kinds: [String] {
        var seen: [String] = []
        for option in entry.options where option.unsupported == nil {
            let kind = option.template.kind == "http" ? "remote" : "local"
            if !seen.contains(kind) { seen.append(kind) }
        }
        return seen
    }
}

private struct CatalogDetail: View {
    let entry: CatalogEntry
    let workspaceID: String
    let onAdded: () -> Void
    @State private var optionID: String

    init(entry: CatalogEntry, workspaceID: String, onAdded: @escaping () -> Void) {
        self.entry = entry
        self.workspaceID = workspaceID
        self.onAdded = onAdded
        _optionID = State(initialValue: entry.options.first?.id ?? "")
    }

    private var option: InstallOption? { entry.options.first { $0.id == optionID } }

    var body: some View {
        VStack(spacing: 0) {
            Form {
                Section {
                    VStack(alignment: .leading, spacing: 6) {
                        Text(entry.title).font(.title2.weight(.semibold))
                        HStack(spacing: 6) {
                            Badge(text: entry.source.title)
                            if let version = entry.version { Badge(text: "v\(version)") }
                            if let category = entry.category { Badge(text: category) }
                        }
                        Text(entry.name)
                            .font(.caption.monospaced())
                            .foregroundStyle(.secondary)
                            .textSelection(.enabled)
                        if !entry.description.isEmpty {
                            Text(entry.description).textSelection(.enabled)
                        }
                        HStack(spacing: 14) {
                            if let repo = entry.repository.flatMap(URL.init(string:)) {
                                Link("Repository", destination: repo)
                            }
                            if let site = entry.website.flatMap(URL.init(string:)) {
                                Link("Website", destination: site)
                            }
                        }
                        .font(.callout)
                    }
                    .padding(.vertical, 4)
                }
                if entry.options.count > 1 {
                    Section {
                        Picker("Install as", selection: $optionID) {
                            ForEach(entry.options) { option in
                                Text(option.unsupported == nil ? option.label : "\(option.label) (not supported)")
                                    .tag(option.id)
                            }
                        }
                    }
                }
                if entry.options.isEmpty {
                    Section {
                        Label("The catalog lists no way to run this server.", systemImage: "exclamationmark.triangle")
                            .foregroundStyle(.secondary)
                    }
                }
            }
            .formStyle(.grouped)
            .frame(maxHeight: entry.options.count > 1 ? 290 : 250)
            if let option {
                InstallForm(entry: entry, option: option, workspaceID: workspaceID, onAdded: onAdded)
                    .id(option.id)
            } else {
                Spacer()
            }
        }
    }
}

private struct InstallForm: View {
    @Environment(AppStore.self) private var store
    @Environment(\.openURL) private var openURL
    let entry: CatalogEntry
    let option: InstallOption
    let workspaceID: String
    let onAdded: () -> Void
    @State private var name: String
    @State private var values: [String: String]
    @State private var preview: RenderedInstall?
    @State private var previewError: String?
    @State private var picking: InputField?
    @State private var busy = false

    init(entry: CatalogEntry, option: InstallOption, workspaceID: String, onAdded: @escaping () -> Void) {
        self.entry = entry
        self.option = option
        self.workspaceID = workspaceID
        self.onAdded = onAdded
        _name = State(initialValue: entry.title)
        _values = State(initialValue: Dictionary(
            option.inputs.compactMap { input in input.default.map { (input.key, $0) } },
            uniquingKeysWith: { first, _ in first }
        ))
    }

    private var missing: [String] {
        option.inputs.filter { $0.required && (values[$0.key] ?? "").trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }
            .map(\.label)
    }

    private var blocker: String? {
        if let reason = option.unsupported { return reason }
        if name.trimmingCharacters(in: .whitespaces).isEmpty { return "Name is required." }
        if !missing.isEmpty { return "Fill in: \(missing.joined(separator: ", "))." }
        if workspaceID.isEmpty { return "Choose a workspace." }
        return nil
    }

    var body: some View {
        VStack(spacing: 0) {
            Form {
                if let reason = option.unsupported {
                    Section {
                        Label(reason, systemImage: "exclamationmark.triangle")
                            .foregroundStyle(.orange)
                    }
                } else {
                    Section("Server") {
                        TextField("Name", text: $name)
                        if let note = option.note {
                            Label(note, systemImage: "info.circle")
                                .font(.callout)
                                .foregroundStyle(.secondary)
                        }
                        if option.oauth {
                            Label("You sign in through the browser right after adding it.", systemImage: "person.badge.key")
                                .font(.callout)
                                .foregroundStyle(.secondary)
                        }
                    }
                    if !option.inputs.isEmpty {
                        Section("Configuration") {
                            ForEach(option.inputs) { input in
                                InputRow(input: input, value: binding(input.key)) { picking = input }
                            }
                        }
                    }
                    Section {
                        commandPreview
                    } header: {
                        Text(option.template.kind == "http" ? "Connects to" : "Runs on your Mac")
                    } footer: {
                        if option.template.kind != "http" {
                            Text("Check the command before adding: it runs with your user's permissions.")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }
                    }
                }
            }
            .formStyle(.grouped)
            Divider()
            HStack {
                if let blocker, option.unsupported == nil {
                    Text(blocker).font(.caption).foregroundStyle(.secondary).lineLimit(2)
                }
                Spacer()
                Button("Add to \(store.workspace(workspaceID)?.name ?? "Workspace")") { Task { await add() } }
                    .keyboardShortcut(.defaultAction)
                    .buttonStyle(.borderedProminent)
                    .disabled(blocker != nil || busy)
            }
            .padding(12)
        }
        .task(id: values) {
            try? await Task.sleep(for: .milliseconds(200))
            guard !Task.isCancelled, option.unsupported == nil else { return }
            do {
                preview = try await store.renderInstall(option, values: values)
                previewError = nil
            } catch {
                previewError = error.localizedDescription
            }
        }
        .fileImporter(
            isPresented: Binding(get: { picking != nil }, set: { if !$0 { picking = nil } }),
            allowedContentTypes: picking?.kind == .directory ? [.folder] : [.item]
        ) { result in
            guard let input = picking, case .success(let url) = result else { return }
            let path = url.path(percentEncoded: false)
            let current = values[input.key] ?? ""
            values[input.key] = input.multiple && !current.isEmpty ? current + "\n" + path : path
        }
    }

    @ViewBuilder
    private var commandPreview: some View {
        if let previewError {
            Text(previewError).foregroundStyle(.red).font(.callout)
        } else if let preview {
            VStack(alignment: .leading, spacing: 6) {
                Text(summary(preview.transport))
                    .font(.callout.monospaced())
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                ForEach(pairs(preview.transport), id: \.key) { kv in
                    HStack(spacing: 4) {
                        Text(kv.key).font(.caption.monospaced())
                        Text("=").font(.caption).foregroundStyle(.secondary)
                        Text(kv.secret ? "•••• (Keychain)" : kv.value)
                            .font(.caption.monospaced())
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                            .truncationMode(.middle)
                    }
                }
            }
        } else {
            ProgressView().controlSize(.small)
        }
    }

    private func summary(_ transport: TransportView) -> String {
        switch transport {
        case .http(let t): t.url
        case .stdio(let t): ([t.command] + t.args.map(quoted)).joined(separator: " ")
        }
    }

    private func quoted(_ arg: String) -> String {
        arg.contains(" ") || arg.isEmpty ? "\"\(arg)\"" : arg
    }

    private func pairs(_ transport: TransportView) -> [KeyValue] {
        switch transport {
        case .http(let t): t.headers
        case .stdio(let t): t.env
        }
    }

    private func binding(_ key: String) -> Binding<String> {
        Binding(get: { values[key] ?? "" }, set: { values[key] = $0 })
    }

    private func add() async {
        busy = true
        defer { busy = false }
        do {
            let rendered = try await store.renderInstall(option, values: values)
            guard rendered.missing.isEmpty else {
                previewError = "Missing values: \(rendered.missing.joined(separator: ", "))"
                return
            }
            let payload = ServerPayload(
                name: name.trimmingCharacters(in: .whitespaces),
                enabled: true,
                autoRestart: true,
                transport: rendered.transport
            )
            guard let id = await store.addServer(to: workspaceID, payload) else { return }
            store.sidebar = .workspace(workspaceID)
            store.selectedServer = id
            if option.oauth, let url = await store.signIn(id) {
                openURL(url)
            }
            onAdded()
        } catch {
            previewError = error.localizedDescription
        }
    }
}

private struct InputRow: View {
    let input: InputField
    @Binding var value: String
    let pick: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            field
            if let description = input.description {
                Text(description)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    private var title: String { input.required ? "\(input.label) *" : input.label }

    @ViewBuilder
    private var field: some View {
        if input.kind == .boolean {
            Toggle(title, isOn: Binding(get: { value == "true" }, set: { value = $0 ? "true" : "false" }))
        } else if !input.choices.isEmpty {
            Picker(title, selection: $value) {
                if !input.required { Text("—").tag("") }
                ForEach(input.choices, id: \.self) { Text($0).tag($0) }
            }
        } else if input.multiple {
            LabeledContent(title) {
                VStack(alignment: .trailing, spacing: 4) {
                    TextEditor(text: $value)
                        .font(.body.monospaced())
                        .frame(minHeight: 44, maxHeight: 90)
                        .scrollContentBackground(.hidden)
                        .padding(4)
                        .background(.background.secondary, in: RoundedRectangle(cornerRadius: 6))
                    if input.kind == .directory || input.kind == .file {
                        Button("Add…", action: pick).controlSize(.small)
                    }
                }
            }
        } else {
            HStack(spacing: 6) {
                if input.secret {
                    SecureField(title, text: $value, prompt: Text("stored in Keychain"))
                } else {
                    TextField(title, text: $value, prompt: input.kind == .number ? Text("number") : nil)
                }
                if input.kind == .directory || input.kind == .file {
                    Button("Choose…", action: pick).controlSize(.small)
                }
            }
            .font(input.kind == .string && !input.secret ? .body : .body.monospaced())
        }
    }
}
