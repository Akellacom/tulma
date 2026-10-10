use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::model::{ServerConfig, ServerId, Transport, Workspace};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ImportStatus {
    New,
    Duplicate,
    Invalid { reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportItem {
    pub name: String,
    pub transport: Option<Transport>,
    pub enabled: bool,
    pub status: ImportStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportPreview {
    pub items: Vec<ImportItem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DuplicatePolicy {
    Skip,
    Replace,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ImportAction {
    Add(ServerConfig),
    Replace { id: ServerId, config: ServerConfig },
    Skip(String),
}

/// Strips `//` and `/* */` comments and trailing commas outside of string literals.
pub fn strip_jsonc(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut in_string = false;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match (c, chars.get(i + 1)) {
            ('"', _) => {
                in_string = true;
                out.push(c);
                i += 1;
            }
            ('/', Some('/')) => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            ('/', Some('*')) => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
            }
            (',', _) => {
                let next = chars[i + 1..].iter().copied().find(|ch| !ch.is_whitespace());
                if !matches!(next, Some('}') | Some(']')) {
                    out.push(c);
                }
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

pub fn parse(text: &str, existing_names: &[String]) -> Result<ImportPreview> {
    let root: Value = serde_json::from_str(&strip_jsonc(text))
        .map_err(|err| anyhow::anyhow!("invalid JSON: {err}"))?;
    let Some(root) = root.as_object() else {
        bail!("expected a JSON object with \"mcpServers\"");
    };
    let servers = server_map(root);
    let items = servers
        .iter()
        .map(|(name, entry)| {
            let duplicate = existing_names.iter().any(|n| n.eq_ignore_ascii_case(name));
            parse_entry(name, entry, duplicate)
        })
        .collect();
    Ok(ImportPreview { items })
}

fn server_map(root: &Map<String, Value>) -> &Map<String, Value> {
    if let Some(Value::Object(m)) = root.get("mcpServers") {
        return m;
    }
    if let Some(Value::Object(m)) = root.get("mcp").and_then(|m| m.get("servers")) {
        return m;
    }
    if let Some(Value::Object(m)) = root.get("servers") {
        return m;
    }
    root
}

fn string_map(value: Option<&Value>) -> BTreeMap<String, String> {
    let Some(Value::Object(m)) = value else {
        return BTreeMap::new();
    };
    m.iter()
        .filter_map(|(k, v)| {
            let v = match v {
                Value::String(s) => s.clone(),
                Value::Null => return None,
                other => other.to_string(),
            };
            Some((k.clone(), v))
        })
        .collect()
}

fn strings(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))
            .collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}

fn parse_entry(name: &str, entry: &Value, duplicate: bool) -> ImportItem {
    let invalid = |reason: &str| ImportItem {
        name: name.to_string(),
        transport: None,
        enabled: false,
        status: ImportStatus::Invalid { reason: reason.to_string() },
        warning: None,
    };
    let Some(obj) = entry.as_object() else {
        return invalid("entry is not an object");
    };
    let enabled = match (obj.get("disabled"), obj.get("enabled")) {
        (Some(Value::Bool(d)), _) => !d,
        (_, Some(Value::Bool(e))) => *e,
        _ => true,
    };
    let kind = obj
        .get("type")
        .or_else(|| obj.get("transport"))
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase);
    let url = obj
        .get("url")
        .or_else(|| obj.get("serverUrl"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|u| !u.is_empty());
    let mut command_parts = strings(obj.get("command"));
    command_parts.retain(|p| !p.trim().is_empty());

    let is_remote = match kind.as_deref() {
        Some("http" | "sse" | "streamable-http" | "streamablehttp" | "streamable_http" | "remote") => true,
        Some("stdio" | "local") => false,
        Some(other) => return invalid(&format!("unknown transport \"{other}\"")),
        None => command_parts.is_empty() && url.is_some(),
    };

    let mut warning = None;
    let transport = if is_remote {
        let Some(url) = url else {
            return invalid("remote server without url");
        };
        if kind.as_deref() == Some("sse") || url.ends_with("/sse") {
            warning = Some("legacy SSE is not supported; if connecting fails, wrap it with `npx mcp-remote <url>`".into());
        }
        Transport::Http {
            url: url.to_string(),
            headers: string_map(obj.get("headers")),
        }
    } else {
        if command_parts.is_empty() {
            return invalid(if url.is_some() { "url given for a stdio server" } else { "empty command and no url" });
        }
        let command = command_parts.remove(0);
        let mut args = command_parts;
        args.extend(strings(obj.get("args")));
        let cwd = obj
            .get("cwd")
            .or_else(|| obj.get("workingDir"))
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
            .map(PathBuf::from);
        Transport::Stdio {
            command,
            args,
            env: string_map(obj.get("env")),
            cwd,
        }
    };
    ImportItem {
        name: name.to_string(),
        transport: Some(transport),
        enabled,
        status: if duplicate { ImportStatus::Duplicate } else { ImportStatus::New },
        warning,
    }
}

pub fn plan_apply(
    preview: &ImportPreview,
    workspace: &Workspace,
    selected: Option<&[String]>,
    policy: DuplicatePolicy,
) -> Vec<ImportAction> {
    let mut taken = workspace.server_slugs();
    preview
        .items
        .iter()
        .filter(|item| selected.is_none_or(|names| names.iter().any(|n| n == &item.name)))
        .map(|item| {
            let Some(transport) = item.transport.clone() else {
                return ImportAction::Skip(item.name.clone());
            };
            let existing = workspace
                .servers
                .iter()
                .find(|s| s.name.eq_ignore_ascii_case(&item.name));
            match (existing, policy) {
                (Some(_), DuplicatePolicy::Skip) => ImportAction::Skip(item.name.clone()),
                (Some(old), DuplicatePolicy::Replace) => ImportAction::Replace {
                    id: old.id.clone(),
                    config: ServerConfig {
                        enabled: item.enabled,
                        transport,
                        ..old.clone()
                    },
                },
                (None, _) => {
                    let mut config = ServerConfig::new(&item.name, transport, &taken);
                    config.enabled = item.enabled;
                    taken.push(config.slug.clone());
                    ImportAction::Add(config)
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/mcp-servers-2026-10-09.json");

    fn item<'a>(p: &'a ImportPreview, name: &str) -> &'a ImportItem {
        p.items.iter().find(|i| i.name == name).unwrap()
    }

    #[test]
    fn fixture_has_19_entries_with_3_invalid() {
        let preview = parse(FIXTURE, &[]).unwrap();
        assert_eq!(preview.items.len(), 19);
        let invalid: Vec<_> = preview
            .items
            .iter()
            .filter(|i| matches!(i.status, ImportStatus::Invalid { .. }))
            .map(|i| i.name.as_str())
            .collect();
        assert_eq!(invalid, vec!["mdn", "mdgate", "ahrefs"]);
        assert!(preview.items.iter().filter(|i| i.status == ImportStatus::New).count() == 16);
    }

    #[test]
    fn fixture_preserves_order_args_and_env() {
        let preview = parse(FIXTURE, &[]).unwrap();
        assert_eq!(preview.items[0].name, "youtrack");
        let Some(Transport::Stdio { command, args, env, .. }) = &item(&preview, "youtrack").transport else {
            panic!("youtrack must be stdio");
        };
        assert_eq!(command, "npx");
        assert_eq!(args.last().unwrap(), "Authorization:${AUTH_HEADER}");
        assert_eq!(env["AUTH_HEADER"], "Bearer ");
    }

    #[test]
    fn jsonc_comments_and_trailing_commas_are_tolerated() {
        let text = r#"{
          // VS Code style
          "mcp": { "servers": {
            "remote": { "type": "http", "url": "https://a.example/mcp", "headers": {"X-Key": "1",}, },
            /* block */ "local": { "command": ["uvx", "mcp-server-git"], "args": ["--repo", "."], "disabled": true, "cwd": "/tmp" },
            "url-in-string": { "command": "echo", "args": ["http://x//y", "/* not a comment */"] },
          }}
        }"#;
        let preview = parse(text, &[]).unwrap();
        assert_eq!(preview.items.len(), 3);
        assert_eq!(
            item(&preview, "remote").transport,
            Some(Transport::Http {
                url: "https://a.example/mcp".into(),
                headers: BTreeMap::from([("X-Key".into(), "1".into())]),
            })
        );
        let local = item(&preview, "local");
        assert!(!local.enabled);
        assert_eq!(
            local.transport,
            Some(Transport::Stdio {
                command: "uvx".into(),
                args: vec!["mcp-server-git".into(), "--repo".into(), ".".into()],
                env: BTreeMap::new(),
                cwd: Some(PathBuf::from("/tmp")),
            })
        );
        let Some(Transport::Stdio { args, .. }) = &item(&preview, "url-in-string").transport else { panic!() };
        assert_eq!(args, &vec!["http://x//y".to_string(), "/* not a comment */".to_string()]);
    }

    #[test]
    fn bare_map_and_sse_type_are_accepted() {
        let text = r#"{ "legacy": { "type": "sse", "url": "https://b.example/sse" }, "n": { "command": "node", "env": {"PORT": 8080} } }"#;
        let preview = parse(text, &[]).unwrap();
        let legacy = item(&preview, "legacy");
        assert!(matches!(legacy.transport, Some(Transport::Http { .. })));
        assert!(legacy.warning.as_deref().unwrap().contains("mcp-remote"));
        let Some(Transport::Stdio { env, .. }) = &item(&preview, "n").transport else { panic!() };
        assert_eq!(env["PORT"], "8080");
    }

    #[test]
    fn existing_names_are_marked_duplicate() {
        let preview = parse(FIXTURE, &["GitHub".to_string(), "memory".to_string()]).unwrap();
        assert_eq!(item(&preview, "github").status, ImportStatus::Duplicate);
        assert_eq!(item(&preview, "memory").status, ImportStatus::Duplicate);
        assert_eq!(item(&preview, "git").status, ImportStatus::New);
    }

    #[test]
    fn non_object_input_is_rejected() {
        assert!(parse("[1,2]", &[]).is_err());
        assert!(parse("not json", &[]).is_err());
    }

    #[test]
    fn plan_apply_honours_selection_and_duplicate_policy() {
        let mut ws = Workspace::new("W", &[]);
        let existing = ServerConfig::new(
            "git",
            Transport::Stdio { command: "old".into(), args: vec![], env: BTreeMap::new(), cwd: None },
            &[],
        );
        let existing_id = existing.id.clone();
        ws.servers.push(existing);
        let text = r#"{"mcpServers": {"git": {"command": "uvx"}, "Git Hub": {"command": "npx"}, "bad": {"command": ""}, "other": {"command": "x"}}}"#;
        let preview = parse(text, &["git".into()]).unwrap();
        let selected = vec!["git".to_string(), "Git Hub".to_string(), "bad".to_string()];

        let skip = plan_apply(&preview, &ws, Some(&selected), DuplicatePolicy::Skip);
        assert_eq!(skip.len(), 3);
        assert_eq!(skip[0], ImportAction::Skip("git".into()));
        let ImportAction::Add(added) = &skip[1] else { panic!() };
        assert_eq!(added.slug, "git_hub");
        assert_eq!(skip[2], ImportAction::Skip("bad".into()));

        let replace = plan_apply(&preview, &ws, None, DuplicatePolicy::Replace);
        let ImportAction::Replace { id, config } = &replace[0] else { panic!() };
        assert_eq!(id, &existing_id);
        assert_eq!(config.id, existing_id);
        assert_eq!(config.slug, "git");
        assert!(matches!(&config.transport, Transport::Stdio { command, .. } if command == "uvx"));
        assert_eq!(replace.len(), 4);
    }
}
