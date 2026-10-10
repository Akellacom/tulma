import Foundation
import Observation

enum SidebarItem: Hashable {
    case workspace(String)
    case logs
}

enum LogTable: String, CaseIterable, Identifiable {
    case toolCalls, events
    var id: String { rawValue }
    var title: String { self == .toolCalls ? "Tool calls" : "Events" }
}

enum LogSelection: Hashable {
    case call(ToolCallRow)
    case event(EventRow)
}

struct LogFilter: Hashable {
    var table: LogTable = .toolCalls
    var serverId: String?
    var status: String?
    var text = ""
}

private struct LogsQuery: Encodable, Sendable {
    var table: String
    var serverId: String?
    var status: String?
    var text: String?
    var limit: Int
    var before: Cursor?

    struct Cursor: Encodable, Sendable {
        var ts: Int64
        var id: Int64
    }
}

@MainActor
@Observable
final class AppStore {
    static let liveLimit = 1000
    static let stderrLimit = 500

    private(set) var state: StateView?
    private(set) var statuses: [String: ServerStatus] = [:]
    private(set) var tools: [String: ToolsView] = [:]
    private(set) var endpoint: EndpointInfo?
    private(set) var liveCalls: [ToolCallRow] = []
    private(set) var liveEvents: [EventRow] = []
    private(set) var stderr: [String: [StderrRow]] = [:]
    var sidebar: SidebarItem? {
        didSet {
            guard case .workspace(let id) = sidebar, let selected = selectedServer else { return }
            if workspace(id)?.servers.contains(where: { $0.id == selected }) != true {
                selectedServer = nil
            }
        }
    }
    var selectedServer: String?
    var selectedLog: LogSelection?
    var logTable: LogTable = .toolCalls {
        didSet { if logTable != oldValue { selectedLog = nil } }
    }
    var errorMessage: String?
    var daemonFailure: String?
    var showImport = false
    var importSeed = ""
    var showExport = false
    var exportSeed: String?
    var showAddServer = false
    var promptNewWorkspace = false

    let client: DaemonClient
    /// Live rows are not stored yet, so they get negative ids that never clash with database ids.
    private var nextLiveID: Int64 = -1
    private var listener: Task<Void, Never>?
    private var quitting = false

    init(client: DaemonClient = DaemonClient()) {
        self.client = client
    }

    // MARK: Lifecycle

    static var dataDirectory: URL {
        if let custom = ProcessInfo.processInfo.environment["TULMA_DATA_DIR"] {
            return URL(filePath: custom)
        }
        return URL.applicationSupportDirectory.appending(path: "Tulma")
    }

    static var daemonExecutable: URL {
        if let custom = ProcessInfo.processInfo.environment["TULMA_DAEMON"] {
            return URL(filePath: custom)
        }
        return Bundle.main.bundleURL.appending(path: "Contents/Helpers/tulma")
    }

    func launch() async {
        if listener == nil {
            let events = client.events
            listener = Task { [weak self] in
                for await event in events {
                    self?.handle(event)
                }
            }
        }
        do {
            try await client.start(
                executable: Self.daemonExecutable,
                log: Self.dataDirectory.appending(path: "daemon.log")
            )
            daemonFailure = nil
            try await reload()
        } catch {
            daemonFailure = error.localizedDescription
        }
    }

    func reload() async throws {
        let snapshot: StateView = try await client.call("state.get")
        applyState(snapshot)
    }

    func shutdown() async {
        quitting = true
        await client.stop()
    }

    private func handle(_ event: DaemonClient.Event) {
        switch event {
        case .notification(let method, let payload):
            apply(method: method, payload: payload)
        case .terminated(let status):
            guard !quitting else { return }
            daemonFailure = "The Tulma daemon stopped unexpectedly (status \(status)). Clients are disconnected until it restarts."
        }
    }

    func restartDaemon() async {
        await client.stop(timeout: .seconds(2))
        await launch()
    }

    // MARK: Notifications

