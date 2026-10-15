use std::collections::{HashMap, VecDeque};
use std::process::Stdio;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use http::{HeaderName, HeaderValue};
use process_wrap::tokio::{CommandWrap, ProcessGroup};
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResponse, CallToolResult, CancelledNotificationParam,
    ClientCapabilities, ClientConfig, ClientRequest, ContentBlock, GetPromptRequestParams, GetPromptResponse,
    Implementation, Prompt, ProtocolVersion, ReadResourceRequestParams, ReadResourceResponse, Resource,
    ServerResult, Tool,
};
use rmcp::service::{NotificationContext, PeerRequestOptions, RunningService, ServiceError};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{ClientHandler, ErrorData, Peer, RoleClient, ServiceExt};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::ChildStderr;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::app::Tulma;
use crate::env::{child_env, expand_vars, resolve_command};
use crate::logs::{LogRecord, StderrRow, ToolCallRow, now_ms};
use crate::oauth::NeedsAuth;
use crate::model::{ServerConfig, ServerId, Transport};
use crate::redact::{redact_json, redact_text, truncate};

const BACKOFF_SECS: [u64; 6] = [1, 2, 4, 8, 16, 30];
const MAX_RESTARTS: usize = 5;
const STABLE_AFTER: Duration = Duration::from_secs(60);
const INIT_TIMEOUT: Duration = Duration::from_secs(120);
const STDERR_TAIL: usize = 500;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RunState {
    #[default]
    Stopped,
    Starting,
    Running,
    Error,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerStatus {
    pub state: RunState,
    pub error: Option<String>,
    pub pid: Option<u32>,
    pub tool_count: usize,
    pub prompt_count: usize,
    pub resource_count: usize,
    pub restart_attempt: u32,
    pub server_name: Option<String>,
    pub server_version: Option<String>,
    /// The server answered 401; it stays down until the user signs in.
    #[serde(default)]
    pub needs_auth: bool,
}

#[derive(Default)]
pub(crate) struct Upstream {
    pub status: ServerStatus,
    pub peer: Option<Peer<RoleClient>>,
    pub tools: Vec<Tool>,
    pub prompts: Vec<Prompt>,
    pub resources: Vec<Resource>,
    pub stop: Option<CancellationToken>,
    pub task: Option<JoinHandle<()>>,
    pub generation: u64,
    pub stderr_tail: VecDeque<String>,
}

enum Exit {
    Stopped,
    Closed,
}

#[derive(Clone, Copy)]
enum ListKind {
    Tools,
    Prompts,
    Resources,
}

/// Client side of one upstream connection; forwards list changes back into the catalog.
struct UpstreamClient {
    app: Weak<Tulma>,
    server_id: ServerId,
    generation: u64,
}

impl UpstreamClient {
    fn refresh(&self, kind: ListKind) {
        if let Some(app) = self.app.upgrade() {
            let id = self.server_id.clone();
            let generation = self.generation;
            tokio::spawn(async move { app.refresh_list(&id, generation, kind).await });
        }
    }
}

impl ClientHandler for UpstreamClient {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("tulma", env!("CARGO_PKG_VERSION")),
        )
        .with_protocol_version(ProtocolVersion::LATEST_WITH_INITIALIZE)
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.refresh(ListKind::Tools);
    }

    async fn on_prompt_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.refresh(ListKind::Prompts);
    }

    async fn on_resource_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.refresh(ListKind::Resources);
    }
}

struct Connection {
    service: RunningService<RoleClient, UpstreamClient>,
    pid: Option<u32>,
}

fn kill_group(pid: Option<u32>) {
    let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()).filter(|p| *p > 1) else {
        return;
    };
    // Grandchildren of `npx`/`uvx` share the leader's process group and outlive a plain kill.
    unsafe {
        libc::killpg(pid, libc::SIGTERM);
    }
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        unsafe {
            libc::killpg(pid, libc::SIGKILL);
        }
    });
}

impl Tulma {
    pub(crate) fn supervised(&self, id: &str) -> bool {
        self.upstreams.lock().unwrap().get(id).is_some_and(|u| u.stop.is_some())
    }

