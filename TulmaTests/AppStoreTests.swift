import Foundation
import Testing

@MainActor
struct AppStoreTests {
    private func note(_ method: String, _ params: String) -> Data {
        Data(#"{"jsonrpc":"2.0","method":"\#(method)","params":\#(params)}"#.utf8)
    }

    @Test func stateSnapshotSelectsActiveWorkspace() {
        let store = AppStore()
        store.apply(method: "state.changed", payload: note("state.changed", ProtocolTests.stateJSON))
        #expect(store.sidebar == .workspace("w1"))
        #expect(store.runningCount == 1)
        #expect(store.enabledCount == 1)
        #expect(store.endpointURL == "http://127.0.0.1:4747/mcp")
    }

    @Test func statusNotificationUpdatesOnlyThatServer() {
        let store = AppStore()
        store.apply(method: "state.changed", payload: note("state.changed", ProtocolTests.stateJSON))
        store.apply(method: "server.status", payload: note("server.status", #"{"serverId":"s1","status":{"state":"error","error":"boom","pid":null,"toolCount":0,"promptCount":0,"resourceCount":0,"restartAttempt":2}}"#))
        #expect(store.status("s1").state == .error)
        #expect(store.status("s1").error == "boom")
        #expect(store.hasErrors)
        #expect(store.status("s2").state == .stopped)
    }

    @Test func liveRowsGetUniqueNegativeIDs() {
        let store = AppStore()
        let call = #"{"id":0,"ts":1,"workspaceId":"w1","serverId":"s1","serverName":"git","tool":"log","exposedTool":"git__log","durationMs":3,"ok":true,"error":null,"argsJson":"{}","resultPreview":"[]"}"#
        store.apply(method: "log.toolCall", payload: note("log.toolCall", call))
        store.apply(method: "log.toolCall", payload: note("log.toolCall", call))
        #expect(store.liveCalls.map(\.id) == [-2, -1])
    }

    @Test func stderrIsCappedPerServer() {
        let store = AppStore()
        for i in 0..<(AppStore.stderrLimit + 5) {
            store.apply(method: "log.stderr", payload: note("log.stderr", #"{"id":0,"ts":\#(i),"serverId":"s1","line":"l\#(i)"}"#))
        }
        #expect(store.stderr["s1"]?.count == AppStore.stderrLimit)
        #expect(store.stderr["s1"]?.last?.line == "l\(AppStore.stderrLimit + 4)")
    }

    @Test func switchingWorkspaceClearsForeignServerSelection() {
        let store = AppStore()
        store.apply(method: "state.changed", payload: note("state.changed", ProtocolTests.stateJSON))
        store.selectedServer = "s1"
        store.sidebar = .logs
        #expect(store.selectedServer == "s1")
        store.sidebar = .workspace("w1")
        #expect(store.selectedServer == "s1")
        store.sidebar = .workspace("w-other")
        #expect(store.selectedServer == nil)
    }

    @Test func switchingLogTableClearsLogSelection() {
        let store = AppStore()
        let call = ToolCallRow(id: 1, ts: 1, workspaceId: nil, serverId: "s1", serverName: "git", tool: "t", exposedTool: "git__t",
                               durationMs: 1, ok: true, error: nil, argsJson: "{}", resultPreview: "")
        store.selectedLog = .call(call)
        store.logTable = .events
        #expect(store.selectedLog == nil)
    }
}