    func apply(method: String, payload: Data) {
        func decode<P: Decodable>(_ type: P.Type) -> P? {
            notificationParams(type, from: payload)
        }
        switch method {
        case "state.changed":
            if let snapshot = decode(StateView.self) { applyState(snapshot) }
        case "server.status":
            if let e = decode(ServerStatusEvent.self) { statuses[e.serverId] = e.status }
        case "server.tools":
            if let view = decode(ToolsView.self) { tools[view.serverId] = view }
        case "log.toolCall":
            if var row = decode(ToolCallRow.self) {
                row.id = takeLiveID()
                prepend(row, to: &liveCalls)
            }
        case "log.event":
            if var row = decode(EventRow.self) {
                row.id = takeLiveID()
                prepend(row, to: &liveEvents)
            }
        case "log.stderr":
            if var row = decode(StderrRow.self) {
                row.id = takeLiveID()
                var lines = stderr[row.serverId, default: []]
                lines.append(row)
                if lines.count > Self.stderrLimit { lines.removeFirst(lines.count - Self.stderrLimit) }
                stderr[row.serverId] = lines
            }
        case "endpoint.changed":
            if let info = decode(EndpointInfo.self) { endpoint = info }
        case "endpoint.error":
            struct PortError: Decodable { var port: Int; var message: String }
            if let e = decode(PortError.self) {
                endpoint?.running = false
                endpoint?.error = e.message
                errorMessage = "Endpoint on port \(e.port) is unavailable: \(e.message)"
            }
        default:
            break
        }
    }

    private func takeLiveID() -> Int64 {
        defer { nextLiveID -= 1 }
        return nextLiveID
    }

    private func prepend<T>(_ row: T, to list: inout [T]) {
        list.insert(row, at: 0)
        if list.count > Self.liveLimit { list.removeLast(list.count - Self.liveLimit) }
    }

    private func applyState(_ snapshot: StateView) {
        state = snapshot
        statuses = snapshot.statuses
        endpoint = snapshot.endpoint
        let selectionGone: Bool = switch sidebar {
        case .workspace(let id): !snapshot.workspaces.contains { $0.id == id }
        case .logs: false
        case nil: true
        }
        if selectionGone {
            sidebar = .workspace(snapshot.activeWorkspace)
        }
        if let selected = selectedServer, server(selected) == nil {
            selectedServer = nil
        }
    }

    // MARK: Queries

    var workspaces: [WorkspaceView] { state?.workspaces ?? [] }
    var activeWorkspaceID: String? { state?.activeWorkspace }
    var activeWorkspace: WorkspaceView? { workspaces.first { $0.id == state?.activeWorkspace } }

    func workspace(_ id: String) -> WorkspaceView? { workspaces.first { $0.id == id } }

    func server(_ id: String) -> ServerView? {
        workspaces.lazy.flatMap(\.servers).first { $0.id == id }
    }

    func status(_ id: String) -> ServerStatus { statuses[id] ?? .stopped }

    var allServers: [ServerView] { workspaces.flatMap(\.servers) }

    var runningCount: Int {
        activeWorkspace?.servers.filter { status($0.id).state == .running }.count ?? 0
    }

    var enabledCount: Int { activeWorkspace?.servers.filter(\.enabled).count ?? 0 }

    var hasErrors: Bool {
        activeWorkspace?.servers.contains { status($0.id).state == .error } ?? false
    }

    var endpointURL: String? { endpoint?.url }

    /// Workspace new servers and imports go to: the one on screen, else the active one.
    var targetWorkspaceID: String? {
        if case .workspace(let id) = sidebar { return id }
        return activeWorkspaceID
    }

    // MARK: Actions

