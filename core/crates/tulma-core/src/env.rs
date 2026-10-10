use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::OnceCell;

const SHELL_TIMEOUT: Duration = Duration::from_millis(1500);
const MARKER: &str = "__TULMA_PATH__";

/// Expands `${VAR}`; unknown names stay verbatim and are reported so the caller can warn.
pub fn expand_vars(input: &str, lookup: impl Fn(&str) -> Option<String>) -> (String, Vec<String>) {
    let mut out = String::with_capacity(input.len());
    let mut unknown = Vec::new();
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return (out, unknown);
        };
        let name = &after[..end];
        match lookup(name) {
            Some(value) => out.push_str(&value),
            None => {
                out.push_str(&rest[start..start + 3 + end]);
                if !unknown.iter().any(|u| u == name) {
                    unknown.push(name.to_string());
                }
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    (out, unknown)
}

pub fn merge_path(sources: &[Option<&str>], fallback: &[PathBuf]) -> String {
    let mut seen: Vec<String> = Vec::new();
    let candidates = sources
        .iter()
        .flatten()
        .flat_map(|p| p.split(':').map(str::to_string))
        .chain(fallback.iter().map(|p| p.display().to_string()));
    for dir in candidates {
        if !dir.is_empty() && !seen.contains(&dir) {
            seen.push(dir);
        }
    }
    seen.join(":")
}

pub fn fallback_dirs(home: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![home.join(".local/bin")];
    if let Some(node) = nvm_node_bin(home) {
        dirs.push(node);
    }
    for rel in [".cargo/bin", ".bun/bin", ".volta/bin", ".deno/bin", "Library/pnpm", ".pyenv/shims"] {
        dirs.push(home.join(rel));
    }
    for abs in ["/opt/homebrew/bin", "/opt/homebrew/sbin", "/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
        dirs.push(PathBuf::from(abs));
    }
    dirs
}

fn parse_version(name: &str) -> Option<Vec<u64>> {
    name.trim_start_matches('v')
        .split('.')
        .map(|p| p.parse().ok())
        .collect()
}

fn nvm_node_bin(home: &Path) -> Option<PathBuf> {
    let versions_dir = home.join(".nvm/versions/node");
    let mut versions: Vec<(Vec<u64>, String)> = std::fs::read_dir(&versions_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            parse_version(&name).map(|v| (v, name))
        })
        .collect();
    versions.sort();
    let wanted = read_nvm_alias(home, "default");
    let prefix = wanted.as_deref().and_then(parse_version);
    let chosen = match prefix {
        Some(prefix) => versions
            .iter()
            .rev()
            .find(|(v, _)| v.starts_with(&prefix))
            .or(versions.last()),
        None => versions.last(),
    }?;
    Some(versions_dir.join(&chosen.1).join("bin"))
}

fn read_nvm_alias(home: &Path, name: &str) -> Option<String> {
    let value = std::fs::read_to_string(home.join(".nvm/alias").join(name)).ok()?;
    let value = value.trim().to_string();
    if parse_version(&value).is_some() {
        return Some(value);
    }
    let nested = std::fs::read_to_string(home.join(".nvm/alias").join(&value)).ok()?;
    let nested = nested.trim().to_string();
    parse_version(&nested).map(|_| nested)
}

pub fn resolve_command(command: &str, path: &str) -> Option<PathBuf> {
    let is_exec = |p: &Path| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if command.contains('/') {
        let p = PathBuf::from(command);
        return is_exec(&p).then_some(p);
    }
    path.split(':')
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(command))
        .find(|p| is_exec(p))
}

static LOGIN_PATH: OnceCell<Option<String>> = OnceCell::const_new();

/// GUI apps launch with a minimal PATH; the login shell knows where nvm/uvx/homebrew live.
pub async fn login_shell_path() -> Option<String> {
    LOGIN_PATH
        .get_or_init(|| async { query_login_shell().await })
        .await
        .clone()
}

