import Foundation

// Mirrors docs/control-protocol.md. Field names follow the daemon's camelCase JSON.

struct KeyValue: Codable, Hashable, Sendable, Identifiable {
    var id = UUID()
    var key: String
    var value: String
    var secret: Bool

    init(key: String = "", value: String = "", secret: Bool = false) {
        self.key = key
        self.value = value
        self.secret = secret
    }

    enum CodingKeys: String, CodingKey { case key, value, secret }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        key = try c.decode(String.self, forKey: .key)
        value = try c.decode(String.self, forKey: .value)
        secret = try c.decodeIfPresent(Bool.self, forKey: .secret) ?? false
    }

    static func == (a: KeyValue, b: KeyValue) -> Bool {
        a.key == b.key && a.value == b.value && a.secret == b.secret
    }

    func hash(into hasher: inout Hasher) {
        hasher.combine(key)
        hasher.combine(value)
    }
}

struct StdioTransport: Codable, Hashable, Sendable {
    var command: String
    var args: [String]
    var env: [KeyValue]
    var cwd: String?
}

struct HttpTransport: Codable, Hashable, Sendable {
    var url: String
    var headers: [KeyValue]
}

enum TransportView: Codable, Hashable, Sendable {
    case stdio(StdioTransport)
    case http(HttpTransport)

    private enum CodingKeys: String, CodingKey { case type }

    init(from decoder: Decoder) throws {
        let type = try decoder.container(keyedBy: CodingKeys.self).decode(String.self, forKey: .type)
        switch type {
        case "http": self = .http(try HttpTransport(from: decoder))
        default: self = .stdio(try StdioTransport(from: decoder))
        }
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .stdio(let t):
            try c.encode("stdio", forKey: .type)
            try t.encode(to: encoder)
        case .http(let t):
            try c.encode("http", forKey: .type)
            try t.encode(to: encoder)
        }
    }

    var kind: String {
        switch self {
        case .stdio: "stdio"
        case .http: "http"
        }
    }

    var summary: String {
        switch self {
        case .stdio(let t): ([t.command] + t.args).joined(separator: " ")
        case .http(let t): t.url
        }
    }
}

struct ServerView: Codable, Hashable, Sendable, Identifiable {
    var id: String
    var name: String
    var slug: String
    var enabled: Bool
    var autoRestart: Bool
    var transport: TransportView
    var disabledTools: [String]
    /// Signed in through OAuth; the tokens live in the daemon's Keychain.
    var oauth: Bool
}

struct ServerPayload: Codable, Hashable, Sendable {
    var name: String
    var enabled: Bool
    var autoRestart: Bool
    var transport: TransportView
}

struct WorkspaceView: Codable, Hashable, Sendable, Identifiable {
    var id: String
    var name: String
    var slug: String
    var servers: [ServerView]
}

struct DaemonSettings: Codable, Hashable, Sendable {
    var port: Int
    var callTimeoutSecs: Int
    var logRetentionDays: Int
}

enum RunState: String, Codable, Sendable {
    case stopped, starting, running, error
}

struct ServerStatus: Codable, Hashable, Sendable {
    var state: RunState = .stopped
    var error: String?
    var pid: Int?
    var toolCount: Int = 0
    var promptCount: Int = 0
    var resourceCount: Int = 0
    var restartAttempt: Int = 0
    var serverName: String?
    var serverVersion: String?
    /// The server answered 401 and waits for the user to sign in.
    var needsAuth = false

    static let stopped = ServerStatus()
}

struct EndpointInfo: Codable, Hashable, Sendable {
    var url: String?
    var port: Int
    var running: Bool
    var error: String?
    var clients: Int
}

struct StateView: Codable, Hashable, Sendable {
    var activeWorkspace: String
    var workspaces: [WorkspaceView]
    var settings: DaemonSettings
    var statuses: [String: ServerStatus]
    var endpoint: EndpointInfo
}

/// Arbitrary JSON (tool input schemas) kept only for display.
enum JSONValue: Codable, Hashable, Sendable {
    case null
    case bool(Bool)
    case number(Double)
    case string(String)
    case array([JSONValue])
    case object([String: JSONValue])