    private func run(_ operation: () async throws -> Void) async {
        do {
            try await operation()
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    private struct ID: Encodable, Sendable { var id: String }
    private struct IDName: Encodable, Sendable { var id: String; var name: String? }
    private struct Name: Encodable, Sendable { var name: String }

    func createWorkspace(named name: String) async {
        await run {
            let created: CreatedID = try await client.call("workspace.create", Name(name: name))
            sidebar = .workspace(created.id)
        }
    }

    func renameWorkspace(_ id: String, to name: String) async {
        await run { try await client.perform("workspace.rename", IDName(id: id, name: name)) }
    }

    func duplicateWorkspace(_ id: String) async {
        await run {
            let created: CreatedID = try await client.call("workspace.duplicate", IDName(id: id, name: nil))
            sidebar = .workspace(created.id)
        }
    }

    func deleteWorkspace(_ id: String) async {
        await run { try await client.perform("workspace.delete", ID(id: id)) }
    }

    func activateWorkspace(_ id: String) async {
        await run { try await client.perform("workspace.activate", ID(id: id)) }
    }

    private struct AddServer: Encodable, Sendable { var workspaceId: String; var server: ServerPayload }
    private struct UpdateServer: Encodable, Sendable { var id: String; var server: ServerPayload }
    private struct Enabled: Encodable, Sendable { var id: String; var enabled: Bool }
    private struct ToolToggle: Encodable, Sendable { var id: String; var tool: String; var enabled: Bool }

    @discardableResult
    func addServer(to workspace: String, _ payload: ServerPayload) async -> String? {
        do {
            let created: CreatedID = try await client.call("server.add", AddServer(workspaceId: workspace, server: payload))
            selectedServer = created.id
            return created.id
        } catch {
            errorMessage = error.localizedDescription
            return nil
        }
    }

    func updateServer(_ id: String, _ payload: ServerPayload) async {
        await run { try await client.perform("server.update", UpdateServer(id: id, server: payload)) }
    }

    func deleteServer(_ id: String) async {
        await run { try await client.perform("server.delete", ID(id: id)) }
    }

    func setEnabled(_ id: String, _ enabled: Bool) async {
        await run { try await client.perform("server.setEnabled", Enabled(id: id, enabled: enabled)) }
    }

    func start(_ id: String) async { await run { try await client.perform("server.start", ID(id: id)) } }
    func stop(_ id: String) async { await run { try await client.perform("server.stop", ID(id: id)) } }
    func restart(_ id: String) async { await run { try await client.perform("server.restart", ID(id: id)) } }

    func setTool(_ server: String, _ tool: String, enabled: Bool) async {
        await run { try await client.perform("server.setToolEnabled", ToolToggle(id: server, tool: tool, enabled: enabled)) }
    }

    private struct ServerRef: Encodable, Sendable { var serverId: String }
    private struct StderrQuery: Encodable, Sendable { var serverId: String; var limit: Int }

    func loadTools(_ server: String) async {
        await run {
            let view: ToolsView = try await client.call("tools.list", ServerRef(serverId: server))
            tools[server] = view
        }
    }

    func loadStderr(_ server: String) async {
        await run {
            let rows: [StderrRow] = try await client.call("logs.stderr", StderrQuery(serverId: server, limit: Self.stderrLimit))
            stderr[server] = rows
        }
    }

    func clearStderr(_ server: String) {
        stderr[server] = []
    }

    private struct ImportText: Encodable, Sendable { var workspaceId: String; var text: String }
    private struct ImportApply: Encodable, Sendable {
        var workspaceId: String
        var text: String
        var names: [String]
        var duplicatePolicy: String
    }

    func importPreview(_ text: String, into workspace: String) async throws -> ImportPreview {
        try await client.call("import.preview", ImportText(workspaceId: workspace, text: text))
    }

    func importApply(_ text: String, into workspace: String, names: [String], replace: Bool) async -> ImportReport? {
        do {
            return try await client.call(
                "import.apply",
                ImportApply(workspaceId: workspace, text: text, names: names, duplicatePolicy: replace ? "replace" : "skip")
            )
        } catch {
            errorMessage = error.localizedDescription
            return nil
        }
    }

    private struct ExportWorkspace: Encodable, Sendable { var id: String; var includeSecrets: Bool }
    private struct ExportedText: Decodable, Sendable { var text: String }

    func exportWorkspace(_ id: String, includeSecrets: Bool) async -> String? {
        do {
            let exported: ExportedText = try await client.call(
                "workspace.export",
                ExportWorkspace(id: id, includeSecrets: includeSecrets)
            )
            return exported.text
        } catch {
            errorMessage = error.localizedDescription
            return nil
        }
    }

    func queryToolCalls(_ filter: LogFilter, before: ToolCallRow? = nil) async -> [ToolCallRow] {
        await query(filter, cursor: before.map { .init(ts: $0.ts, id: $0.id) })
    }

    func queryEvents(_ filter: LogFilter, before: EventRow? = nil) async -> [EventRow] {
        await query(filter, cursor: before.map { .init(ts: $0.ts, id: $0.id) })
    }

    private func query<R: Decodable & Sendable>(_ filter: LogFilter, cursor: LogsQuery.Cursor?) async -> [R] {
        let q = LogsQuery(
            table: filter.table.rawValue,
            serverId: filter.serverId,
            status: filter.table == .toolCalls ? filter.status : nil,
            text: filter.text.isEmpty ? nil : filter.text,
            limit: 300,
            before: cursor
        )
        do {
            return try await client.call("logs.query", q)
        } catch {
            errorMessage = error.localizedDescription
            return []
        }
    }

    func restartEndpoint() async {
        await run {
            let info: EndpointInfo = try await client.call("endpoint.restart")
            endpoint = info
        }
    }

    private struct SettingsPatch: Encodable, Sendable {
        var port: Int?
        var callTimeoutSecs: Int?
        var logRetentionDays: Int?
    }

    func updateSettings(_ settings: DaemonSettings) async {
        await run {
            let _: DaemonSettings = try await client.call(
                "settings.update",
                SettingsPatch(port: settings.port, callTimeoutSecs: settings.callTimeoutSecs, logRetentionDays: settings.logRetentionDays)
            )
        }
    }
}

private func notificationParams<P: Decodable>(_: P.Type, from payload: Data) -> P? {
    do {
        return try JSONDecoder().decode(NotificationEnvelope<P>.self, from: payload).params
    } catch {
        return nil
    }
}
