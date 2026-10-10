import Foundation
import Testing

struct ProtocolTests {
    static let stateJSON = """
    {"activeWorkspace":"w1","workspaces":[{"id":"w1","name":"Default","slug":"default","servers":[
      {"id":"s1","name":"git","slug":"git","enabled":true,"autoRestart":true,"disabledTools":["log"],
       "transport":{"type":"stdio","command":"uvx","args":["mcp-server-git"],"env":[{"key":"GH_TOKEN","value":"x","secret":true}],"cwd":null}},
      {"id":"s2","name":"remote","slug":"remote","enabled":false,"autoRestart":true,"disabledTools":[],
       "transport":{"type":"http","url":"https://example.com/mcp","headers":[]}}]}],
     "settings":{"port":4747,"callTimeoutSecs":120,"logRetentionDays":14},
     "statuses":{"s1":{"state":"running","error":null,"pid":42,"toolCount":12,"promptCount":0,"resourceCount":0,"restartAttempt":0,"serverName":"git","serverVersion":"1.0"}},
     "endpoint":{"url":"http://127.0.0.1:4747/mcp","port":4747,"running":true,"error":null,"clients":2}}
    """

    @Test func decodesStateSnapshotFromDaemon() throws {
        let state = try JSONDecoder().decode(StateView.self, from: Data(Self.stateJSON.utf8))
        #expect(state.workspaces[0].servers.count == 2)
        guard case .stdio(let stdio) = state.workspaces[0].servers[0].transport else {
            Issue.record("git must be stdio")
            return
        }
        #expect(stdio.env == [KeyValue(key: "GH_TOKEN", value: "x", secret: true)])
        #expect(state.workspaces[0].servers[1].transport.kind == "http")
        #expect(state.statuses["s1"]?.state == .running)
        #expect(state.endpoint.clients == 2)
    }

    @Test func encodesTransportWithTypeTag() throws {
        let payload = ServerPayload(
            name: "fs",
            enabled: true,
            autoRestart: false,
            transport: .stdio(StdioTransport(command: "npx", args: ["-y", "x"], env: [KeyValue(key: "A", value: "1")], cwd: nil))
        )
        let object = try JSONSerialization.jsonObject(with: JSONEncoder().encode(payload)) as! [String: Any]
        let transport = object["transport"] as! [String: Any]
        #expect(transport["type"] as? String == "stdio")
        #expect(transport["command"] as? String == "npx")
        let env = transport["env"] as! [[String: Any]]
        #expect(env[0]["key"] as? String == "A")
        #expect(env[0]["id"] == nil)
    }

    @Test func toolSchemaPrettyPrintsArbitraryJSON() throws {
        let json = #"{"name":"t","exposedName":"s__t","title":null,"description":"d","enabled":true,"inputSchema":{"type":"object","properties":{"n":{"type":"integer","minimum":1}}}}"#
        let tool = try JSONDecoder().decode(ToolView.self, from: Data(json.utf8))
        #expect(tool.inputSchema.pretty.contains("\"minimum\" : 1"))
    }
}