    init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() { self = .null }
        else if let b = try? c.decode(Bool.self) { self = .bool(b) }
        else if let n = try? c.decode(Double.self) { self = .number(n) }
        else if let s = try? c.decode(String.self) { self = .string(s) }
        else if let a = try? c.decode([JSONValue].self) { self = .array(a) }
        else { self = .object(try c.decode([String: JSONValue].self)) }
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .null: try c.encodeNil()
        case .bool(let b): try c.encode(b)
        case .number(let n): try c.encode(n)
        case .string(let s): try c.encode(s)
        case .array(let a): try c.encode(a)
        case .object(let o): try c.encode(o)
        }
    }

    var pretty: String {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
        return (try? encoder.encode(self)).flatMap { String(data: $0, encoding: .utf8) } ?? ""
    }
}

struct ToolView: Codable, Hashable, Sendable, Identifiable {
    var name: String
    var exposedName: String
    var title: String?
    var description: String?
    var inputSchema: JSONValue
    var enabled: Bool
    var id: String { name }
}

struct ToolsView: Codable, Hashable, Sendable {
    var serverId: String
    var live: Bool
    var tools: [ToolView]
}

struct ImportTransport: Codable, Hashable, Sendable {
    var type: String
    var command: String?
    var args: [String]?
    var url: String?

    var summary: String {
        type == "http" ? (url ?? "") : ([command ?? ""] + (args ?? [])).joined(separator: " ")
    }
}

struct ImportStatus: Codable, Hashable, Sendable {
    var kind: String
    var reason: String?
}

struct ImportItem: Codable, Hashable, Sendable, Identifiable {
    var name: String
    var transport: ImportTransport?
    var enabled: Bool
    var status: ImportStatus
    var warning: String?
    var id: String { name }
}

struct ImportPreview: Codable, Hashable, Sendable {
    var items: [ImportItem]
}

struct ImportReport: Codable, Hashable, Sendable {
    var added: [String]
    var replaced: [String]
    var skipped: [String]
}

struct EventRow: Codable, Hashable, Sendable, Identifiable {
    var id: Int64
    var ts: Int64
    var kind: String
    var workspaceId: String?
    var serverId: String?
    var level: String
    var message: String
}

struct ToolCallRow: Codable, Hashable, Sendable, Identifiable {
    var id: Int64
    var ts: Int64
    var workspaceId: String?
    var serverId: String
    var serverName: String
    var tool: String
    var exposedTool: String
    var durationMs: Int64
    var ok: Bool
    var error: String?
    var argsJson: String
    var resultPreview: String
}

struct StderrRow: Codable, Hashable, Sendable, Identifiable {
    var id: Int64
    var ts: Int64
    var serverId: String
    var line: String
}

struct ServerStatusEvent: Codable, Sendable {
    var serverId: String
    var status: ServerStatus
}

struct ServerAuthEvent: Codable, Sendable {
    var serverId: String
    var ok: Bool
    var error: String?
}

struct AuthStart: Codable, Sendable {
    var authUrl: String
}

// MARK: Catalog

enum CatalogSource: String, Codable, CaseIterable, Identifiable, Sendable {
    case official, archestra
    var id: String { rawValue }
    var title: String { self == .official ? "MCP Registry" : "Archestra" }
}

enum InputKind: String, Codable, Sendable {
    case string, number, boolean, directory, file
}

struct InputField: Codable, Hashable, Sendable, Identifiable {
    var key: String
    var label: String
    var description: String?
    var required: Bool
    var secret: Bool
    var `default`: String?
    var kind: InputKind
    var choices: [String]
    var multiple: Bool
    var id: String { key }
}

struct InstallOption: Codable, Hashable, Sendable, Identifiable {
    var id: String
    var label: String
    var template: TransportView
    var inputs: [InputField]
    var oauth: Bool
    var note: String?
    var unsupported: String?
}

struct CatalogEntry: Codable, Hashable, Sendable, Identifiable {
    var id: String
    var source: CatalogSource
    var name: String
    var title: String
    var description: String
    var version: String?
    var repository: String?
    var website: String?
    var stars: Int?
    var category: String?
    var options: [InstallOption]

    var installable: Bool { options.contains { $0.unsupported == nil } }
}

struct CatalogPage: Codable, Hashable, Sendable {
    var entries: [CatalogEntry]
    var nextCursor: String?
}

struct RenderedInstall: Codable, Hashable, Sendable {
    var transport: TransportView
    var missing: [String]
}

struct CreatedID: Codable, Sendable {
    var id: String
}

struct Empty: Codable, Sendable {}

extension Int64 {
    var date: Date { Date(timeIntervalSince1970: TimeInterval(self) / 1000) }
}
