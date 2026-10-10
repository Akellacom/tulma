mod common;

use std::time::Duration;

use common::*;
use tulma_core::control;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct Pipe {
    write: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    lines: tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    next_id: u64,
}

impl Pipe {
    async fn send_raw(&mut self, line: &str) {
        self.write.write_all(format!("{line}\n").as_bytes()).await.unwrap();
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send_raw(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()).await;
        self.response(id).await
    }

    async fn response(&mut self, id: u64) -> Value {
        loop {
            let msg = self.next().await;
            if msg["id"] == json!(id) {
                return msg;
            }
        }
    }

    async fn next(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(5), self.lines.next_line())
            .await
            .expect("control output within 5s")
            .unwrap()
            .expect("control stream open");
        serde_json::from_str(&line).expect("every output line is JSON")
    }

    async fn notification(&mut self, method: &str) -> Value {
        loop {
            let msg = self.next().await;
            if msg["method"] == method {
                return msg["params"].clone();
            }
        }
    }
}

async fn pipe() -> (tempfile::TempDir, std::sync::Arc<tulma_core::app::Tulma>, Pipe, tokio::task::JoinHandle<anyhow::Result<()>>) {
    let (dir, app) = boot().await;
    let (ours, theirs) = tokio::io::duplex(1 << 20);
    let (their_read, their_write) = tokio::io::split(theirs);
    let task = tokio::spawn(control::serve(app.clone(), their_read, their_write));
    let (read, write) = tokio::io::split(ours);
    (dir, app, Pipe { write, lines: BufReader::new(read).lines(), next_id: 0 }, task)
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_get_responses_and_state_changes_are_pushed() {
    let (_dir, _app, mut p, _task) = pipe().await;
    let state = p.request("state.get", json!({})).await;
    assert_eq!(state["result"]["workspaces"][0]["name"], "Default");

    let created = p.request("workspace.create", json!({"name": "Work"})).await;
    let ws = created["result"]["id"].as_str().unwrap().to_string();
    let pushed = p.notification("state.changed").await;
    assert!(pushed["workspaces"].as_array().unwrap().iter().any(|w| w["id"] == ws.as_str()));

    let added = p
        .request(
            "server.add",
            json!({"workspaceId": ws, "server": {"name": "Git Hub", "enabled": false,
                "transport": {"type": "stdio", "command": "npx", "args": ["-y", "x"],
                    "env": [{"key": "GITHUB_TOKEN", "value": "ghp_1"}, {"key": "MODE", "value": "a"}]}}}),
        )
        .await;
    let server_id = added["result"]["id"].as_str().unwrap().to_string();
    let state = p.request("state.get", json!({})).await;
    let server = state["result"]["workspaces"][1]["servers"][0].clone();
    assert_eq!(server["id"], server_id.as_str());
    assert_eq!(server["slug"], "git_hub");
    let env = server["transport"]["env"].as_array().unwrap();
    assert_eq!(env[0], json!({"key": "GITHUB_TOKEN", "value": "ghp_1", "secret": true}));
    assert_eq!(env[1]["secret"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn import_preview_and_apply_over_control() {
    let (_dir, _app, mut p, _task) = pipe().await;
    let ws = p.request("state.get", json!({})).await["result"]["activeWorkspace"].clone();
    let text = include_str!("fixtures/mcp-servers-2026-10-09.json");
    let preview = p.request("import.preview", json!({"workspaceId": ws, "text": text})).await;
    let items = preview["result"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 19);
    assert_eq!(items.iter().filter(|i| i["status"]["kind"] == "invalid").count(), 3);
    let report = p
        .request("import.apply", json!({"workspaceId": ws, "text": text, "names": ["git", "memory"], "duplicatePolicy": "skip"}))
        .await;
    assert_eq!(report["result"]["added"].as_array().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn workspace_export_hides_secrets_unless_asked() {
    let (_dir, _app, mut p, _task) = pipe().await;
    let ws = p.request("state.get", json!({})).await["result"]["activeWorkspace"].clone();
    p.request(
        "server.add",
        json!({"workspaceId": ws, "server": {"name": "git", "enabled": false,
            "transport": {"type": "stdio", "command": "npx",
                "env": [{"key": "GITHUB_TOKEN", "value": "ghp_1"}, {"key": "MODE", "value": "a"}]}}}),
    )
    .await;

    let plain = p.request("workspace.export", json!({"id": ws})).await;
    let plain: Value = serde_json::from_str(plain["result"]["text"].as_str().unwrap()).unwrap();
    assert_eq!(plain["mcpServers"]["git"]["env"], json!({"GITHUB_TOKEN": "", "MODE": "a"}));
    assert_eq!(plain["mcpServers"]["git"]["disabled"], true);

    let full = p.request("workspace.export", json!({"id": ws, "includeSecrets": true})).await;
    let text = full["result"]["text"].as_str().unwrap().to_string();
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["mcpServers"]["git"]["env"]["GITHUB_TOKEN"], "ghp_1");
    let preview = p.request("import.preview", json!({"workspaceId": ws, "text": text})).await;
    assert_eq!(preview["result"]["items"][0]["status"]["kind"], "duplicate");
}

#[tokio::test(flavor = "multi_thread")]
async fn errors_are_reported_per_request() {
    let (_dir, _app, mut p, _task) = pipe().await;
    let unknown = p.request("nope.nothing", json!({})).await;
    assert_eq!(unknown["error"]["code"], -32601);
    let bad = p.request("workspace.rename", json!({"id": 5})).await;
    assert_eq!(bad["error"]["code"], -32602);
    let missing = p.request("workspace.activate", json!({"id": "missing"})).await;
    assert_eq!(missing["error"]["code"], -32000);
    assert!(missing["error"]["message"].as_str().unwrap().contains("not found"));
    p.send_raw("{not json").await;
    let parse = p.next().await;
    assert_eq!(parse["error"]["code"], -32700);
}

#[tokio::test(flavor = "multi_thread")]
async fn eof_on_input_ends_the_session() {
    let (_dir, _app, p, task) = pipe().await;
    drop(p);
    let outcome = tokio::time::timeout(Duration::from_secs(5), task).await.expect("serve returns on EOF");
    assert!(outcome.unwrap().is_ok());
}
