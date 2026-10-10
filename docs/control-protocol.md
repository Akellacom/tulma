# Control protocol

`Tulma.app` talks to `tulma daemon` over the daemon's stdin/stdout. It is JSON-RPC 2.0, one message per line (NDJSON). Daemon logs go to stderr. When stdin closes, the daemon stops every child server and exits.

```
→ {"jsonrpc":"2.0","id":1,"method":"state.get","params":{}}
← {"jsonrpc":"2.0","id":1,"result":{...}}
← {"jsonrpc":"2.0","method":"server.status","params":{...}}
```

Requests are handled concurrently, so responses can come back out of order. Match them by `id`.

## Errors

| code | meaning |
|------|---------|
| -32700 | the line is not valid JSON |
| -32601 | unknown method |
| -32602 | params do not match the method's shape |
| -32000 | the operation failed; `message` explains why (for example `workspace not found`) |

## Types

```ts
type KeyValue = { key: string; value: string; secret: boolean }   // secret: stored in Keychain
type TransportView =
  | { type: "stdio"; command: string; args: string[]; env: KeyValue[]; cwd?: string }
  | { type: "http"; url: string; headers: KeyValue[] }
type ServerPayload = { name: string; enabled?: boolean; autoRestart?: boolean; transport: TransportView }
type ServerView = { id; name; slug; enabled; autoRestart; transport: TransportView; disabledTools: string[] }
type WorkspaceView = { id; name; slug; servers: ServerView[] }
type Settings = { port: number; callTimeoutSecs: number; logRetentionDays: number }
type RunState = "stopped" | "starting" | "running" | "error"
type ServerStatus = { state: RunState; error?: string; pid?: number; toolCount; promptCount; resourceCount;
                      restartAttempt; serverName?: string; serverVersion?: string }
type EndpointInfo = { url?: string; port: number; running: boolean; error?: string; clients: number }
type StateView = { activeWorkspace: string; workspaces: WorkspaceView[]; settings: Settings;
                   statuses: { [serverId]: ServerStatus }; endpoint: EndpointInfo }
type ToolView = { name; exposedName; title?; description?; inputSchema: object; enabled: boolean }
type ToolsView = { serverId; live: boolean; tools: ToolView[] }        // live=false: last known list
```

On input, secret values are sent in plain text. The daemon moves any non-empty value whose key matches `token|secret|key|password|auth` into the Keychain (service `app.tulma`). `config.json` keeps only a `keychain:<uuid>` reference. Views always carry the resolved value.

## Requests

| method | params | result |
|--------|--------|--------|
| `ping` | — | `"pong"` |
| `state.get` | — | `StateView` |
| `workspace.create` | `{name}` | `{id}` |
| `workspace.rename` | `{id, name}` | `null` |
| `workspace.delete` | `{id}` | `null` (the last workspace cannot be deleted; deleting the active one activates another) |
| `workspace.duplicate` | `{id, name?}` | `{id}` (servers are copied with new ids) |
| `workspace.activate` | `{id}` | `null` (stops the old servers, starts the new ones, sends `tools/list_changed` to clients) |
| `workspace.export` | `{id, includeSecrets?: boolean}` | `{text}`: an `mcpServers` JSON that `import.*`, Claude Desktop and Cursor read. Without `includeSecrets`, sensitive env/header values are empty and tokens in urls and args are masked |
| `server.add` | `{workspaceId, server: ServerPayload}` | `{id}` |
| `server.update` | `{id, server: ServerPayload}` | `{id}` (restarts the server if its transport or name changed) |
| `server.delete` | `{id}` | `null` |
| `server.setEnabled` | `{id, enabled}` | `null` |
| `server.start` / `server.stop` / `server.restart` | `{id}` | `null` (a manual start resets the restart counter) |
| `server.setToolEnabled` | `{id, tool, enabled}` | `null` (`tool` is the server's original tool name) |
| `import.preview` | `{workspaceId, text}` | `{items: [{name, transport?, enabled, status: {kind: "new"\|"duplicate"\|"invalid", reason?}, warning?}]}` |
| `import.apply` | `{workspaceId, text, names?: string[], duplicatePolicy: "skip"\|"replace"}` | `{added: id[], replaced: id[], skipped: name[]}` |
| `tools.list` | `{serverId}` | `ToolsView` |
| `logs.query` | `{table: "events"\|"toolCalls", serverId?, kind?, status?: "ok"\|"error", text?, limit?, before?: {ts, id}}` | rows, newest first |
| `logs.stderr` | `{serverId, limit?}` | `[{id, ts, serverId, line}]`, oldest first |
| `settings.get` | — | `Settings` |
| `settings.update` | `{port?, callTimeoutSecs?, logRetentionDays?}` | `Settings` (a new port restarts the endpoint, even one that failed to start; if the new port cannot be bound, the settings are still saved and the call fails with the bind error) |
| `endpoint.info` | — | `EndpointInfo` |
| `endpoint.restart` | — | `EndpointInfo`, after trying to bind the configured port again (fails with -32000 if it is still taken) |

`import.apply` parses `text` again, so the daemon keeps no preview state between the two calls.

Event row: `{id, ts, kind, workspaceId?, serverId?, level, message}`. Tool call row: `{id, ts, workspaceId?, serverId, serverName, tool, exposedTool, durationMs, ok, error?, argsJson, resultPreview}`. `ts` is Unix time in milliseconds. Arguments are redacted and capped at 4 KB; result previews are capped at 2 KB.

## Notifications (daemon → app)

| method | params |
|--------|--------|
| `state.changed` | `StateView`, sent after any workspace, server or settings change |
| `server.status` | `{serverId, status: ServerStatus}` |
| `server.tools` | `ToolsView` |
| `catalog.changed` | `{tools, prompts, resources}`, the counts exposed on the endpoint |
| `log.event` | event row (`id` is 0) |
| `log.toolCall` | tool call row (`id` is 0) |
| `log.stderr` | `{id: 0, ts, serverId, line}` |
| `endpoint.changed` | `EndpointInfo` |
| `endpoint.error` | `{port, message}`, for example when the port is taken (the daemon does not move to another port) |

If the app falls behind the notification buffer, the daemon sends a fresh `state.changed` in place of the messages it dropped.

## Running it by hand

```sh
TULMA_LOG=debug core/target/debug/tulma daemon --data-dir /tmp/mf --memory-secrets
{"jsonrpc":"2.0","id":1,"method":"state.get"}
```
