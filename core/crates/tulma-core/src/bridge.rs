//! `tulma connect`: exposes the HTTP gateway over stdio for clients that only spawn processes.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CancelledNotificationParam, ClientCapabilities, ClientConfig,
    DiscoverRequestMethod, DiscoverResult, GetPromptRequestParams, GetPromptResponse, Implementation, ListPromptsResult, ListResourceTemplatesResult,
    ListResourcesResult, ListToolsResult, PaginatedRequestParams, ProtocolVersion, ReadResourceRequestParams,
    ReadResourceResponse, ResultType, ServerConfig,
};
use rmcp::service::{NotificationContext, RequestContext, RunningService, ServiceError};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{IntoTransport, StreamableHttpClientTransport};
use rmcp::{ClientHandler, ErrorData, Peer, RoleClient, RoleServer, ServerHandler, ServiceExt};

use crate::http::{EndpointFile, mcp_url};
use crate::store::Paths;

type DownstreamPeer = Arc<OnceLock<Peer<RoleServer>>>;

/// URL from `endpoint.json` written by the running app, else the default port.
pub fn default_url(paths: &Paths) -> String {
    std::fs::read(paths.endpoint())
        .ok()
        .and_then(|b| serde_json::from_slice::<EndpointFile>(&b).ok())
        .map(|e| e.url)
        .unwrap_or_else(|| mcp_url(4747))
}

#[derive(Clone)]
struct Upward {
    downstream: DownstreamPeer,
}

impl ClientHandler for Upward {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(ClientCapabilities::default(), Implementation::new("tulma-connect", env!("CARGO_PKG_VERSION")))
            .with_protocol_version(ProtocolVersion::LATEST_WITH_INITIALIZE)
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        if let Some(peer) = self.downstream.get() {
            let _ = peer.notify_tool_list_changed().await;
        }
    }

    async fn on_prompt_list_changed(&self, _context: NotificationContext<RoleClient>) {
        if let Some(peer) = self.downstream.get() {
            let _ = peer.notify_prompt_list_changed().await;
        }
    }

    async fn on_resource_list_changed(&self, _context: NotificationContext<RoleClient>) {
        if let Some(peer) = self.downstream.get() {
            let _ = peer.notify_resource_list_changed().await;
        }
    }
}

struct Downward {
    upstream: Peer<RoleClient>,
    downstream: DownstreamPeer,
}

fn upstream_error(err: ServiceError) -> ErrorData {
    match err {
        ServiceError::McpError(e) => e,
        other => ErrorData::internal_error(format!("tulma gateway: {other}"), None),
    }
}

impl ServerHandler for Downward {
    /// The bridge relays one legacy HTTP session and has no `subscriptions/listen`. Answering
    /// discovery like a pre-2026 server makes modern clients fall back to `initialize`, where
    /// list changes reach them.
    async fn discover(&self, _context: RequestContext<RoleServer>) -> Result<DiscoverResult, ErrorData> {
        Err(ErrorData::method_not_found::<DiscoverRequestMethod>())
    }

