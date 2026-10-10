import SwiftUI

struct LogTableView: View {
    @Environment(AppStore.self) private var store
    @State private var filter = LogFilter()
    @State private var calls: [ToolCallRow] = []
    @State private var events: [EventRow] = []
    @State private var exhausted = false

    var body: some View {
        VStack(spacing: 0) {
            filters
            Divider()
            switch filter.table {
            case .toolCalls: callTable
            case .events: eventTable
            }
        }
        .navigationTitle("Logs")
        .task(id: filter) {
            try? await Task.sleep(for: .milliseconds(filter.text.isEmpty ? 0 : 250))
            guard !Task.isCancelled else { return }
            await reload()
        }
        .onChange(of: store.liveCalls.first?.id) { _, _ in
            if let row = store.liveCalls.first, matches(row) { calls.insert(row, at: 0) }
        }
        .onChange(of: store.liveEvents.first?.id) { _, _ in
            if let row = store.liveEvents.first, matches(row) { events.insert(row, at: 0) }
        }
        .onAppear { filter.table = store.logTable }
        .onChange(of: store.logTable) { _, table in filter.table = table }
    }

    /// Selection lives in the store so the table highlight and the detail pane never disagree.
    private var callSelection: Binding<ToolCallRow.ID?> {
        Binding(
            get: { if case .call(let row) = store.selectedLog { row.id } else { nil } },
            set: { id in store.selectedLog = calls.first { $0.id == id }.map(LogSelection.call) }
        )
    }

    private var eventSelection: Binding<EventRow.ID?> {
        Binding(
            get: { if case .event(let row) = store.selectedLog { row.id } else { nil } },
            set: { id in store.selectedLog = events.first { $0.id == id }.map(LogSelection.event) }
        )
    }

