#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use tulma_core::app::{Tulma, ServerInput, SettingsPatch};
use tulma_core::model::Transport;
use tulma_core::store::{MemorySecrets, Paths};
use rmcp::model::{ClientCapabilities, ClientConfig, Implementation, ProtocolVersion};
use rmcp::service::{NotificationContext, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct Watcher(pub mpsc::UnboundedSender<&'static str>);

impl ClientHandler for Watcher {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(ClientCapabilities::default(), Implementation::new("test-client", "0.0.1"))
            .with_protocol_version(ProtocolVersion::LATEST_WITH_INITIALIZE)
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        let _ = self.0.send("tools");
    }
}

pub fn echo(name: &str) -> ServerInput {
    ServerInput {
        name: name.into(),
        enabled: true,
        auto_restart: true,
        transport: Transport::Stdio {
            command: env!("CARGO_BIN_EXE_echo-server").into(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
        },
    }
}

pub async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

pub async fn boot() -> (tempfile::TempDir, Arc<Tulma>) {
    let dir = tempfile::tempdir().unwrap();
    let app = Tulma::open(Paths::new(dir.path()), Arc::new(MemorySecrets::default())).unwrap();
    app.update_settings(SettingsPatch { port: Some(0), ..Default::default() }).await.unwrap();
    (dir, app)
}

pub async fn connect(app: &Tulma) -> (RunningService<RoleClient, Watcher>, mpsc::UnboundedReceiver<&'static str>) {
    let url = app.endpoint().url.expect("endpoint url");
    let (tx, rx) = mpsc::unbounded_channel();
    let transport = StreamableHttpClientTransport::from_config(StreamableHttpClientTransportConfig::with_uri(url));
    let client = Watcher(tx).serve(transport).await.expect("connect to gateway");
    (client, rx)
}

pub async fn tool_names(client: &RunningService<RoleClient, Watcher>) -> Vec<String> {
    client.list_all_tools().await.unwrap().into_iter().map(|t| t.name.to_string()).collect()
}

pub async fn expect_list_changed(rx: &mut mpsc::UnboundedReceiver<&'static str>) {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("tools/list_changed within 5s")
        .expect("watcher alive");
    while rx.try_recv().is_ok() {}
}

pub fn text_of(result: &rmcp::model::CallToolResult) -> String {
    serde_json::to_value(&result.content).unwrap()[0]["text"].as_str().unwrap_or_default().to_string()
}

