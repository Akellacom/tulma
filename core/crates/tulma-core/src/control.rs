//! NDJSON JSON-RPC 2.0 between the app and the daemon; see docs/control-protocol.md.

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc};

use crate::app::{Tulma, ServerInput, SettingsPatch, TransportView};
use crate::import::DuplicatePolicy;
use crate::logs::LogQuery;
use crate::registry::{self, InstallOption, Source};

const PARSE_ERROR: i64 = -32700;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const APP_ERROR: i64 = -32000;

struct RpcError {
    code: i64,
    message: String,
}

impl From<anyhow::Error> for RpcError {
    fn from(err: anyhow::Error) -> Self {
        RpcError { code: APP_ERROR, message: format!("{err:#}") }
    }
}

type RpcResult = std::result::Result<Value, RpcError>;

#[derive(Deserialize)]
struct Request {
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerPayload {
    name: String,
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default = "yes")]
    auto_restart: bool,
    transport: TransportView,
}

fn yes() -> bool {
    true
}

impl From<ServerPayload> for ServerInput {
    fn from(p: ServerPayload) -> Self {
        ServerInput {
            name: p.name,
            enabled: p.enabled,
            auto_restart: p.auto_restart,
            secret_keys: p.transport.secret_keys(),
            transport: p.transport.into(),
        }
    }
}

fn params<T: DeserializeOwned>(value: Value) -> std::result::Result<T, RpcError> {
    let value = if value.is_null() { json!({}) } else { value };
    serde_json::from_value(value).map_err(|e| RpcError { code: INVALID_PARAMS, message: e.to_string() })
}

fn ok(value: impl serde::Serialize) -> RpcResult {
    serde_json::to_value(value).map_err(|e| RpcError { code: APP_ERROR, message: e.to_string() })
}

pub async fn serve<R, W>(app: Arc<Tulma>, input: R, output: W) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        let mut output = output;
        while let Some(line) = rx.recv().await {
            if output.write_all(line.as_bytes()).await.is_err() || output.write_all(b"\n").await.is_err() {
                break;
            }
            if output.flush().await.is_err() {
                break;
            }
        }
    });

    let forward_tx = tx.clone();
    let mut events = app.subscribe();
    let forward_app = app.clone();
    let forwarder = tokio::spawn(async move {
        loop {
            let note = match events.recv().await {
                Ok(note) => note,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    crate::events::Notification::new("state.changed", forward_app.snapshot())
                }
                Err(broadcast::error::RecvError::Closed) => return,
            };
            let line = json!({"jsonrpc": "2.0", "method": note.method, "params": note.params}).to_string();
            if forward_tx.send(line).is_err() {
                return;
            }
        }
    });

    let mut lines = BufReader::new(input).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let request: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(error_line(Value::Null, PARSE_ERROR, &e.to_string()));
                continue;
            }
        };
        let app = app.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let outcome = dispatch(&app, &request.method, request.params).await;
            let Some(id) = request.id else { return };
            let line = match outcome {
                Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string(),
                Err(e) => error_line(id, e.code, &e.message),
            };
            let _ = tx.send(line);
        });
    }
    forwarder.abort();
    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), writer).await;
    Ok(())
}

