use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Router;
use axum::routing::get;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::app::Tulma;
use crate::gateway::GatewayHandler;
use crate::store::write_atomic;

pub struct HttpHandle {
    pub addr: SocketAddr,
    ct: CancellationToken,
    task: JoinHandle<()>,
}

impl HttpHandle {
    pub async fn stop(self) {
        self.ct.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(3), self.task).await;
    }
}

/// Read by `tulma connect` to find the gateway without configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointFile {
    pub url: String,
    pub port: u16,
    pub pid: u32,
}

pub fn mcp_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/mcp")
}

pub async fn serve(app: Arc<Tulma>, port: u16) -> Result<HttpHandle> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("cannot bind 127.0.0.1:{port}"))?;
    let addr = listener.local_addr()?;
    let ct = CancellationToken::new();
    let mut sessions = LocalSessionManager::default();
    // Quiet clients (no GET stream, rare calls) otherwise lose their session after 5 minutes.
    sessions.session_config.keep_alive = Some(Duration::from_secs(3600));
    let p = addr.port();
    let config = StreamableHttpServerConfig::default()
        .with_allowed_hosts(["127.0.0.1", "localhost", &format!("127.0.0.1:{p}"), &format!("localhost:{p}")])
        .with_allowed_origins([format!("http://127.0.0.1:{p}"), format!("http://localhost:{p}")])
        .with_cancellation_token(ct.child_token());
    let factory_app = app.clone();
    let service = StreamableHttpService::new(
        move || Ok(GatewayHandler::new(factory_app.clone())),
        Arc::new(sessions),
        config,
    );
    let router = Router::new()
        .route("/health", get(|| async { "ok" }))
        .nest_service("/mcp", service);
    let shutdown = ct.clone();
    let task = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, router)
            .with_graceful_shutdown(async move { shutdown.cancelled_owned().await })
            .await
        {
            tracing::warn!("http server stopped: {err}");
        }
    });
    Ok(HttpHandle { addr, ct, task })
}

impl Tulma {
    /// URL stays tied to the configured port: on conflict we report instead of hopping ports.
    pub(crate) async fn restart_http(&self) -> Result<()> {
        let mut slot = self.http.lock().await;
        if let Some(old) = slot.take() {
            old.stop().await;
        }
        let port = self.settings().port;
        match serve(self.arc(), port).await {
            Ok(handle) => {
                let url = mcp_url(handle.addr.port());
                {
                    let mut ep = self.endpoint.lock().unwrap();
                    ep.url = Some(url.clone());
                    ep.port = handle.addr.port();
                    ep.running = true;
                    ep.error = None;
                }
                let file = EndpointFile { url: url.clone(), port: handle.addr.port(), pid: std::process::id() };
                if let Err(err) = write_atomic(&self.store.paths.endpoint(), &serde_json::to_vec_pretty(&file)?) {
                    tracing::warn!("endpoint.json: {err:#}");
                }
                *slot = Some(handle);
                drop(slot);
                self.log_event("endpoint.started", None, "info", format!("Listening on {url}"));
                self.emit("endpoint.changed", self.endpoint());
                Ok(())
            }
            Err(err) => {
                let message = format!("{err:#}");
                {
                    let mut ep = self.endpoint.lock().unwrap();
                    ep.url = None;
                    ep.running = false;
                    ep.error = Some(message.clone());
                }
                drop(slot);
                self.log_event("endpoint.error", None, "error", message.clone());
                self.emit("endpoint.error", serde_json::json!({"port": port, "message": message}));
                Err(err)
            }
        }
    }
}
