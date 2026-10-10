use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use crate::model::{ServerConfig, Transport};
use crate::redact::{MASK, is_sensitive_key, redact_text};
use crate::store::SECRET_PREFIX;

/// Renders servers as an `mcpServers` JSON that `import`, Claude Desktop and Cursor read.
/// With `include_secrets` the transports must already be resolved. Without it, sensitive
/// env/header values are emptied and inline tokens in urls and args are masked.
pub fn render(servers: &[ServerConfig], include_secrets: bool) -> String {
    let mut entries = Map::new();
    for server in servers {
        let name = unique_name(&server.name, &entries);
        entries.insert(name, entry(server, include_secrets));
    }
    serde_json::to_string_pretty(&json!({ "mcpServers": entries })).expect("JSON values always serialize")
}

fn entry(server: &ServerConfig, include_secrets: bool) -> Value {
    let mut obj = Map::new();
    match &server.transport {
        Transport::Stdio { command, args, env, cwd } => {
            obj.insert("command".into(), command.clone().into());
            if !args.is_empty() {
                let args = if include_secrets { args.clone() } else { mask_args(args) };
                obj.insert("args".into(), args.into());
            }
            if !env.is_empty() {
                obj.insert("env".into(), pairs(env, include_secrets));
            }
            if let Some(cwd) = cwd {
                obj.insert("cwd".into(), cwd.to_string_lossy().into());
            }
        }
        Transport::Http { url, headers } => {
            obj.insert("type".into(), "http".into());
            let url = if include_secrets { url.clone() } else { redact_text(url) };
            obj.insert("url".into(), url.into());
            if !headers.is_empty() {
                obj.insert("headers".into(), pairs(headers, include_secrets));
            }
        }
    }
    if !server.enabled {
        obj.insert("disabled".into(), true.into());
    }
    Value::Object(obj)
}

fn pairs(map: &BTreeMap<String, String>, include_secrets: bool) -> Value {
    map.iter()
        .map(|(k, v)| {
            let value = if include_secrets {
                v.clone()
            } else if is_sensitive_key(k) || v.starts_with(SECRET_PREFIX) {
                String::new()
            } else {
                redact_text(v)
            };
            (k.clone(), Value::String(value))
        })
        .collect::<Map<_, _>>()
        .into()
}

/// Besides inline `--token=abc`, masks the value that follows a sensitive flag: `--api-key abc`.
fn mask_args(args: &[String]) -> Vec<String> {
    let mut after_flag = false;
    args.iter()
        .map(|arg| {
            let is_flag = arg.starts_with('-');
            let masked = if after_flag && !is_flag && !arg.starts_with('$') {
                MASK.to_string()
            } else {
                redact_text(arg)
            };
            after_flag = is_flag && !arg.contains('=') && is_sensitive_key(arg);
            masked
        })
        .collect()
}

fn unique_name(name: &str, taken: &Map<String, Value>) -> String {
    if !taken.contains_key(name) {
        return name.to_string();
    }
    (2..)
        .map(|n| format!("{name} {n}"))
        .find(|candidate| !taken.contains_key(candidate))
        .expect("unbounded range always yields a free name")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::import::{self, ImportStatus};

    fn stdio(name: &str, args: &[&str], env: &[(&str, &str)]) -> ServerConfig {
        ServerConfig::new(
            name,
            Transport::Stdio {
                command: "npx".into(),
                args: args.iter().map(|a| a.to_string()).collect(),
                env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
                cwd: None,
            },
            &[],
        )
    }

    fn http(name: &str, url: &str, headers: &[(&str, &str)]) -> ServerConfig {
        ServerConfig::new(
            name,
            Transport::Http {
                url: url.into(),
                headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            },
            &[],
        )
    }

    #[test]
    fn export_with_secrets_round_trips_through_import() {
        let mut git = stdio("git", &["-y", "mcp-server-git"], &[("GITHUB_TOKEN", "ghp_1"), ("MODE", "a")]);
        git.enabled = false;
        if let Transport::Stdio { cwd, .. } = &mut git.transport {
            *cwd = Some(PathBuf::from("/tmp/repo"));
        }
        let remote = http("Remote", "https://a.example/mcp", &[("Authorization", "Bearer abc")]);
        let servers = vec![git, remote];

        let text = render(&servers, true);
        let preview = import::parse(&text, &[]).unwrap();

        assert_eq!(preview.items.len(), 2);
        for (item, server) in preview.items.iter().zip(&servers) {
            assert_eq!(item.name, server.name);
            assert_eq!(item.status, ImportStatus::New);
            assert_eq!(item.enabled, server.enabled);
            assert_eq!(item.transport.as_ref(), Some(&server.transport));
        }
    }

    #[test]
    fn export_without_secrets_blanks_and_masks_them() {
        let servers = vec![
            stdio(
                "cli",
                &["--api-key", "abc", "--token=xyz", "--header", "Authorization:${AUTH_HEADER}", "--verbose"],
                &[("API_KEY", "keychain:1234"), ("AUTH_HEADER", "Bearer abc"), ("MODE", "fast")],
            ),
            http("stats", "https://s.example/mcp?token=abc&page=2", &[("Authorization", "keychain:5678"), ("X-Team", "core")]),
        ];

        let exported: Value = serde_json::from_str(&render(&servers, false)).unwrap();
        let cli = &exported["mcpServers"]["cli"];
        assert_eq!(
            cli["args"],
            json!(["--api-key", "***", "--token=***", "--header", "Authorization:${AUTH_HEADER}", "--verbose"])
        );
        assert_eq!(cli["env"], json!({"API_KEY": "", "AUTH_HEADER": "", "MODE": "fast"}));
        let stats = &exported["mcpServers"]["stats"];
        assert_eq!(stats["url"], "https://s.example/mcp?token=***&page=2");
        assert_eq!(stats["headers"], json!({"Authorization": "", "X-Team": "core"}));
        assert!(!render(&servers, false).contains("keychain:"));
    }

    #[test]
    fn colliding_names_get_a_suffix() {
        let servers = vec![stdio("git", &[], &[]), stdio("git", &[], &[]), stdio("git 2", &[], &[])];
        let exported: Value = serde_json::from_str(&render(&servers, true)).unwrap();
        let names: Vec<&String> = exported["mcpServers"].as_object().unwrap().keys().collect();
        assert_eq!(names, vec!["git", "git 2", "git 2 2"]);
    }
}