    private var filters: some View {
        @Bindable var store = store
        return HStack(spacing: 10) {
            Picker("Table", selection: $store.logTable) {
                ForEach(LogTable.allCases) { Text($0.title).tag($0) }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .fixedSize()
            Picker("Server", selection: $filter.serverId) {
                Text("All servers").tag(String?.none)
                ForEach(store.allServers) { Text($0.name).tag(Optional($0.id)) }
            }
            .fixedSize()
            if filter.table == .toolCalls {
                Picker("Status", selection: $filter.status) {
                    Text("Any status").tag(String?.none)
                    Text("OK").tag(Optional("ok"))
                    Text("Errors").tag(Optional("error"))
                }
                .fixedSize()
            }
            TextField("Search", text: $filter.text)
                .textFieldStyle(.roundedBorder)
                .frame(minWidth: 120)
        }
        .labelsHidden()
        .controlSize(.small)
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }

    private var callTable: some View {
        Table(calls, selection: callSelection) {
            TableColumn("Time") { Text(Self.time($0.ts)).monospacedDigit() }
                .width(min: 70, ideal: 80)
            TableColumn("Server", value: \.serverName)
                .width(min: 70, ideal: 100)
            TableColumn("Tool", value: \.tool)
            TableColumn("Duration") { Text("\($0.durationMs) ms").monospacedDigit() }
                .width(min: 60, ideal: 70)
            TableColumn("Status") { row in
                if row.ok {
                    Label("OK", systemImage: "checkmark.circle.fill").foregroundStyle(.green)
                } else {
                    Label("Error", systemImage: "xmark.octagon.fill").foregroundStyle(.red)
                }
            }
            .width(min: 60, ideal: 70)
        }
        .safeAreaInset(edge: .bottom, spacing: 0) { loadMore }
        .overlay {
            if calls.isEmpty {
                ContentUnavailableView("No tool calls", systemImage: "arrow.left.arrow.right", description: Text("Calls made through the endpoint are logged here."))
            }
        }
    }

    private var eventTable: some View {
        Table(events, selection: eventSelection) {
            TableColumn("Time") { Text(Self.time($0.ts)).monospacedDigit() }
                .width(min: 70, ideal: 80)
            TableColumn("Server") { row in Text(row.serverId.flatMap { store.server($0)?.name } ?? "—") }
                .width(min: 70, ideal: 100)
            TableColumn("Kind", value: \.kind)
                .width(min: 90, ideal: 120)
            TableColumn("Message") { row in
                Text(row.message)
                    .foregroundStyle(row.level == "error" ? .red : row.level == "warn" ? .orange : .primary)
            }
        }
        .safeAreaInset(edge: .bottom, spacing: 0) { loadMore }
        .overlay {
            if events.isEmpty {
                ContentUnavailableView("No events", systemImage: "list.bullet.rectangle")
            }
        }
    }

    private var loadMore: some View {
        HStack {
            Text("\(filter.table == .toolCalls ? calls.count : events.count) rows")
                .font(.caption)
                .foregroundStyle(.secondary)
            Spacer()
            Button("Load Older") { Task { await loadOlder() } }
                .disabled(exhausted)
        }
        .controlSize(.small)
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
        .background(.bar)
    }

    private func reload() async {
        exhausted = false
        switch filter.table {
        case .toolCalls: calls = await store.queryToolCalls(filter)
        case .events: events = await store.queryEvents(filter)
        }
    }

    private func loadOlder() async {
        switch filter.table {
        case .toolCalls:
            let older = await store.queryToolCalls(filter, before: calls.last { $0.id > 0 })
            exhausted = older.isEmpty
            calls.append(contentsOf: older)
        case .events:
            let older = await store.queryEvents(filter, before: events.last { $0.id > 0 })
            exhausted = older.isEmpty
            events.append(contentsOf: older)
        }
    }

    private func matches(_ row: ToolCallRow) -> Bool {
        guard filter.table == .toolCalls else { return false }
        if let server = filter.serverId, row.serverId != server { return false }
        if filter.status == "ok", !row.ok { return false }
        if filter.status == "error", row.ok { return false }
        if !filter.text.isEmpty {
            let haystack = [row.exposedTool, row.serverName, row.error ?? "", row.argsJson].joined(separator: " ")
            if !haystack.localizedCaseInsensitiveContains(filter.text) { return false }
        }
        return true
    }

    private func matches(_ row: EventRow) -> Bool {
        guard filter.table == .events else { return false }
        if let server = filter.serverId, row.serverId != server { return false }
        return filter.text.isEmpty || row.message.localizedCaseInsensitiveContains(filter.text)
    }

    static func time(_ ts: Int64) -> String {
        ts.date.formatted(.dateTime.hour(.twoDigits(amPM: .omitted)).minute(.twoDigits).second(.twoDigits))
    }
}

struct LogEntryDetail: View {
    @Environment(AppStore.self) private var store
    let entry: LogSelection

    var body: some View {
        Form {
            switch entry {
            case .call(let row):
                Section(row.exposedTool) {
                    LabeledContent("Server", value: row.serverName)
                    LabeledContent("Tool", value: row.tool)
                    LabeledContent("Time", value: row.ts.date.formatted(date: .abbreviated, time: .standard))
                    LabeledContent("Duration", value: "\(row.durationMs) ms")
                    LabeledContent("Status") {
                        Text(row.ok ? "OK" : "Error").foregroundStyle(row.ok ? .green : .red)
                    }
                    if let error = row.error {
                        Text(error).foregroundStyle(.red).textSelection(.enabled)
                    }
                }
                Section("Arguments") { code(prettyJSON(row.argsJson)) }
                Section("Result preview") { code(prettyJSON(row.resultPreview)) }
            case .event(let row):
                Section(row.kind) {
                    LabeledContent("Time", value: row.ts.date.formatted(date: .abbreviated, time: .standard))
                    LabeledContent("Level", value: row.level)
                    if let server = row.serverId.flatMap({ store.server($0) }) {
                        LabeledContent("Server", value: server.name)
                    }
                }
                Section("Message") { code(row.message) }
            }
        }
        .formStyle(.grouped)
    }

    private func code(_ text: String) -> some View {
        ScrollView(.horizontal) {
            Text(text.isEmpty ? "—" : text)
                .font(.caption.monospaced())
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}
