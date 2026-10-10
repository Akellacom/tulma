mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::*;
use tulma_core::app::SettingsPatch;
use tulma_core::logs::LogQuery;
use tulma_core::model::Transport;
use tulma_core::upstream::RunState;
use rmcp::model::{CallToolRequestParams, GetPromptRequestParams, ReadResourceRequestParams};

#[tokio::test(flavor = "multi_thread")]
async fn gateway_routes_namespaced_tools_and_notifies_changes() {
    let (_dir, app) = boot().await;
    let ws1 = app.state().active_workspace.clone();
    let server = app.add_server(&ws1, echo("echo")).await.unwrap();
    app.start().await;
    wait_for("echo running", || app.status(&server.id).state == RunState::Running).await;

    let (client, mut changes) = connect(&app).await;
    let names = tool_names(&client).await;
    assert!(names.contains(&"echo__ping".to_string()), "{names:?}");
    assert!(names.contains(&"echo__echo".to_string()));

    let result = client
        .call_tool(CallToolRequestParams::new("echo__echo").with_arguments(
            serde_json::json!({"text": "hi"}).as_object().unwrap().clone(),
        ))
        .await
        .unwrap();
    assert_eq!(text_of(&result), "hi");

    app.set_tool_enabled(&server.id, "ping", false).await.unwrap();
    expect_list_changed(&mut changes).await;
    assert!(!tool_names(&client).await.contains(&"echo__ping".to_string()));

    client.call_tool(CallToolRequestParams::new("echo__grow")).await.unwrap();
    expect_list_changed(&mut changes).await;
    wait_for("extra tool", || app.server_tools(&server.id).unwrap().tools.iter().any(|t| t.name == "extra_0")).await;
    assert!(tool_names(&client).await.contains(&"echo__extra_0".to_string()));

    let prompts = client.list_all_prompts().await.unwrap();
    assert_eq!(prompts[0].name, "echo__greet");
    client.get_prompt(GetPromptRequestParams::new("echo__greet")).await.unwrap();
    let resources = client.list_all_resources().await.unwrap();
    assert_eq!(resources[0].uri, "echo://readme");
    client.read_resource(ReadResourceRequestParams::new("echo://readme")).await.unwrap();

    let ws2 = app.create_workspace("Second").await.unwrap();
    let other = app.add_server(&ws2.id, echo("other")).await.unwrap();
    app.activate_workspace(&ws2.id).await.unwrap();
    wait_for("other running", || app.status(&other.id).state == RunState::Running).await;
    wait_for("echo stopped", || app.status(&server.id).state == RunState::Stopped).await;
    expect_list_changed(&mut changes).await;
    let names = tool_names(&client).await;
    assert!(names.contains(&"other__ping".to_string()), "{names:?}");
    assert!(!names.iter().any(|n| n.starts_with("echo__")));

    app.activate_workspace(&ws1).await.unwrap();
    wait_for("echo running again", || app.status(&server.id).state == RunState::Running).await;
    let pid = app.status(&server.id).pid.expect("pid");
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    wait_for("echo restarted", || {
        let s = app.status(&server.id);
        s.state == RunState::Running && s.pid.is_some_and(|p| p != pid)
    })
    .await;
    assert!(tool_names(&client).await.contains(&"echo__echo".to_string()));

    app.logs.flush();
    let calls = app.logs.tool_calls(&LogQuery::default()).unwrap();
    assert!(calls.iter().any(|c| c.exposed_tool == "echo__echo" && c.ok && c.args_json.contains("hi")));
    let stderr = app.logs.stderr(&server.id, 50).unwrap();
    assert!(stderr.iter().any(|l| l.line.contains("token=***")), "{stderr:?}");
    assert!(!stderr.iter().any(|l| l.line.contains("supersecret")));

    let final_pid = app.status(&server.id).pid.unwrap();
    client.cancel().await.unwrap();
    app.shutdown().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(unsafe { libc::kill(final_pid as i32, 0) }, -1, "child must be gone after shutdown");
}

