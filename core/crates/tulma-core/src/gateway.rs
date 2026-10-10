use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, GetPromptRequestParams, GetPromptResponse, Implementation,
    ListPromptsResult, ListResourcesResult, ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams,
    ReadResourceResponse, ResultType, ServerCapabilities, ServerConfig, SubscriptionFilter,
};
use rmcp::service::{NotificationContext, RequestContext, SubscriptionContext, SubscriptionSink};
use rmcp::{ErrorData, Peer, RoleServer, ServerHandler};

use crate::app::{LIST_PROMPTS, LIST_RESOURCES, LIST_TOOLS, Tulma};

const NOTIFY_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone)]
enum Listener {
    Session(Peer<RoleServer>),
    Subscription(SubscriptionSink),
}

/// Every downstream client that should hear about list changes.
#[derive(Default)]
pub struct SessionRegistry {
    next: AtomicU64,
    listeners: Mutex<HashMap<u64, Listener>>,
}

impl SessionRegistry {
    fn add(&self, listener: Listener) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.listeners.lock().unwrap().insert(id, listener);
        id
    }

    fn remove(&self, id: u64) {
        self.listeners.lock().unwrap().remove(&id);
    }

    pub fn count(&self) -> usize {
        self.listeners.lock().unwrap().len()
    }

    pub async fn notify(self: &Self, flags: u8) {
        let listeners: Vec<(u64, Listener)> =
            self.listeners.lock().unwrap().iter().map(|(k, v)| (*k, v.clone())).collect();
        let results = futures::future::join_all(listeners.into_iter().map(|(id, listener)| async move {
            let ok = tokio::time::timeout(NOTIFY_TIMEOUT, send(&listener, flags))
                .await
                .unwrap_or(true);
            (id, ok)
        }))
        .await;
        for (id, ok) in results {
            if !ok {
                self.remove(id);
            }
        }
    }
}

/// Returns false once the listener's transport is gone so it can be dropped.
async fn send(listener: &Listener, flags: u8) -> bool {
    match listener {
        Listener::Session(peer) => {
            if peer.is_transport_closed() {
                return false;
            }
            let mut alive = true;
            if flags & LIST_TOOLS != 0 {
                alive &= peer.notify_tool_list_changed().await.is_ok();
            }
            if flags & LIST_PROMPTS != 0 {
                alive &= peer.notify_prompt_list_changed().await.is_ok();
            }
            if flags & LIST_RESOURCES != 0 {
                alive &= peer.notify_resource_list_changed().await.is_ok();
            }
            alive
        }
        Listener::Subscription(sink) => {
            let accepted = sink.accepted();
            let mut alive = true;
            if flags & LIST_TOOLS != 0 && accepted.tools_list_changed == Some(true) {
                alive &= sink.notify_tool_list_changed().await.is_ok();
            }
            if flags & LIST_PROMPTS != 0 && accepted.prompts_list_changed == Some(true) {
                alive &= sink.notify_prompt_list_changed().await.is_ok();
            }
            if flags & LIST_RESOURCES != 0 && accepted.resources_list_changed == Some(true) {
                alive &= sink.notify_resource_list_changed().await.is_ok();
            }
            alive
        }
    }
}

// Upstreams speak 2025-11-25, whose results omit `resultType`. 2026-07-28 clients require it,
// and rmcp strips it again for older peers, so forwarded results always get it set.

/// One instance per HTTP session (or per stateless request).
pub struct GatewayHandler {
    app: Arc<Tulma>,
    session: Mutex<Option<u64>>,
}

impl GatewayHandler {
    pub fn new(app: Arc<Tulma>) -> Self {
        Self { app, session: Mutex::new(None) }
    }
}

impl Drop for GatewayHandler {
    fn drop(&mut self) {
        if let Some(id) = self.session.lock().unwrap().take() {
            self.app.sessions.remove(id);
        }
    }
}

pub fn capabilities() -> ServerCapabilities {
    ServerCapabilities::builder()
        .enable_tools()
        .enable_tool_list_changed()
        .enable_prompts()
        .enable_prompts_list_changed()
        .enable_resources()
        .enable_resources_list_changed()
        .build()
}

impl ServerHandler for GatewayHandler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(capabilities())
            .with_server_info(Implementation::new("tulma", env!("CARGO_PKG_VERSION")))
    }

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        let id = self.app.sessions.add(Listener::Session(context.peer.clone()));
        if let Some(old) = self.session.lock().unwrap().replace(id) {
            self.app.sessions.remove(old);
        }
        let client = context
            .peer
            .peer_info()
            .map(|i| format!("{} {}", i.client_info.name, i.client_info.version))
            .unwrap_or_else(|| "unknown client".into());
        self.app.log_event("client.connected", None, "info", format!("{client} connected"));
    }

    fn accepted_subscription_filter(&self, requested: &SubscriptionFilter) -> Option<SubscriptionFilter> {
        Some(requested.supported_by(&capabilities()))
    }

    async fn listen(&self, context: SubscriptionContext) -> Result<(), ErrorData> {
        let id = self.app.sessions.add(Listener::Subscription(context.sink().clone()));
        context.cancelled().await;
        self.app.sessions.remove(id);
        Ok(())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.app.catalog.load().tools.clone()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        Ok(match self.app.call_tool(request, context.ct.clone()).await? {
            CallToolResponse::Complete(mut r) => {
                r.result_type = Some(ResultType::COMPLETE);
                CallToolResponse::Complete(r)
            }
            other => other,
        })
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        Ok(ListPromptsResult::with_all_items(self.app.catalog.load().prompts.clone()))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        Ok(match self.app.get_prompt(request).await? {
            GetPromptResponse::Complete(mut r) => {
                r.result_type = Some(ResultType::COMPLETE);
                GetPromptResponse::Complete(r)
            }
            other => other,
        })
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult::with_all_items(self.app.catalog.load().resources.clone()))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        Ok(match self.app.read_resource(request).await? {
            ReadResourceResponse::Complete(mut r) => {
                r.result_type = Some(ResultType::COMPLETE);
                ReadResourceResponse::Complete(r)
            }
            other => other,
        })
    }
}