    fn get_info(&self) -> ServerConfig {
        let info = self.upstream.peer_info();
        let capabilities = info.as_ref().map(|i| i.capabilities.clone()).unwrap_or_else(crate::gateway::capabilities);
        let mut config = ServerConfig::new(capabilities).with_server_info(
            info.as_ref()
                .and_then(|i| i.server_info.clone())
                .unwrap_or_else(|| Implementation::new("tulma", env!("CARGO_PKG_VERSION"))),
        );
        config.instructions = info.and_then(|i| i.instructions.clone());
        config
    }

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        let _ = self.downstream.set(context.peer.clone());
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mut result = self.upstream.list_tools(request).await.map_err(upstream_error)?;
        result.result_type = Some(ResultType::COMPLETE);
        Ok(result)
    }

    async fn call_tool(
        &self,
        mut request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        request.meta = None;
        let handle = self
            .upstream
            .send_request_with_option(
                rmcp::model::ClientRequest::CallToolRequest(rmcp::model::CallToolRequest::new(request)),
                Default::default(),
            )
            .await
            .map_err(upstream_error)?;
        let id = handle.id.clone();
        tokio::select! {
            r = handle.await_response() => match r.map_err(upstream_error)? {
                rmcp::model::ServerResult::CallToolResult(mut r) => {
                    r.result_type = Some(ResultType::COMPLETE);
                    Ok(CallToolResponse::Complete(r))
                }
                rmcp::model::ServerResult::InputRequiredResult(r) => Ok(CallToolResponse::InputRequired(r)),
                rmcp::model::ServerResult::CreateTaskResult(r) => Ok(CallToolResponse::Task(r)),
                _ => Err(ErrorData::internal_error("unexpected response from gateway", None)),
            },
            _ = context.ct.cancelled() => {
                let _ = self.upstream.notify_cancelled(CancelledNotificationParam::new(Some(id), None)).await;
                Err(ErrorData::internal_error("cancelled", None))
            }
        }
    }

    async fn list_prompts(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        let mut result = self.upstream.list_prompts(request).await.map_err(upstream_error)?;
        result.result_type = Some(ResultType::COMPLETE);
        Ok(result)
    }

    async fn get_prompt(
        &self,
        mut request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        request.meta = None;
        Ok(match self.upstream.get_prompt_once(request).await.map_err(upstream_error)? {
            GetPromptResponse::Complete(mut r) => {
                r.result_type = Some(ResultType::COMPLETE);
                GetPromptResponse::Complete(r)
            }
            other => other,
        })
    }

    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let mut result = self.upstream.list_resources(request).await.map_err(upstream_error)?;
        result.result_type = Some(ResultType::COMPLETE);
        Ok(result)
    }

    async fn list_resource_templates(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        let mut result = self.upstream.list_resource_templates(request).await.map_err(upstream_error)?;
        result.result_type = Some(ResultType::COMPLETE);
        Ok(result)
    }

    async fn read_resource(
        &self,
        mut request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        request.meta = None;
        Ok(match self.upstream.read_resource_once(request).await.map_err(upstream_error)? {
            ReadResourceResponse::Complete(mut r) => {
                r.result_type = Some(ResultType::COMPLETE);
                ReadResourceResponse::Complete(r)
            }
            other => other,
        })
    }
}

async fn connect_upstream(url: &str, downstream: DownstreamPeer) -> Result<RunningService<RoleClient, Upward>> {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(url.to_string()).reinit_on_expired_session(true),
    );
    Upward { downstream }
        .serve(transport)
        .await
        .map_err(|e| anyhow!("cannot reach Tulma at {url}: {e}"))
}

pub async fn run<T, E, A>(url: &str, downstream: T) -> Result<()>
where
    T: IntoTransport<RoleServer, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    run_with_retry(url, downstream, Duration::from_secs(10)).await
}

/// Retries while the app may still be starting (e.g. both launched at login).
pub async fn run_with_retry<T, E, A>(url: &str, downstream: T, patience: Duration) -> Result<()>
where
    T: IntoTransport<RoleServer, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let slot: DownstreamPeer = Arc::new(OnceLock::new());
    let deadline = Instant::now() + patience;
    let mut delay = Duration::from_millis(100);
    let client = loop {
        match connect_upstream(url, slot.clone()).await {
            Ok(client) => break client,
            Err(err) if Instant::now() + delay < deadline => {
                tracing::debug!("{err:#}; retrying");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(2));
            }
            Err(err) => return Err(err),
        }
    };
    let server = Downward { upstream: client.peer().clone(), downstream: slot }
        .serve(downstream)
        .await
        .context("stdio handshake failed")?;
    let client_ct = client.cancellation_token();
    tokio::select! {
        _ = server.waiting() => {
            client_ct.cancel();
            Ok(())
        }
        _ = client.waiting() => Err(anyhow!("Tulma gateway closed the connection")),
    }
}