fn error_line(id: Value, code: i64, message: &str) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Id {
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IdName {
    id: String,
    name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Name {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddServer {
    workspace_id: String,
    server: ServerPayload,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateServer {
    id: String,
    server: ServerPayload,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Enabled {
    id: String,
    enabled: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolToggle {
    id: String,
    tool: String,
    enabled: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExportWorkspace {
    id: String,
    #[serde(default)]
    include_secrets: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImportText {
    workspace_id: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImportApply {
    workspace_id: String,
    text: String,
    names: Option<Vec<String>>,
    #[serde(default = "skip")]
    duplicate_policy: DuplicatePolicy,
}

fn skip() -> DuplicatePolicy {
    DuplicatePolicy::Skip
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerRef {
    server_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LogsQuery {
    #[serde(default = "events_table")]
    table: String,
    #[serde(flatten)]
    query: LogQuery,
}

fn events_table() -> String {
    "events".into()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StderrQuery {
    server_id: String,
    limit: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogSearch {
    source: Source,
    #[serde(default)]
    query: String,
    cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogRender {
    option: InstallOption,
    #[serde(default)]
    values: std::collections::BTreeMap<String, String>,
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f).await?
}

async fn dispatch(app: &Arc<Tulma>, method: &str, raw: Value) -> RpcResult {
    match method {
        "ping" => ok("pong"),
        "state.get" => ok(app.snapshot()),
        "workspace.create" => {
            let p: Name = params(raw)?;
            ok(json!({"id": app.create_workspace(&p.name).await?.id}))
        }
        "workspace.rename" => {
            let p: IdName = params(raw)?;
            let name = p.name.ok_or(RpcError { code: INVALID_PARAMS, message: "missing name".into() })?;
            app.rename_workspace(&p.id, &name).await?;
            ok(Value::Null)
        }
        "workspace.delete" => {
            let p: Id = params(raw)?;
            app.delete_workspace(&p.id).await?;
            ok(Value::Null)
        }
        "workspace.duplicate" => {
            let p: IdName = params(raw)?;
            ok(json!({"id": app.duplicate_workspace(&p.id, p.name.as_deref()).await?.id}))
        }
        "workspace.activate" => {
            let p: Id = params(raw)?;
            app.activate_workspace(&p.id).await?;
            ok(Value::Null)
        }
        "workspace.export" => {
            let p: ExportWorkspace = params(raw)?;
            ok(json!({"text": app.export_workspace(&p.id, p.include_secrets)?}))
        }
        "server.add" => {
            let p: AddServer = params(raw)?;
            ok(json!({"id": app.add_server(&p.workspace_id, p.server.into()).await?.id}))
        }
        "server.update" => {
            let p: UpdateServer = params(raw)?;
            ok(json!({"id": app.update_server(&p.id, p.server.into()).await?.id}))
        }
        "server.delete" => {
            let p: Id = params(raw)?;
            app.delete_server(&p.id).await?;
            ok(Value::Null)
        }
        "server.setEnabled" => {
            let p: Enabled = params(raw)?;
            app.set_server_enabled(&p.id, p.enabled).await?;
            ok(Value::Null)
        }
        "server.start" => {
            let p: Id = params(raw)?;
            app.start_server(&p.id).await?;
            ok(Value::Null)
        }
        "server.stop" => {
            let p: Id = params(raw)?;
            app.stop_server(&p.id).await?;
            ok(Value::Null)
        }
        "server.restart" => {
            let p: Id = params(raw)?;
            app.restart_server(&p.id).await?;
            ok(Value::Null)
        }
        "server.setToolEnabled" => {
            let p: ToolToggle = params(raw)?;
            app.set_tool_enabled(&p.id, &p.tool, p.enabled).await?;
            ok(Value::Null)
        }
        "server.authorize" => {
            let p: Id = params(raw)?;
            ok(json!({"authUrl": app.authorize(&p.id).await?}))
        }
        "server.signOut" => {
            let p: Id = params(raw)?;
            app.sign_out(&p.id).await?;
            ok(Value::Null)
        }
        "catalog.search" => {
            let p: CatalogSearch = params(raw)?;
            ok(app.registry.search(p.source, &p.query, p.cursor.as_deref()).await?)
        }
        "catalog.render" => {
            let p: CatalogRender = params(raw)?;
            ok(registry::render(&p.option, &p.values))
        }
        "import.preview" => {
            let p: ImportText = params(raw)?;
            ok(app.import_preview(&p.workspace_id, &p.text)?)
        }
        "import.apply" => {
            let p: ImportApply = params(raw)?;
            ok(app.import_apply(&p.workspace_id, &p.text, p.names, p.duplicate_policy).await?)
        }
        "tools.list" => {
            let p: ServerRef = params(raw)?;
            ok(app.server_tools(&p.server_id)?)
        }
        "logs.query" => {
            let p: LogsQuery = params(raw)?;
            let app = app.clone();
            match p.table.as_str() {
                "events" => ok(blocking(move || app.logs.events(&p.query)).await?),
                "toolCalls" => ok(blocking(move || app.logs.tool_calls(&p.query)).await?),
                other => Err(RpcError { code: INVALID_PARAMS, message: format!("unknown table {other}") }),
            }
        }
        "logs.stderr" => {
            let p: StderrQuery = params(raw)?;
            let app = app.clone();
            ok(blocking(move || app.logs.stderr(&p.server_id, p.limit.unwrap_or(500))).await?)
        }
        "settings.get" => ok(app.settings()),
        "settings.update" => {
            let p: SettingsPatch = params(raw)?;
            ok(app.update_settings(p).await?)
        }
        "endpoint.info" => ok(app.endpoint()),
        "endpoint.restart" => ok(app.restart_endpoint().await?),
        other => Err(RpcError { code: METHOD_NOT_FOUND, message: format!("unknown method {other}") }),
    }
}