    pub(crate) fn spawn_supervisor(&self, id: &str) -> Result<()> {
        self.server_config(id).context("server not found")?;
        let mut ups = self.upstreams.lock().unwrap();
        let up = ups.entry(id.to_string()).or_default();
        if up.stop.is_some() {
            return Ok(());
        }
        up.generation += 1;
        let stop = CancellationToken::new();
        up.stop = Some(stop.clone());
        up.status = ServerStatus { state: RunState::Starting, ..Default::default() };
        let app = self.arc();
        let generation = up.generation;
        let sid = id.to_string();
        up.task = Some(tokio::spawn(async move { app.supervise(sid, generation, stop).await }));
        drop(ups);
        self.emit_status(id);
        Ok(())
    }

    /// Cancels the supervisor and waits for its child to be gone.
    pub(crate) async fn halt_server(&self, id: &str) {
        let (stop, task) = {
            let mut ups = self.upstreams.lock().unwrap();
            let Some(up) = ups.get_mut(id) else { return };
            (up.stop.take(), up.task.take())
        };
        if let Some(stop) = stop {
            stop.cancel();
        }
        if let Some(task) = task {
            let _ = tokio::time::timeout(Duration::from_secs(8), task).await;
        }
        let was_running = {
            let mut ups = self.upstreams.lock().unwrap();
            let Some(up) = ups.get_mut(id) else { return };
            let changed = up.status.state != RunState::Stopped;
            up.generation += 1;
            up.peer = None;
            up.status = ServerStatus::default();
            changed
        };
        if was_running {
            self.emit_status(id);
            self.rebuild_catalog();
        }
    }

    fn emit_status(&self, id: &str) {
        let status = self.status(id);
        self.emit("server.status", serde_json::json!({"serverId": id, "status": status}));
    }

    fn update(&self, id: &str, generation: u64, f: impl FnOnce(&mut Upstream)) -> bool {
        let mut ups = self.upstreams.lock().unwrap();
        match ups.get_mut(id) {
            Some(up) if up.generation == generation => {
                f(up);
                true
            }
            _ => false,
        }
    }

