use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, Weak};

use anyhow::{Context, Result, anyhow, bail};
use arc_swap::ArcSwap;
use rmcp::model::Tool;
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, broadcast};
use tokio::task::JoinHandle;

use crate::catalog::{Catalog, CatalogInput, Fingerprints, exposed_name};
use crate::events::Notification;
use crate::export;
use crate::gateway::SessionRegistry;
use crate::http::HttpHandle;
use crate::import::{self, DuplicatePolicy, ImportAction, ImportPreview};
use crate::logs::{EventRow, LogRecord, Logs, now_ms};
use crate::model::{ServerConfig, ServerId, Settings, State, Transport, Workspace, unique_slug};
use crate::redact::is_sensitive_key;
use crate::store::{Paths, SECRET_PREFIX, SecretStore, Store, write_atomic};
use crate::upstream::{RunState, ServerStatus, Upstream};

pub const LIST_TOOLS: u8 = 1;
pub const LIST_PROMPTS: u8 = 2;
pub const LIST_RESOURCES: u8 = 4;

pub struct Tulma {
    pub(crate) me: Weak<Tulma>,
    pub(crate) state: Mutex<State>,
    pub(crate) store: Store,
    pub logs: Logs,
    pub(crate) events: broadcast::Sender<Notification>,
    pub(crate) upstreams: Mutex<HashMap<ServerId, Upstream>>,
    pub(crate) catalog: ArcSwap<Catalog>,
    pub(crate) sessions: SessionRegistry,
    pub(crate) tool_cache: Mutex<HashMap<ServerId, Vec<Tool>>>,
    pub(crate) http: tokio::sync::Mutex<Option<HttpHandle>>,
    pub(crate) endpoint: Mutex<EndpointInfo>,
    pub(crate) ops: tokio::sync::Mutex<()>,
    pub(crate) sign_ins: Mutex<HashMap<ServerId, JoinHandle<()>>>,
    pub registry: crate::registry::Registry,
    pending_lists: AtomicU8,
    lists_dirty: Notify,
    started: AtomicBool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointInfo {
    pub url: Option<String>,
    pub port: u16,
    pub running: bool,
    pub error: Option<String>,
    pub clients: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInput {
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "yes")]
    pub auto_restart: bool,
    pub transport: Transport,
    /// Env/header keys to keep in the secret store even if their name does not look secret.
    #[serde(default)]
    pub secret_keys: BTreeSet<String>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SettingsPatch {
    pub port: Option<u16>,
    pub call_timeout_secs: Option<u64>,
    pub log_retention_days: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyValue {
    pub key: String,
    pub value: String,
    #[serde(default)]
    pub secret: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum TransportView {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Vec<KeyValue>,
        #[serde(default)]
        cwd: Option<String>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: Vec<KeyValue>,
    },
}

impl TransportView {
    pub fn secret_keys(&self) -> BTreeSet<String> {
        let pairs = match self {
            TransportView::Stdio { env, .. } => env,
            TransportView::Http { headers, .. } => headers,
        };
        pairs.iter().filter(|kv| kv.secret).map(|kv| kv.key.trim().to_string()).collect()
    }
}

impl From<TransportView> for Transport {
    fn from(view: TransportView) -> Self {
        let map = |pairs: Vec<KeyValue>| -> BTreeMap<String, String> {
            pairs
                .into_iter()
                .filter(|kv| !kv.key.trim().is_empty())
                .map(|kv| (kv.key.trim().to_string(), kv.value))
                .collect()
        };
        match view {
            TransportView::Stdio { command, args, env, cwd } => Transport::Stdio {
                command: command.trim().to_string(),
                args,
                env: map(env),
                cwd: cwd.filter(|c| !c.trim().is_empty()).map(PathBuf::from),
            },
            TransportView::Http { url, headers } => Transport::Http {
                url: url.trim().to_string(),
                headers: map(headers),
            },
        }
    }
}

fn view_pairs(resolved: &BTreeMap<String, String>, stored: &BTreeMap<String, String>) -> Vec<KeyValue> {
    resolved
        .iter()
        .map(|(k, v)| KeyValue {
            key: k.clone(),
            value: v.clone(),
            secret: is_sensitive_key(k) || stored.get(k).is_some_and(|s| s.starts_with(SECRET_PREFIX)),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerView {
    pub id: ServerId,
    pub name: String,
    pub slug: String,
    pub enabled: bool,
    pub auto_restart: bool,
    pub transport: TransportView,
    pub disabled_tools: Vec<String>,
    pub oauth: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceView {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub servers: Vec<ServerView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateView {
    pub active_workspace: String,
    pub workspaces: Vec<WorkspaceView>,
    pub settings: Settings,
    pub statuses: HashMap<ServerId, ServerStatus>,
    pub endpoint: EndpointInfo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolView {
    pub name: String,
    pub exposed_name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub input_schema: serde_json::Value,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsView {
    pub server_id: ServerId,
    pub live: bool,
    pub tools: Vec<ToolView>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    pub added: Vec<ServerId>,
    pub replaced: Vec<ServerId>,
    pub skipped: Vec<String>,
}

impl Tulma {
    pub fn open(paths: Paths, secrets: Arc<dyn SecretStore>) -> Result<Arc<Self>> {
        paths.ensure()?;
        let store = Store::new(paths, secrets);
        let mut state = store.load()?;
        if state.workspaces.is_empty() {
            state = State::default();
        }
        if state.active().is_none() {
            state.active_workspace = state.workspaces[0].id.clone();
        }
        let logs = Logs::open(&store.paths.logs_db(), state.settings.log_retention_days)?;
        let tool_cache = std::fs::read(store.paths.tool_cache())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let (events, _) = broadcast::channel(4096);
        let port = state.settings.port;
        Ok(Arc::new_cyclic(|me| Tulma {
            me: me.clone(),
            state: Mutex::new(state),
            store,
            logs,
            events,
            upstreams: Mutex::new(HashMap::new()),
            catalog: ArcSwap::from_pointee(Catalog::default()),
            sessions: SessionRegistry::default(),
            tool_cache: Mutex::new(tool_cache),
            http: tokio::sync::Mutex::new(None),
            endpoint: Mutex::new(EndpointInfo { port, ..Default::default() }),
            ops: tokio::sync::Mutex::new(()),
            sign_ins: Mutex::new(HashMap::new()),
            registry: crate::registry::Registry::new(),
            pending_lists: AtomicU8::new(0),
            lists_dirty: Notify::new(),
            started: AtomicBool::new(false),
        }))
    }

    pub(crate) fn arc(&self) -> Arc<Tulma> {
        self.me.upgrade().expect("Tulma is always owned by an Arc")
    }

    /// Boots the endpoint and every enabled server of the active workspace.
    pub async fn start(&self) {
        self.started.store(true, Ordering::SeqCst);
        let me = self.arc();
        tokio::spawn(async move { me.list_change_notifier().await });
        tokio::spawn(async { crate::env::login_shell_path().await });
        if let Err(err) = self.restart_http().await {
            tracing::warn!("endpoint failed to start: {err:#}");
        }
        let ids: Vec<ServerId> = {
            let state = self.state.lock().unwrap();
            state
                .active()
                .map(|w| w.servers.iter().filter(|s| s.enabled).map(|s| s.id.clone()).collect())
                .unwrap_or_default()
        };
        for id in ids {
            if let Err(err) = self.spawn_supervisor(&id) {
                tracing::warn!("start {id}: {err:#}");
            }
        }
        self.log_event("daemon.started", None, "info", "Tulma started".into());
    }

    pub async fn shutdown(&self) {
        let _guard = self.ops.lock().await;
        let ids: Vec<ServerId> = self.upstreams.lock().unwrap().keys().cloned().collect();
        futures::future::join_all(ids.iter().map(|id| self.halt_server(id))).await;
        if let Some(handle) = self.http.lock().await.take() {
            handle.stop().await;
        }
        self.logs.flush();
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Notification> {
        self.events.subscribe()
    }

    pub(crate) fn emit(&self, method: &str, params: impl Serialize) {
        let _ = self.events.send(Notification::new(method, params));
    }

    pub(crate) fn log_event(&self, kind: &str, server_id: Option<&str>, level: &str, message: String) {
        let workspace_id = Some(self.state.lock().unwrap().active_workspace.clone());
        let row = EventRow {
            id: 0,
            ts: now_ms(),
            kind: kind.into(),
            workspace_id,
            server_id: server_id.map(str::to_string),
            level: level.into(),
            message,
        };
        self.emit("log.event", &row);
        self.logs.record(LogRecord::Event(row));
    }

    pub fn state(&self) -> State {
        self.state.lock().unwrap().clone()
    }

    pub fn status(&self, id: &str) -> ServerStatus {
        self.upstreams
            .lock()
            .unwrap()
            .get(id)
            .map(|u| u.status.clone())
            .unwrap_or_default()
    }

    pub fn endpoint(&self) -> EndpointInfo {
        let mut info = self.endpoint.lock().unwrap().clone();
        info.clients = self.sessions.count();
        info
    }

    pub fn snapshot(&self) -> StateView {
        let state = self.state();
        let statuses = self
            .upstreams
            .lock()
            .unwrap()
            .iter()
            .map(|(id, u)| (id.clone(), u.status.clone()))
            .collect();
        let workspaces = state
            .workspaces
            .iter()
            .map(|w| WorkspaceView {
                id: w.id.clone(),
                name: w.name.clone(),
                slug: w.slug.clone(),
                servers: w.servers.iter().map(|s| self.server_view(s)).collect(),
            })
            .collect();
        StateView {
            active_workspace: state.active_workspace,
            workspaces,
            settings: state.settings,
            statuses,
            endpoint: self.endpoint(),
        }
    }

    fn server_view(&self, s: &ServerConfig) -> ServerView {
        let resolved = self.store.resolve(&s.transport).unwrap_or_else(|err| {
            tracing::warn!("resolve secrets for {}: {err:#}", s.name);
            s.transport.clone()
        });
        let transport = match (&resolved, &s.transport) {
            (Transport::Stdio { command, args, env, cwd }, stored) => TransportView::Stdio {
                command: command.clone(),
                args: args.clone(),
                env: view_pairs(env, stored.secrets()),
                cwd: cwd.as_ref().map(|c| c.display().to_string()),
            },
            (Transport::Http { url, headers }, stored) => TransportView::Http {
                url: url.clone(),
                headers: view_pairs(headers, stored.secrets()),
            },
        };
        ServerView {
            id: s.id.clone(),
            name: s.name.clone(),
            slug: s.slug.clone(),
            enabled: s.enabled,
            auto_restart: s.auto_restart,
            transport,
            disabled_tools: s.disabled_tools.iter().cloned().collect(),
            oauth: s.oauth,
        }
    }

    pub(crate) fn emit_state(&self) {
        self.emit("state.changed", self.snapshot());
    }

    fn save(&self, state: &State) -> Result<()> {
        self.store.save(state)
    }

    /// Applies `f` to the state and persists it; the closure's error leaves state untouched.
    pub(crate) fn mutate<T>(&self, f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        let mut state = self.state.lock().unwrap();
        let mut draft = state.clone();
        let out = f(&mut draft)?;
        self.save(&draft)?;
        *state = draft;
        Ok(out)
    }

    pub fn server_config(&self, id: &str) -> Option<ServerConfig> {
        self.state.lock().unwrap().find_server(id).map(|(_, s)| s.clone())
    }

    pub(crate) fn is_active_server(&self, id: &str) -> bool {
        self.state.lock().unwrap().is_active_server(id)
    }

    // Workspaces

    pub async fn create_workspace(&self, name: &str) -> Result<Workspace> {
        let _guard = self.ops.lock().await;
        let ws = self.mutate(|s| {
            let ws = Workspace::new(non_empty(name, "Workspace")?, &s.workspace_slugs());
            s.workspaces.push(ws.clone());
            Ok(ws)
        })?;
        self.emit_state();
        Ok(ws)
    }

    pub async fn rename_workspace(&self, id: &str, name: &str) -> Result<()> {
        let _guard = self.ops.lock().await;
        self.mutate(|s| {
            let name = non_empty(name, "")?;
            s.workspace_mut(id).context("workspace not found")?.name = name.to_string();
            Ok(())
        })?;
        self.emit_state();
        Ok(())
    }

    pub async fn duplicate_workspace(&self, id: &str, name: Option<&str>) -> Result<Workspace> {
        let _guard = self.ops.lock().await;
        let copy = self.mutate(|s| {
            let source = s.workspace(id).context("workspace not found")?;
            let name = name.map(str::to_string).unwrap_or_else(|| format!("{} copy", source.name));
            let mut copy = source.duplicate(&name, &s.workspace_slugs());
            for (server, original) in copy.servers.iter_mut().zip(&source.servers) {
                server.transport = self.store.resolve(&original.transport)?;
                self.store.externalize(&mut server.transport, None, &Store::secret_keys(&original.transport))?;
                // Tokens are stored per server id, so the copy signs in on its own.
                server.oauth = false;
            }
            s.workspaces.push(copy.clone());
            Ok(copy)
        })?;
        self.emit_state();
        Ok(copy)
    }

    pub async fn delete_workspace(&self, id: &str) -> Result<()> {
        let _guard = self.ops.lock().await;
        let (was_active, servers) = {
            let state = self.state.lock().unwrap();
            if state.workspaces.len() <= 1 {
                bail!("cannot delete the last workspace");
            }
            let ws = state.workspace(id).context("workspace not found")?;
            (state.active_workspace == id, ws.servers.clone())
        };
        if was_active {
            let next = self
                .state
                .lock()
                .unwrap()
                .workspaces
                .iter()
                .find(|w| w.id != id)
                .map(|w| w.id.clone())
                .expect("at least two workspaces");
            self.activate_locked(&next).await?;
        }
        self.mutate(|s| {
            s.workspaces.retain(|w| w.id != id);
            Ok(())
        })?;
        for server in &servers {
            self.store.forget(&server.transport);
            self.forget_sign_in(&server.id);
            self.forget_runtime(&server.id);
        }
        self.emit_state();
        Ok(())
    }

    pub async fn activate_workspace(&self, id: &str) -> Result<()> {
        let _guard = self.ops.lock().await;
        self.activate_locked(id).await
    }

    async fn activate_locked(&self, id: &str) -> Result<()> {
        let to_start: Vec<ServerId> = {
            let state = self.state.lock().unwrap();
            let ws = state.workspace(id).context("workspace not found")?;
            ws.servers.iter().filter(|s| s.enabled).map(|s| s.id.clone()).collect()
        };
        let running: Vec<ServerId> = self.upstreams.lock().unwrap().keys().cloned().collect();
        futures::future::join_all(running.iter().map(|sid| self.halt_server(sid))).await;
        self.mutate(|s| {
            s.active_workspace = id.to_string();
            Ok(())
        })?;
        self.rebuild_catalog();
        for sid in &to_start {
            self.spawn_supervisor(sid)?;
        }
        let name = self.state.lock().unwrap().workspace(id).map(|w| w.name.clone()).unwrap_or_default();
        self.log_event("workspace.activated", None, "info", format!("Activated workspace {name}"));
        self.emit_state();
        Ok(())
    }

    // Servers

    pub async fn add_server(&self, workspace_id: &str, input: ServerInput) -> Result<ServerConfig> {
        let _guard = self.ops.lock().await;
        validate(&input)?;
        let config = self.mutate(|s| {
            let ws = s.workspace_mut(workspace_id).context("workspace not found")?;
            let mut config = ServerConfig::new(&input.name, input.transport.clone(), &ws.server_slugs());
            config.enabled = input.enabled;
            config.auto_restart = input.auto_restart;
            self.store.externalize(&mut config.transport, None, &input.secret_keys)?;
            ws.servers.push(config.clone());
            Ok(config)
        })?;
        if config.enabled && self.is_active_server(&config.id) {
            self.spawn_supervisor(&config.id)?;
        }
        self.emit_state();
        Ok(config)
    }

    pub async fn update_server(&self, id: &str, input: ServerInput) -> Result<ServerConfig> {
        let _guard = self.ops.lock().await;
        validate(&input)?;
        let (old, new) = self.mutate(|s| {
            let taken: Vec<String> = s
                .workspaces
                .iter()
                .find(|w| w.servers.iter().any(|x| x.id == id))
                .map(|w| w.servers.iter().filter(|x| x.id != id).map(|x| x.slug.clone()).collect())
                .context("server not found")?;
            let server = s.find_server_mut(id).context("server not found")?;
            let old = server.clone();
            if server.name != input.name {
                server.slug = unique_slug(&input.name, &taken);
            }
            server.name = input.name.clone();
            server.enabled = input.enabled;
            server.auto_restart = input.auto_restart;
            server.transport = input.transport.clone();
            self.store.externalize(&mut server.transport, Some(&old.transport), &input.secret_keys)?;
            if server.oauth && endpoint_url(&old.transport) != endpoint_url(&server.transport) {
                server.oauth = false;
            }
            Ok((old, server.clone()))
        })?;
        if old.oauth && !new.oauth {
            self.forget_sign_in(id);
        }
        let active = self.is_active_server(id);
        let changed = old.transport != new.transport || old.slug != new.slug;
        if !new.enabled || !active {
            self.halt_server(id).await;
        } else if changed || !self.supervised(id) {
            self.halt_server(id).await;
            self.spawn_supervisor(id)?;
        } else {
            self.rebuild_catalog();
        }
        self.emit_state();
        Ok(new)
    }

    pub async fn delete_server(&self, id: &str) -> Result<()> {
        let _guard = self.ops.lock().await;
        self.halt_server(id).await;
        let removed = self.mutate(|s| {
            let ws = s
                .workspaces
                .iter_mut()
                .find(|w| w.servers.iter().any(|x| x.id == id))
                .context("server not found")?;
            let idx = ws.servers.iter().position(|x| x.id == id).expect("found above");
            Ok(ws.servers.remove(idx))
        })?;
        self.store.forget(&removed.transport);
        self.forget_sign_in(id);
        self.forget_runtime(id);
        self.rebuild_catalog();
        self.emit_state();
        Ok(())
    }

    pub async fn set_server_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        let _guard = self.ops.lock().await;
        self.mutate(|s| {
            s.find_server_mut(id).context("server not found")?.enabled = enabled;
            Ok(())
        })?;
        if enabled && self.is_active_server(id) {
            self.spawn_supervisor(id)?;
        } else {
            self.halt_server(id).await;
        }
        self.emit_state();
        Ok(())
    }

    pub async fn start_server(&self, id: &str) -> Result<()> {
        let _guard = self.ops.lock().await;
        if !self.is_active_server(id) {
            bail!("server belongs to an inactive workspace");
        }
        self.spawn_supervisor(id)
    }

    pub async fn stop_server(&self, id: &str) -> Result<()> {
        let _guard = self.ops.lock().await;
        self.server_config(id).context("server not found")?;
        self.halt_server(id).await;
        Ok(())
    }

    pub async fn restart_server(&self, id: &str) -> Result<()> {
        let _guard = self.ops.lock().await;
        if !self.is_active_server(id) {
            bail!("server belongs to an inactive workspace");
        }
        self.halt_server(id).await;
        self.spawn_supervisor(id)
    }

    pub async fn set_tool_enabled(&self, id: &str, tool: &str, enabled: bool) -> Result<()> {
        let _guard = self.ops.lock().await;
        self.mutate(|s| {
            let server = s.find_server_mut(id).context("server not found")?;
            if enabled {
                server.disabled_tools.remove(tool);
            } else {
                server.disabled_tools.insert(tool.to_string());
            }
            Ok(())
        })?;
        self.rebuild_catalog();
        self.emit_state();
        if let Ok(view) = self.server_tools(id) {
            self.emit("server.tools", &view);
        }
        Ok(())
    }

    pub fn server_tools(&self, id: &str) -> Result<ToolsView> {
        let config = self.server_config(id).context("server not found")?;
        let (live, tools) = {
            let ups = self.upstreams.lock().unwrap();
            match ups.get(id) {
                Some(u) if u.status.state == RunState::Running => (true, u.tools.clone()),
                _ => (false, self.tool_cache.lock().unwrap().get(id).cloned().unwrap_or_default()),
            }
        };
        let catalog = self.catalog.load();
        let tools = tools
            .into_iter()
            .map(|t| {
                let enabled = !config.disabled_tools.contains(t.name.as_ref());
                let exposed = catalog
                    .exposed_for(id, &t.name)
                    .map(str::to_string)
                    .unwrap_or_else(|| exposed_name(&config.slug, &t.name));
                ToolView {
                    name: t.name.to_string(),
                    exposed_name: exposed,
                    title: t.title.clone(),
                    description: t.description.as_ref().map(|d| d.to_string()),
                    input_schema: serde_json::Value::Object((*t.input_schema).clone()),
                    enabled,
                }
            })
            .collect();
        Ok(ToolsView { server_id: id.to_string(), live, tools })
    }

    pub(crate) fn remember_tools(&self, id: &str, tools: &[Tool]) {
        let snapshot = {
            let mut cache = self.tool_cache.lock().unwrap();
            cache.insert(id.to_string(), tools.to_vec());
            serde_json::to_vec(&*cache)
        };
        if let Ok(bytes) = snapshot {
            let path = self.store.paths.tool_cache();
            tokio::task::spawn_blocking(move || {
                if let Err(err) = write_atomic(&path, &bytes) {
                    tracing::warn!("tool cache: {err:#}");
                }
            });
        }
    }

    fn forget_runtime(&self, id: &str) {
        self.upstreams.lock().unwrap().remove(id);
        self.tool_cache.lock().unwrap().remove(id);
    }

    // Import

    pub fn import_preview(&self, workspace_id: &str, text: &str) -> Result<ImportPreview> {
        let names: Vec<String> = {
            let state = self.state.lock().unwrap();
            let ws = state.workspace(workspace_id).context("workspace not found")?;
            ws.servers.iter().map(|s| s.name.clone()).collect()
        };
        import::parse(text, &names)
    }

    pub async fn import_apply(
        &self,
        workspace_id: &str,
        text: &str,
        names: Option<Vec<String>>,
        policy: DuplicatePolicy,
    ) -> Result<ImportReport> {
        let preview = self.import_preview(workspace_id, text)?;
        let _guard = self.ops.lock().await;
        let mut report = ImportReport::default();
        let mut previous: Vec<ServerConfig> = Vec::new();
        self.mutate(|s| {
            let ws = s.workspace_mut(workspace_id).context("workspace not found")?;
            for action in import::plan_apply(&preview, ws, names.as_deref(), policy) {
                match action {
                    ImportAction::Add(mut config) => {
                        self.store.externalize(&mut config.transport, None, &BTreeSet::new())?;
                        report.added.push(config.id.clone());
                        ws.servers.push(config);
                    }
                    ImportAction::Replace { id, mut config } => {
                        let slot = ws.servers.iter_mut().find(|x| x.id == id).expect("planned from this workspace");
                        self.store.externalize(&mut config.transport, Some(&slot.transport), &BTreeSet::new())?;
                        previous.push(slot.clone());
                        *slot = config;
                        report.replaced.push(id);
                    }
                    ImportAction::Skip(name) => report.skipped.push(name),
                }
            }
            Ok(())
        })?;
        let active = self.state.lock().unwrap().active_workspace == workspace_id;
        if active {
            for id in report.replaced.iter() {
                self.halt_server(id).await;
            }
            for id in report.added.iter().chain(report.replaced.iter()) {
                if self.server_config(id).is_some_and(|c| c.enabled) {
                    self.spawn_supervisor(id)?;
                }
            }
        }
        self.log_event(
            "import",
            None,
            "info",
            format!(
                "Imported {} added, {} replaced, {} skipped",
                report.added.len(),
                report.replaced.len(),
                report.skipped.len()
            ),
        );
        self.emit_state();
        Ok(report)
    }

    // Export

    /// Keychain values are read back only with `include_secrets`, so the plain export never touches them.
    pub fn export_workspace(&self, id: &str, include_secrets: bool) -> Result<String> {
        let (name, mut servers) = {
            let state = self.state.lock().unwrap();
            let ws = state.workspace(id).context("workspace not found")?;
            (ws.name.clone(), ws.servers.clone())
        };
        if include_secrets {
            for server in &mut servers {
                server.transport = self.store.resolve(&server.transport)?;
            }
        }
        let secrets = if include_secrets { "with secrets" } else { "without secrets" };
        self.log_event("export", None, "info", format!("Exported {} servers from {name} {secrets}", servers.len()));
        Ok(export::render(&servers, include_secrets))
    }

    // Settings

    pub fn settings(&self) -> Settings {
        self.state.lock().unwrap().settings.clone()
    }

    pub async fn update_settings(&self, patch: SettingsPatch) -> Result<Settings> {
        let _guard = self.ops.lock().await;
        let (old, new) = self.mutate(|s| {
            let old = s.settings.clone();
            if let Some(port) = patch.port {
                s.settings.port = port;
            }
            if let Some(t) = patch.call_timeout_secs {
                s.settings.call_timeout_secs = t.max(1);
            }
            if let Some(d) = patch.log_retention_days {
                s.settings.log_retention_days = d.max(1);
            }
            Ok((old, s.settings.clone()))
        })?;
        if old.log_retention_days != new.log_retention_days {
            self.logs.set_retention(new.log_retention_days);
        }
        let restarted = if old.port != new.port && self.started.load(Ordering::SeqCst) {
            self.endpoint.lock().unwrap().port = new.port;
            self.restart_http().await
        } else {
            Ok(())
        };
        self.emit_state();
        restarted.map(|_| new)
    }

    /// Retries binding the configured port, e.g. after whatever held it has quit.
    pub async fn restart_endpoint(&self) -> Result<EndpointInfo> {
        let _guard = self.ops.lock().await;
        let restarted = self.restart_http().await;
        self.emit_state();
        restarted.map(|_| self.endpoint())
    }

    // Catalog

    pub(crate) fn rebuild_catalog(&self) {
        // Build and swap under the same locks: two concurrent rebuilds must not let the older
        // snapshot land last and hide a server that just came up.
        let (old, new, counts, warnings) = {
            let state = self.state.lock().unwrap();
            let ups = self.upstreams.lock().unwrap();
            let empty = Vec::new();
            let servers = state.active().map(|w| &w.servers).unwrap_or(&empty);
            let inputs: Vec<CatalogInput<'_>> = servers
                .iter()
                .filter_map(|s| {
                    let u = ups.get(&s.id).filter(|u| u.status.state == RunState::Running)?;
                    Some(CatalogInput {
                        server_id: &s.id,
                        slug: &s.slug,
                        tools: &u.tools,
                        prompts: &u.prompts,
                        resources: &u.resources,
                        disabled_tools: &s.disabled_tools,
                    })
                })
                .collect();
            let catalog = Catalog::build(&inputs);
            let counts = (catalog.tools.len(), catalog.prompts.len(), catalog.resources.len());
            let new = catalog.fingerprints;
            let warnings = catalog.warnings.clone();
            let old: Fingerprints = self.catalog.swap(Arc::new(catalog)).fingerprints;
            (old, new, counts, warnings)
        };
        for warning in &warnings {
            tracing::warn!("{warning}");
        }
        let mut changed = 0;
        if old.tools != new.tools {
            changed |= LIST_TOOLS;
        }
        if old.prompts != new.prompts {
            changed |= LIST_PROMPTS;
        }
        if old.resources != new.resources {
            changed |= LIST_RESOURCES;
        }
        if changed != 0 {
            self.pending_lists.fetch_or(changed, Ordering::SeqCst);
            self.lists_dirty.notify_one();
            self.emit(
                "catalog.changed",
                serde_json::json!({"tools": counts.0, "prompts": counts.1, "resources": counts.2}),
            );
        }
    }

    pub fn catalog(&self) -> Arc<Catalog> {
        self.catalog.load_full()
    }

    /// Coalesces bursts (e.g. many servers coming up) into one notification per list.
    async fn list_change_notifier(self: Arc<Self>) {
        loop {
            self.lists_dirty.notified().await;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let flags = self.pending_lists.swap(0, Ordering::SeqCst);
            if flags != 0 {
                self.sessions.notify(flags).await;
            }
        }
    }
}

fn non_empty<'a>(name: &'a str, fallback: &'a str) -> Result<&'a str> {
    let trimmed = name.trim();
    match (trimmed.is_empty(), fallback.is_empty()) {
        (false, _) => Ok(trimmed),
        (true, false) => Ok(fallback),
        (true, true) => Err(anyhow!("name must not be empty")),
    }
}

/// Tokens are bound to the server they were issued for.
fn endpoint_url(transport: &Transport) -> Option<&str> {
    match transport {
        Transport::Http { url, .. } => Some(url),
        Transport::Stdio { .. } => None,
    }
}

fn validate(input: &ServerInput) -> Result<()> {
    if input.name.trim().is_empty() {
        bail!("name must not be empty");
    }
    match &input.transport {
        Transport::Stdio { command, .. } if command.trim().is_empty() => bail!("command must not be empty"),
        Transport::Http { url, .. } if !(url.starts_with("http://") || url.starts_with("https://")) => {
            bail!("url must start with http:// or https://")
        }
        _ => Ok(()),
    }
}
