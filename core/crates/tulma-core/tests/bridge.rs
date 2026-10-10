mod common;

use std::time::Duration;

use common::*;
use tulma_core::bridge;
use tulma_core::upstream::RunState;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use tokio::sync::mpsc;

#[tokio::test(flavor = "multi_thread")]
async fn stdio_bridge_proxies_tools_and_list_changes() {
    let (_dir, app) = boot().await;
    let ws = app.state().active_workspace.clone();
    let server = app.add_server(&ws, echo("echo")).await.unwrap();
    app.start().await;
    wait_for("echo running", || app.status(&server.id).state == RunState::Running).await;

    let url = app.endpoint().url.unwrap();
    let (bridge_side, client_side) = tokio::io::duplex(64 * 1024);
    let bridge_task = tokio::spawn(async move { bridge::run(&url, tokio::io::split(bridge_side)).await });

    let (tx, mut changes) = mpsc::unbounded_channel();
    let client = Watcher(tx).serve(tokio::io::split(client_side)).await.expect("initialize through bridge");
    let names = tool_names(&client).await;
    assert!(names.contains(&"echo__ping".to_string()), "{names:?}");
    let result = client.call_tool(CallToolRequestParams::new("echo__ping")).await.unwrap();
    assert_eq!(text_of(&result), "pong");

    app.set_tool_enabled(&server.id, "ping", false).await.unwrap();
    expect_list_changed(&mut changes).await;
    assert!(!tool_names(&client).await.contains(&"echo__ping".to_string()));

    client.cancel().await.unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), bridge_task).await.expect("bridge exits when stdin closes");
    assert!(outcome.unwrap().is_ok());
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn bridge_fails_fast_when_gateway_is_down() {
    let (bridge_side, _client_side) = tokio::io::duplex(1024);
    let err = bridge::run_with_retry("http://127.0.0.1:9/mcp", tokio::io::split(bridge_side), Duration::from_millis(300))
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("127.0.0.1:9"), "{err:#}");
}

#[tokio::test(flavor = "multi_thread")]
async fn bridge_keeps_modern_clients_on_a_protocol_it_can_relay() {
    use rmcp::model::ProtocolVersion;
    use rmcp::{ClientLifecycleMode, ClientServiceExt};

    let (_dir, app) = boot().await;
    let ws = app.state().active_workspace.clone();
    let server = app.add_server(&ws, echo("echo")).await.unwrap();
    app.start().await;
    wait_for("echo running", || app.status(&server.id).state == RunState::Running).await;
    let url = app.endpoint().url.unwrap();
    let (bridge_side, client_side) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move { bridge::run(&url, tokio::io::split(bridge_side)).await });

    let (tx, mut changes) = mpsc::unbounded_channel();
    let client = Watcher(tx)
        .serve_with_lifecycle(
            tokio::io::split(client_side),
            ClientLifecycleMode::Auto {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
                legacy_version: Some(ProtocolVersion::V_2025_11_25),
            },
        )
        .await
        .expect("client negotiates through the bridge");
    let result = client.call_tool(CallToolRequestParams::new("echo__ping")).await.unwrap();
    assert_eq!(text_of(&result), "pong");
    app.set_tool_enabled(&server.id, "ping", false).await.unwrap();
    expect_list_changed(&mut changes).await;
    client.cancel().await.unwrap();
    app.shutdown().await;
}