    async fn supervise(self: Arc<Self>, id: ServerId, generation: u64, stop: CancellationToken) {
        let mut attempt = 0usize;
        loop {
            if stop.is_cancelled() {
                break;
            }
            let attempt_no = attempt as u32;
            self.update(&id, generation, |u| {
                u.status = ServerStatus { state: RunState::Starting, restart_attempt: attempt_no, ..Default::default() };
            });
            self.emit_status(&id);
            let started = Instant::now();
            let (message, needs_auth) = match self.run_once(&id, generation, &stop).await {
                Ok(Exit::Stopped) => break,
                Ok(Exit::Closed) => {
                    let tail = self.stderr_last(&id);
                    let message = match tail {
                        Some(line) => format!("server exited: {line}"),
                        None => "server exited".to_string(),
                    };
                    (message, false)
                }
                Err(err) if err.downcast_ref::<NeedsAuth>().is_some() => (NeedsAuth.to_string(), true),
                Err(err) => (format!("{err:#}"), false),
            };
            if stop.is_cancelled() {
                break;
            }
            self.update(&id, generation, |u| {
                u.peer = None;
                u.status = ServerStatus {
                    state: RunState::Error,
                    error: Some(message.clone()),
                    restart_attempt: attempt_no,
                    needs_auth,
                    ..Default::default()
                };
            });
            self.emit_status(&id);
            self.rebuild_catalog();
            if needs_auth {
                // Retrying cannot help; signing in restarts the server.
                self.log_event("server.auth", Some(&id), "warn", "sign-in required".into());
                self.update(&id, generation, |u| {
                    u.stop = None;
                    u.task = None;
                });
                return;
            }
            self.log_event("server.error", Some(&id), "error", message);
            if started.elapsed() > STABLE_AFTER {
                attempt = 0;
            }
            let restartable = self.server_config(&id).is_some_and(|c| c.enabled && c.auto_restart)
                && self.is_active_server(&id);
            if !restartable || attempt >= MAX_RESTARTS {
                self.update(&id, generation, |u| {
                    u.stop = None;
                    u.task = None;
                });
                return;
            }
            let delay = BACKOFF_SECS[attempt];
            attempt += 1;
            self.log_event(
                "server.restart",
                Some(&id),
                "warn",
                format!("restarting in {delay}s (attempt {attempt}/{MAX_RESTARTS})"),
            );
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(delay)) => {}
                _ = stop.cancelled() => break,
            }
        }
        self.update(&id, generation, |u| {
            u.peer = None;
            u.status = ServerStatus::default();
        });
        self.emit_status(&id);
        self.rebuild_catalog();
    }

    fn stderr_last(&self, id: &str) -> Option<String> {
        self.upstreams.lock().unwrap().get(id)?.stderr_tail.back().cloned()
    }

    async fn run_once(&self, id: &str, generation: u64, stop: &CancellationToken) -> Result<Exit> {
        let config = self.server_config(id).context("server was deleted")?;
        let connecting = tokio::time::timeout(INIT_TIMEOUT, self.connect(&config, generation));
        let conn = tokio::select! {
            result = connecting => match result {
                Ok(Ok(conn)) => conn,
                Ok(Err(err)) => return Err(self.with_stderr_hint(id, err)),
                Err(_) => return Err(anyhow!("initialize timed out after {}s", INIT_TIMEOUT.as_secs())),
            },
            _ = stop.cancelled() => return Ok(Exit::Stopped),
        };
        let Connection { service, pid } = conn;
        let peer = service.peer().clone();
        let info = peer.peer_info();
        let caps = info.as_ref().map(|i| i.capabilities.clone()).unwrap_or_default();
        let listing = async {
            let tools = if caps.tools.is_some() { fetch(id, "tools", peer.list_all_tools()).await } else { Vec::new() };
            let prompts =
                if caps.prompts.is_some() { fetch(id, "prompts", peer.list_all_prompts()).await } else { Vec::new() };
            let resources = if caps.resources.is_some() {
                fetch(id, "resources", peer.list_all_resources()).await
            } else {
                Vec::new()
            };
            (tools, prompts, resources)
        };
        let limit = Duration::from_secs(self.settings().call_timeout_secs);
        let (tools, prompts, resources) = tokio::select! {
            listed = tokio::time::timeout(limit, listing) => match listed {
                Ok(lists) => lists,
                Err(_) => {
                    drop(service);
                    kill_group(pid);
                    return Err(anyhow!("listing tools timed out after {}s", limit.as_secs()));
                }
            },
            _ = stop.cancelled() => {
                drop(service);
                kill_group(pid);
                return Ok(Exit::Stopped);
            }
        };
        let counts = (tools.len(), prompts.len(), resources.len());
        let fresh = self.update(id, generation, |u| {
            u.peer = Some(peer.clone());
            u.status = ServerStatus {
                state: RunState::Running,
                error: None,
                pid,
                tool_count: counts.0,
                prompt_count: counts.1,
                resource_count: counts.2,
                restart_attempt: u.status.restart_attempt,
                server_name: info.as_ref().and_then(|i| i.server_info.as_ref()).map(|i| i.name.clone()),
                server_version: info.as_ref().and_then(|i| i.server_info.as_ref()).map(|i| i.version.clone()),
                needs_auth: false,
            };
            u.tools = tools.clone();
            u.prompts = prompts;
            u.resources = resources;
        });
        if fresh {
            self.remember_tools(id, &tools);
            self.emit_status(id);
            if let Ok(view) = self.server_tools(id) {
                self.emit("server.tools", &view);
            }
            self.rebuild_catalog();
            self.log_event("server.running", Some(id), "info", format!("running with {} tools", counts.0));
        }
        let ct = service.cancellation_token();
        let waiting = service.waiting();
        tokio::pin!(waiting);
        tokio::select! {
            _ = &mut waiting => {
                kill_group(pid);
                Ok(Exit::Closed)
            }
            _ = stop.cancelled() => {
                ct.cancel();
                let _ = tokio::time::timeout(Duration::from_secs(5), waiting).await;
                kill_group(pid);
                Ok(Exit::Stopped)
            }
        }
    }

    fn with_stderr_hint(&self, id: &str, err: anyhow::Error) -> anyhow::Error {
        match self.stderr_last(id) {
            Some(line) => err.context(format!("stderr: {line}")),
            None => err,
        }
    }

    async fn connect(&self, config: &ServerConfig, generation: u64) -> Result<Connection> {
        let resolved = self.store.resolve(&config.transport)?;
        let handler = UpstreamClient { app: self.me.clone(), server_id: config.id.clone(), generation };
        match resolved {
            Transport::Stdio { command, args, env, cwd } => {
                let configured_args = args.clone();
                let mut child_vars = child_env(&env).await;
                let lookup = |vars: &std::collections::BTreeMap<String, String>, k: &str| {
                    vars.get(k).cloned().or_else(|| std::env::var(k).ok())
                };
                let mut unknown = Vec::new();
                let snapshot = child_vars.clone();
                for value in child_vars.values_mut() {
                    let (expanded, missing) = expand_vars(value, |k| lookup(&snapshot, k));
                    *value = expanded;
                    unknown.extend(missing);
                }
                let args: Vec<String> = args
                    .iter()
                    .map(|a| {
                        let (expanded, missing) = expand_vars(a, |k| lookup(&child_vars, k));
                        unknown.extend(missing);
                        expanded
                    })
                    .collect();
                if !unknown.is_empty() {
                    unknown.sort();
                    unknown.dedup();
                    self.log_event(
                        "server.env",
                        Some(&config.id),
                        "warn",
                        format!("unresolved variables left as-is: {}", unknown.join(", ")),
                    );
                }
                let path = child_vars.get("PATH").cloned().unwrap_or_default();
                let program = resolve_command(&command, &path)
                    .ok_or_else(|| anyhow!("command not found: {command} (searched PATH {path})"))?;
                let mut cmd = tokio::process::Command::new(&program);
                cmd.args(&args).envs(&child_vars);
                if let Some(cwd) = cwd {
                    cmd.current_dir(expand_home(&cwd.display().to_string()));
                }
                let mut wrapped = CommandWrap::from(cmd);
                wrapped.wrap(ProcessGroup::leader());
                let (transport, stderr) = TokioChildProcess::builder(wrapped)
                    .stderr(Stdio::piped())
                    .spawn()
                    .with_context(|| format!("spawn {}", program.display()))?;
                let pid = transport.id();
                if let Some(stderr) = stderr {
                    self.pump_stderr(config.id.clone(), generation, stderr);
                }
                // Configured args, not expanded ones: `${VAR}` may hold a Keychain secret.
                self.log_event(
                    "server.spawn",
                    Some(&config.id),
                    "info",
                    redact_text(&format!("{} {}", program.display(), configured_args.join(" "))),
                );
                let service = handler.serve(transport).await.map_err(|e| anyhow!("initialize failed: {e}"))?;
                Ok(Connection { service, pid })
            }
            Transport::Http { url, headers } => {
                let env = |k: &str| std::env::var(k).ok();
                let (url, _) = expand_vars(&url, env);
                let mut map = HashMap::new();
                for (k, v) in headers {
                    let (v, _) = expand_vars(&v, env);
                    let name = HeaderName::from_bytes(k.as_bytes()).with_context(|| format!("invalid header name {k}"))?;
                    let value = HeaderValue::from_str(&v).with_context(|| format!("invalid value for header {k}"))?;
                    map.insert(name, value);
                }
                let transport_config = StreamableHttpClientTransportConfig::with_uri(url.clone())
                    .custom_headers(map)
                    .reinit_on_expired_session(true);
                let served = if config.oauth {
                    let client = self.authorized_client(&config.id, &url).await?;
                    handler.serve(StreamableHttpClientTransport::with_client(client, transport_config)).await
                } else {
                    handler.serve(StreamableHttpClientTransport::from_config(transport_config)).await
                };
                let service = served.map_err(|e| {
                    if e.is_authorization_required() {
                        anyhow::Error::new(NeedsAuth)
                    } else if url.trim_end_matches('/').ends_with("/sse") {
                        anyhow!("connect failed: {e}. Legacy SSE is not supported; wrap it with `npx mcp-remote {url}`")
                    } else {
                        anyhow!("connect failed: {e}")
                    }
                })?;
                Ok(Connection { service, pid: None })
            }
        }
    }

    fn pump_stderr(&self, id: ServerId, generation: u64, stderr: ChildStderr) {
        let app = self.me.clone();
        tokio::spawn(async move {
            // Read raw bytes: a non-UTF-8 line must not end the reader, or the child dies on EPIPE.
            let mut reader = BufReader::new(stderr);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                let Some(app) = app.upgrade() else { return };
                let text = String::from_utf8_lossy(&buf);
                let line = truncate(&redact_text(text.trim_end_matches(['\n', '\r'])), 4096);
                app.update(&id, generation, |u| {
                    if u.stderr_tail.len() >= STDERR_TAIL {
                        u.stderr_tail.pop_front();
                    }
                    u.stderr_tail.push_back(line.clone());
                });
                let row = StderrRow { id: 0, ts: now_ms(), server_id: id.clone(), line };
                app.emit("log.stderr", &row);
                app.logs.record(LogRecord::Stderr(row));
            }
        });
    }

    async fn refresh_list(&self, id: &str, generation: u64, kind: ListKind) {
        let peer = {
            let ups = self.upstreams.lock().unwrap();
            match ups.get(id) {
                Some(u) if u.generation == generation => u.peer.clone(),
                _ => None,
            }
        };
        let Some(peer) = peer else { return };
        let limit = Duration::from_secs(self.settings().call_timeout_secs);
        match kind {
            ListKind::Tools => {
                let Ok(tools) = tokio::time::timeout(limit, fetch(id, "tools", peer.list_all_tools())).await else {
                    tracing::warn!("{id}: refreshing tools timed out");
                    return;
                };
                let count = tools.len();
                if self.update(id, generation, |u| {
                    u.tools = tools.clone();
                    u.status.tool_count = count;
                }) {
                    self.remember_tools(id, &tools);
                    if let Ok(view) = self.server_tools(id) {
                        self.emit("server.tools", &view);
                    }
                }
            }
            ListKind::Prompts => {
                let Ok(prompts) = tokio::time::timeout(limit, fetch(id, "prompts", peer.list_all_prompts())).await else {
                    return;
                };
                self.update(id, generation, |u| {
                    u.status.prompt_count = prompts.len();
                    u.prompts = prompts;
                });
            }
            ListKind::Resources => {
                let Ok(resources) = tokio::time::timeout(limit, fetch(id, "resources", peer.list_all_resources())).await
                else {
                    return;
                };
                self.update(id, generation, |u| {
                    u.status.resource_count = resources.len();
                    u.resources = resources;
                });
            }
        }
        self.emit_status(id);
        self.rebuild_catalog();
    }

    fn running_peer(&self, id: &str) -> Option<Peer<RoleClient>> {
        let ups = self.upstreams.lock().unwrap();
        ups.get(id).filter(|u| u.status.state == RunState::Running)?.peer.clone()
    }

    pub async fn call_tool(
        &self,
        mut params: CallToolRequestParams,
        cancel: CancellationToken,
    ) -> Result<CallToolResponse, ErrorData> {
        let exposed = params.name.to_string();
        let catalog = self.catalog.load();
        let route = catalog
            .tool_route
            .get(&exposed)
            .cloned()
            .ok_or_else(|| ErrorData::invalid_params(format!("unknown tool: {exposed}"), None))?;
        drop(catalog);
        let server_name = self.server_config(&route.server_id).map(|c| c.name).unwrap_or_default();
        let args_json = params
            .arguments
            .as_ref()
            .map(|a| redact_json(&serde_json::Value::Object(a.clone())).to_string())
            .unwrap_or_else(|| "{}".into());
        let started = Instant::now();
        let outcome = match self.running_peer(&route.server_id) {
            None => Ok(CallToolResponse::Complete(CallToolResult::error(vec![ContentBlock::text(format!(
                "server {server_name} is not running"
            ))]))),
            Some(peer) => {
                params.name = route.name.clone().into();
                params.meta = None;
                let timeout = Duration::from_secs(self.settings().call_timeout_secs);
                forward_call(&peer, params, timeout, cancel).await
            }
        };
        let duration_ms = started.elapsed().as_millis() as i64;
        let (ok, error, preview) = match &outcome {
            Ok(CallToolResponse::Complete(r)) => {
                let text = serde_json::to_string(&r.content).unwrap_or_default();
                let failed = r.is_error == Some(true);
                (!failed, failed.then(|| truncate(&text, 512)), text)
            }
            Ok(_) => (true, None, "(input required / task)".to_string()),
            Err(e) => (false, Some(e.message.to_string()), String::new()),
        };
        let row = ToolCallRow {
            id: 0,
            ts: now_ms(),
            workspace_id: Some(self.state.lock().unwrap().active_workspace.clone()),
            server_id: route.server_id.clone(),
            server_name,
            tool: route.name,
            exposed_tool: exposed,
            duration_ms,
            ok,
            error: error.map(|e| redact_text(&e)),
            args_json,
            result_preview: redact_text(&truncate(&preview, crate::logs::RESULT_LIMIT)),
        };
        self.emit("log.toolCall", &row);
        self.logs.record(LogRecord::ToolCall(row));
        outcome
    }

    pub async fn get_prompt(&self, mut params: GetPromptRequestParams) -> Result<GetPromptResponse, ErrorData> {
        let route = self
            .catalog
            .load()
            .prompt_route
            .get(&params.name)
            .cloned()
            .ok_or_else(|| ErrorData::invalid_params(format!("unknown prompt: {}", params.name), None))?;
        let peer = self
            .running_peer(&route.server_id)
            .ok_or_else(|| ErrorData::internal_error("server is not running", None))?;
        params.name = route.name;
        params.meta = None;
        peer.get_prompt_once(params).await.map_err(service_error)
    }

    pub async fn read_resource(&self, mut params: ReadResourceRequestParams) -> Result<ReadResourceResponse, ErrorData> {
        let server_id = self
            .catalog
            .load()
            .resource_route
            .get(&params.uri)
            .cloned()
            .ok_or_else(|| ErrorData::resource_not_found(format!("unknown resource: {}", params.uri), None))?;
        let peer = self
            .running_peer(&server_id)
            .ok_or_else(|| ErrorData::internal_error("server is not running", None))?;
        params.meta = None;
        peer.read_resource_once(params).await.map_err(service_error)
    }
}