#[tokio::test(flavor = "multi_thread")]
async fn slow_calls_time_out_with_a_visible_error() {
    let (_dir, app) = boot().await;
    app.update_settings(SettingsPatch { call_timeout_secs: Some(1), ..Default::default() }).await.unwrap();
    let ws = app.state().active_workspace.clone();
    let server = app.add_server(&ws, echo("echo")).await.unwrap();
    app.start().await;
    wait_for("echo running", || app.status(&server.id).state == RunState::Running).await;
    let (client, _changes) = connect(&app).await;
    let result = client
        .call_tool(CallToolRequestParams::new("echo__slow").with_arguments(
            serde_json::json!({"ms": 3000}).as_object().unwrap().clone(),
        ))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(text_of(&result).contains("timed out"), "{}", text_of(&result));
    client.cancel().await.unwrap();
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_command_reports_error_without_restart_loop() {
    let (_dir, app) = boot().await;
    let ws = app.state().active_workspace.clone();
    let mut input = echo("broken");
    input.auto_restart = false;
    input.transport = Transport::Stdio {
        command: "definitely-not-a-real-command".into(),
        args: vec![],
        env: BTreeMap::new(),
        cwd: None,
    };
    let server = app.add_server(&ws, input).await.unwrap();
    app.start().await;
    wait_for("error status", || app.status(&server.id).state == RunState::Error).await;
    let status = app.status(&server.id);
    assert!(status.error.unwrap().contains("command not found"));
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_server_kills_its_whole_process_group() {
    let (dir, app) = boot().await;
    let ws = app.state().active_workspace.clone();
    let pid_file = dir.path().join("grandchild.pid");
    let script = format!(
        "sleep 300 & echo $! > {}; exec {}",
        pid_file.display(),
        env!("CARGO_BIN_EXE_echo-server")
    );
    let mut input = echo("wrapped");
    input.transport = Transport::Stdio { command: "/bin/sh".into(), args: vec!["-c".into(), script], env: BTreeMap::new(), cwd: None };
    let server = app.add_server(&ws, input).await.unwrap();
    app.start().await;
    wait_for("wrapped running", || app.status(&server.id).state == RunState::Running).await;
    let grandchild: i32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
    assert_eq!(unsafe { libc::kill(grandchild, 0) }, 0, "grandchild alive while running");

    app.stop_server(&server.id).await.unwrap();
    assert_eq!(app.status(&server.id).state, RunState::Stopped);
    wait_for("grandchild reaped", || unsafe { libc::kill(grandchild, 0) } == -1).await;
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn modern_clients_get_result_type_on_forwarded_results() {
    use rmcp::model::{ClientConfig, ProtocolVersion, ResultType};
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
    use rmcp::{ClientLifecycleMode, ClientServiceExt};

    let (_dir, app) = boot().await;
    let ws = app.state().active_workspace.clone();
    let server = app.add_server(&ws, echo("echo")).await.unwrap();
    app.start().await;
    wait_for("echo running", || app.status(&server.id).state == RunState::Running).await;
    let transport = StreamableHttpClientTransport::from_config(StreamableHttpClientTransportConfig::with_uri(
        app.endpoint().url.unwrap(),
    ));
    let client = ClientConfig::default()
        .serve_with_lifecycle(transport, ClientLifecycleMode::Discover { preferred_versions: vec![ProtocolVersion::V_2026_07_28] })
        .await
        .expect("2026-07-28 client connects");
    let result = client.call_tool(CallToolRequestParams::new("echo__ping")).await.unwrap();
    assert_eq!(text_of(&result), "pong");
    assert_eq!(result.result_type, Some(ResultType::COMPLETE));
    let prompt = client.get_prompt(GetPromptRequestParams::new("echo__greet")).await.unwrap();
    assert_eq!(prompt.result_type, Some(ResultType::COMPLETE));
    let resource = client.read_resource(ReadResourceRequestParams::new("echo://readme")).await.unwrap();
    assert_eq!(resource.result_type, Some(ResultType::COMPLETE));
    client.cancel().await.unwrap();
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn modern_clients_hear_list_changes_through_listen() {
    use rmcp::model::{ClientConfig, ProtocolVersion, ServerNotification, SubscriptionFilter};
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
    use rmcp::{ClientLifecycleMode, ClientServiceExt};

    let (_dir, app) = boot().await;
    let ws = app.state().active_workspace.clone();
    let server = app.add_server(&ws, echo("echo")).await.unwrap();
    app.start().await;
    wait_for("echo running", || app.status(&server.id).state == RunState::Running).await;
    let transport = StreamableHttpClientTransport::from_config(StreamableHttpClientTransportConfig::with_uri(
        app.endpoint().url.unwrap(),
    ));
    let client = ClientConfig::default()
        .serve_with_lifecycle(transport, ClientLifecycleMode::Discover { preferred_versions: vec![ProtocolVersion::V_2026_07_28] })
        .await
        .unwrap();
    let mut subscription = client.listen(SubscriptionFilter::builder().tools_list_changed().build()).await.unwrap();
    wait_for("subscription registered", || app.endpoint().clients >= 1).await;
    app.set_tool_enabled(&server.id, "ping", false).await.unwrap();
    let note = tokio::time::timeout(Duration::from_secs(5), subscription.next()).await.expect("notification").unwrap();
    assert!(matches!(note, Some(ServerNotification::ToolListChangedNotification(_))), "{note:?}");
    subscription.cancel().await.unwrap();
    client.cancel().await.unwrap();
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn changing_port_starts_an_endpoint_that_failed_to_bind() {
    let blocker = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = blocker.local_addr().unwrap().port();
    let (_dir, app) = boot().await;
    app.update_settings(SettingsPatch { port: Some(taken), ..Default::default() }).await.ok();
    app.start().await;
    assert!(!app.endpoint().running, "port {taken} is taken");
    app.update_settings(SettingsPatch { port: Some(0), ..Default::default() }).await.unwrap();
    let ep = app.endpoint();
    assert!(ep.running, "{ep:?}");
    assert!(ep.error.is_none());
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn non_utf8_stderr_does_not_kill_the_server() {
    let (_dir, app) = boot().await;
    let ws = app.state().active_workspace.clone();
    let mut input = echo("noisy");
    let script = format!("printf 'bad \\377\\376 bytes\\n' >&2; exec {}", env!("CARGO_BIN_EXE_echo-server"));
    input.transport = Transport::Stdio { command: "/bin/sh".into(), args: vec!["-c".into(), script], env: BTreeMap::new(), cwd: None };
    let server = app.add_server(&ws, input).await.unwrap();
    app.start().await;
    wait_for("noisy running", || app.status(&server.id).state == RunState::Running).await;
    app.logs.flush();
    let lines = app.logs.stderr(&server.id, 10).unwrap();
    assert!(lines.iter().any(|l| l.line.starts_with("bad ")), "{lines:?}");
    assert!(lines.iter().any(|l| l.line.contains("echo-server ready")), "{lines:?}");
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn expanded_secrets_never_reach_the_event_log() {
    let (_dir, app) = boot().await;
    let ws = app.state().active_workspace.clone();
    let mut input = echo("keyed");
    input.transport = Transport::Stdio {
        command: env!("CARGO_BIN_EXE_echo-server").into(),
        args: vec!["--api-key".into(), "${EXA_API_KEY}".into()],
        env: BTreeMap::from([("EXA_API_KEY".into(), "sk-live-123".into())]),
        cwd: None,
    };
    let server = app.add_server(&ws, input).await.unwrap();
    app.start().await;
    wait_for("keyed running", || app.status(&server.id).state == RunState::Running).await;
    app.logs.flush();
    let events = app.logs.events(&LogQuery { server_id: Some(server.id.clone()), ..Default::default() }).unwrap();
    assert!(events.iter().any(|e| e.kind == "server.spawn"));
    assert!(!events.iter().any(|e| e.message.contains("sk-live-123")), "{events:?}");
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn hanging_tool_listing_times_out_and_stop_ends_it() {
    let (_dir, app) = boot().await;
    app.update_settings(SettingsPatch { call_timeout_secs: Some(1), ..Default::default() }).await.unwrap();
    let ws = app.state().active_workspace.clone();
    let mut input = echo("stuck");
    input.auto_restart = false;
    input.transport = Transport::Stdio {
        command: env!("CARGO_BIN_EXE_echo-server").into(),
        args: vec![],
        env: BTreeMap::from([("ECHO_HANG_LIST".into(), "1".into())]),
        cwd: None,
    };
    let server = app.add_server(&ws, input).await.unwrap();
    app.start().await;
    wait_for("listing error", || app.status(&server.id).state == RunState::Error).await;
    assert!(app.status(&server.id).error.unwrap().contains("timed out"));

    app.set_server_enabled(&server.id, true).await.unwrap();
    app.start_server(&server.id).await.unwrap();
    wait_for("starting again", || app.status(&server.id).pid.is_some() || app.status(&server.id).state == RunState::Starting).await;
    let started = std::time::Instant::now();
    app.stop_server(&server.id).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(5), "stop took {:?}", started.elapsed());
    assert_eq!(app.status(&server.id).state, RunState::Stopped);
    app.shutdown().await;
}
