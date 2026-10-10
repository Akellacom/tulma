import Foundation
import Testing

/// Runs the real `tulma daemon` built by `make core-debug`.
struct DaemonClientTests {
    private static var repoRoot: URL {
        URL(filePath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
    }

    private static func binary(_ env: String, _ name: String) -> URL {
        ProcessInfo.processInfo.environment[env].map { URL(filePath: $0) }
            ?? repoRoot.appending(path: "core/target/debug/\(name)")
    }

    private func makeDataDir() throws -> URL {
        let dir = FileManager.default.temporaryDirectory.appending(path: "tulma-tests-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let config = #"{"activeWorkspace":"w","workspaces":[{"id":"w","name":"Test","slug":"test","servers":[]}],"settings":{"port":0}}"#
        try Data(config.utf8).write(to: dir.appending(path: "config.json"))
        return dir
    }

    private struct AddServer: Encodable, Sendable { var workspaceId: String; var server: ServerPayload }
    private struct Name: Encodable, Sendable { var name: String }

    @Test(.timeLimit(.minutes(1)))
    func roundTripsRequestsAndStreamsNotifications() async throws {
        let dir = try makeDataDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let client = DaemonClient()
        try await client.start(
            executable: Self.binary("TULMA_TEST_DAEMON", "tulma"),
            arguments: ["daemon", "--data-dir", dir.path, "--memory-secrets"]
        )

        let state: StateView = try await client.call("state.get")
        #expect(state.workspaces.map(\.name) == ["Test"])
        #expect(state.endpoint.running)

        let echo = Self.binary("TULMA_TEST_ECHO", "echo-server")
        let created: CreatedID = try await client.call("server.add", AddServer(
            workspaceId: "w",
            server: ServerPayload(name: "echo", enabled: true, autoRestart: true,
                                  transport: .stdio(StdioTransport(command: echo.path, args: [], env: [], cwd: nil)))
        ))

        var running = false
        for await event in client.events {
            guard case .notification(let method, let payload) = event, method == "server.status" else { continue }
            let status = try JSONDecoder().decode(NotificationEnvelope<ServerStatusEvent>.self, from: payload).params
            if status.serverId == created.id, status.status.state == .running {
                running = true
                break
            }
        }
        #expect(running)

        await #expect(throws: DaemonError.self) {
            try await client.perform("workspace.activate", ["id": "missing"])
        }

        await client.stop()
        #expect(await client.isRunning == false)
    }
}
