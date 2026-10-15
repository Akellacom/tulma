use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub type WorkspaceId = String;
pub type ServerId = String;

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub active_workspace: WorkspaceId,
    pub workspaces: Vec<Workspace>,
    #[serde(default)]
    pub settings: Settings,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    pub slug: String,
    #[serde(default)]
    pub servers: Vec<ServerConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerConfig {
    pub id: ServerId,
    pub name: String,
    pub slug: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub auto_restart: bool,
    pub transport: Transport,
    #[serde(default)]
    pub disabled_tools: BTreeSet<String>,
    /// Set once the user signed in; the tokens themselves live in the secret store.
    #[serde(default, skip_serializing_if = "is_false")]
    pub oauth: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Transport {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

impl Transport {
    pub fn kind(&self) -> &'static str {
        match self {
            Transport::Stdio { .. } => "stdio",
            Transport::Http { .. } => "http",
        }
    }

    pub fn secrets(&self) -> &BTreeMap<String, String> {
        match self {
            Transport::Stdio { env, .. } => env,
            Transport::Http { headers, .. } => headers,
        }
    }

    pub fn secrets_mut(&mut self) -> &mut BTreeMap<String, String> {
        match self {
            Transport::Stdio { env, .. } => env,
            Transport::Http { headers, .. } => headers,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub port: u16,
    pub call_timeout_secs: u64,
    pub log_retention_days: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            port: 4747,
            call_timeout_secs: 120,
            log_retention_days: 14,
        }
    }
}

fn default_true() -> bool {
    true
}

impl Default for State {
    fn default() -> Self {
        let ws = Workspace::new("Default", &[]);
        Self {
            active_workspace: ws.id.clone(),
            workspaces: vec![ws],
            settings: Settings::default(),
        }
    }
}

impl State {
    pub fn workspace(&self, id: &str) -> Option<&Workspace> {
        self.workspaces.iter().find(|w| w.id == id)
    }

    pub fn workspace_mut(&mut self, id: &str) -> Option<&mut Workspace> {
        self.workspaces.iter_mut().find(|w| w.id == id)
    }

    pub fn active(&self) -> Option<&Workspace> {
        self.workspace(&self.active_workspace)
    }

    pub fn find_server(&self, id: &str) -> Option<(&Workspace, &ServerConfig)> {
        self.workspaces
            .iter()
            .find_map(|w| w.servers.iter().find(|s| s.id == id).map(|s| (w, s)))
    }

    pub fn find_server_mut(&mut self, id: &str) -> Option<&mut ServerConfig> {
        self.workspaces
            .iter_mut()
            .find_map(|w| w.servers.iter_mut().find(|s| s.id == id))
    }

    pub fn is_active_server(&self, id: &str) -> bool {
        self.active()
            .is_some_and(|w| w.servers.iter().any(|s| s.id == id))
    }

    pub fn workspace_slugs(&self) -> Vec<String> {
        self.workspaces.iter().map(|w| w.slug.clone()).collect()
    }
}

impl Workspace {
    pub fn new(name: &str, taken_slugs: &[String]) -> Self {
        Self {
            id: new_id(),
            name: name.to_string(),
            slug: unique_slug(name, taken_slugs),
            servers: Vec::new(),
        }
    }

    pub fn server_slugs(&self) -> Vec<String> {
        self.servers.iter().map(|s| s.slug.clone()).collect()
    }

    /// Copies servers with fresh ids so the copy can run independently of the original.
    pub fn duplicate(&self, name: &str, taken_slugs: &[String]) -> Self {
        let mut copy = Workspace::new(name, taken_slugs);
        copy.servers = self
            .servers
            .iter()
            .map(|s| ServerConfig {
                id: new_id(),
                ..s.clone()
            })
            .collect();
        copy
    }
}

impl ServerConfig {
    pub fn new(name: &str, transport: Transport, taken_slugs: &[String]) -> Self {
        Self {
            id: new_id(),
            name: name.to_string(),
            slug: unique_slug(name, taken_slugs),
            enabled: true,
            auto_restart: true,
            transport,
            disabled_tools: BTreeSet::new(),
            oauth: false,
        }
    }
}

pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_sep = false;
    for ch in name.to_lowercase().chars() {
        let mapped = if ch.is_ascii_alphanumeric() || ch == '-' {
            ch
        } else {
            '_'
        };
        if mapped == '_' {
            if !last_sep {
                out.push('_');
            }
            last_sep = true;
        } else {
            out.push(mapped);
            last_sep = false;
        }
    }
    let trimmed = out.trim_matches(|c| c == '_' || c == '-');
    if trimmed.is_empty() {
        "server".to_string()
    } else {
        trimmed.to_string()
    }
}

pub fn unique_slug(name: &str, taken: &[String]) -> String {
    let base = slugify(name);
    if !taken.iter().any(|t| t == &base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}_{n}"))
        .find(|candidate| !taken.iter().any(|t| t == candidate))
        .expect("unbounded range always yields a free slug")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_lowercases_and_collapses_separators() {
        assert_eq!(slugify("My  Cool.Server!"), "my_cool_server");
        assert_eq!(slugify("openpanel-2"), "openpanel-2");
        assert_eq!(slugify("__Git__"), "git");
        assert_eq!(slugify("Файлы"), "server");
    }

    #[test]
    fn unique_slug_appends_counter_on_collision() {
        let taken = vec!["git".to_string(), "git_2".to_string()];
        assert_eq!(unique_slug("Git", &taken), "git_3");
        assert_eq!(unique_slug("memory", &taken), "memory");
    }

    #[test]
    fn duplicate_workspace_copies_servers_with_new_ids() {
        let mut ws = Workspace::new("Work", &[]);
        ws.servers.push(ServerConfig::new(
            "git",
            Transport::Stdio {
                command: "uvx".into(),
                args: vec!["mcp-server-git".into()],
                env: BTreeMap::new(),
                cwd: None,
            },
            &[],
        ));
        let copy = ws.duplicate("Work copy", &[ws.slug.clone()]);
        assert_ne!(copy.id, ws.id);
        assert_eq!(copy.slug, "work_copy");
        assert_eq!(copy.servers.len(), 1);
        assert_ne!(copy.servers[0].id, ws.servers[0].id);
        assert_eq!(copy.servers[0].slug, "git");
    }

    #[test]
    fn state_roundtrips_through_json() {
        let mut state = State::default();
        let ws_id = state.active_workspace.clone();
        state.workspace_mut(&ws_id).unwrap().servers.push(ServerConfig::new(
            "remote",
            Transport::Http {
                url: "https://example.com/mcp".into(),
                headers: BTreeMap::from([("Authorization".into(), "keychain:abc".into())]),
            },
            &[],
        ));
        let json = serde_json::to_string_pretty(&state).unwrap();
        let back: State = serde_json::from_str(&json).unwrap();
        assert_eq!(back, state);
        assert!(json.contains("\"type\": \"http\""));
    }
}