async fn forward_call(
    peer: &Peer<RoleClient>,
    params: CallToolRequestParams,
    timeout: Duration,
    cancel: CancellationToken,
) -> Result<CallToolResponse, ErrorData> {
    let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));
    let handle = peer
        .send_request_with_option(request, PeerRequestOptions::with_timeout(timeout))
        .await
        .map_err(service_error)?;
    let request_id = handle.id.clone();
    let response = tokio::select! {
        r = handle.await_response() => r,
        _ = cancel.cancelled() => {
            let _ = peer
                .notify_cancelled(CancelledNotificationParam::new(
                    Some(request_id),
                    Some("cancelled by client".into()),
                ))
                .await;
            return Err(ErrorData::internal_error("cancelled by client", None));
        }
    };
    match response {
        Ok(ServerResult::CallToolResult(r)) => Ok(CallToolResponse::Complete(r)),
        Ok(ServerResult::InputRequiredResult(r)) => Ok(CallToolResponse::InputRequired(r)),
        Ok(ServerResult::CreateTaskResult(r)) => Ok(CallToolResponse::Task(r)),
        Ok(_) => Err(ErrorData::internal_error("unexpected response from upstream", None)),
        Err(ServiceError::Timeout { timeout }) => Ok(CallToolResponse::Complete(CallToolResult::error(vec![
            ContentBlock::text(format!("tool call timed out after {}s", timeout.as_secs())),
        ]))),
        Err(err) => Err(service_error(err)),
    }
}

fn service_error(err: ServiceError) -> ErrorData {
    match err {
        ServiceError::McpError(e) => e,
        other => ErrorData::internal_error(other.to_string(), None),
    }
}

async fn fetch<T>(id: &str, what: &str, fut: impl Future<Output = Result<Vec<T>, ServiceError>>) -> Vec<T> {
    match fut.await {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!("{id}: listing {what} failed: {err}");
            Vec::new()
        }
    }
}

fn expand_home(path: &str) -> String {
    match (path.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest).display().to_string(),
        _ => path.to_string(),
    }
}