async fn query_login_shell() -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let script = format!("printf '\\n{MARKER}%s{MARKER}\\n' \"$PATH\"");
    let child = tokio::process::Command::new(&shell)
        .args(["-lic", &script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = match tokio::time::timeout(SHELL_TIMEOUT, child).await {
        Ok(Ok(out)) => out,
        Ok(Err(err)) => {
            tracing::warn!("login shell PATH query failed: {err}");
            return None;
        }
        Err(_) => {
            tracing::warn!("login shell PATH query timed out");
            return None;
        }
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let start = text.find(MARKER)? + MARKER.len();
    let end = start + text[start..].find(MARKER)?;
    Some(text[start..end].to_string())
}

/// Child env: server env wins, PATH is assembled server → login shell → process → fallback.
pub async fn child_env(server_env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let login = login_shell_path().await;
    let process = std::env::var("PATH").ok();
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    let path = merge_path(
        &[server_env.get("PATH").map(String::as_str), login.as_deref(), process.as_deref()],
        &fallback_dirs(&home),
    );
    let mut env = server_env.clone();
    env.insert("PATH".into(), path);
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn expand_vars_prefers_lookup_and_keeps_unknown() {
        let env = BTreeMap::from([("AUTH_HEADER".to_string(), "Bearer abc".to_string())]);
        let (out, unknown) = expand_vars("Authorization:${AUTH_HEADER} ${MISSING} $PLAIN", |k| env.get(k).cloned());
        assert_eq!(out, "Authorization:Bearer abc ${MISSING} $PLAIN");
        assert_eq!(unknown, vec!["MISSING".to_string()]);
    }

    #[test]
    fn expand_vars_handles_unterminated_braces() {
        let (out, unknown) = expand_vars("x${OPEN", |_| Some("v".into()));
        assert_eq!(out, "x${OPEN");
        assert!(unknown.is_empty());
    }

    #[test]
    fn merge_path_keeps_order_and_dedups() {
        let merged = merge_path(
            &[Some("/a:/b"), None, Some("/b:/c:")],
            &[PathBuf::from("/a"), PathBuf::from("/d")],
        );
        assert_eq!(merged, "/a:/b:/c:/d");
    }

    #[test]
    fn fallback_dirs_pick_default_nvm_version() {
        let home = tempfile::tempdir().unwrap();
        let versions = home.path().join(".nvm/versions/node");
        for v in ["v18.20.0", "v22.3.0", "v20.11.1"] {
            fs::create_dir_all(versions.join(v).join("bin")).unwrap();
        }
        fs::create_dir_all(home.path().join(".nvm/alias")).unwrap();
        fs::write(home.path().join(".nvm/alias/default"), "20\n").unwrap();
        let dirs = fallback_dirs(home.path());
        assert!(dirs.contains(&versions.join("v20.11.1/bin")));
        assert!(!dirs.contains(&versions.join("v22.3.0/bin")));
        assert_eq!(dirs[0], home.path().join(".local/bin"));
        assert!(dirs.contains(&PathBuf::from("/opt/homebrew/bin")));
    }

    #[test]
    fn fallback_dirs_use_newest_nvm_without_alias() {
        let home = tempfile::tempdir().unwrap();
        let versions = home.path().join(".nvm/versions/node");
        for v in ["v9.0.0", "v18.20.0", "v22.3.0"] {
            fs::create_dir_all(versions.join(v).join("bin")).unwrap();
        }
        let dirs = fallback_dirs(home.path());
        assert!(dirs.contains(&versions.join("v22.3.0/bin")));
        assert!(!dirs.contains(&versions.join("v9.0.0/bin")));
    }

    #[test]
    fn resolve_command_searches_path_for_executables() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("npx");
        fs::write(&exe, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!("/nonexistent:{}", dir.path().display());
        assert_eq!(resolve_command("npx", &path), Some(exe.clone()));
        assert_eq!(resolve_command(exe.to_str().unwrap(), ""), Some(exe));
        assert_eq!(resolve_command("definitely-missing-cmd", &path), None);
    }

    #[tokio::test]
    #[ignore = "depends on the developer's login shell"]
    async fn login_shell_path_contains_homebrew() {
        let path = login_shell_path().await.expect("login shell PATH");
        assert!(path.split(':').any(|d| d == "/opt/homebrew/bin"), "{path}");
    }
}
