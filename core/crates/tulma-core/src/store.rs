use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::model::{State, Transport};
use crate::redact::is_sensitive_key;

pub const SECRET_PREFIX: &str = "keychain:";
const KEYCHAIN_SERVICE: &str = "app.tulma";

#[derive(Debug, Clone)]
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `TULMA_DATA_DIR` lets tests and side-by-side dev builds keep their own state.
    pub fn from_env() -> Self {
        if let Ok(dir) = std::env::var("TULMA_DATA_DIR") {
            return Self::new(dir);
        }
        let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
        Self::new(base.join("Tulma"))
    }

    pub fn ensure(&self) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("create data dir {}", self.root.display()))
    }

    pub fn config(&self) -> PathBuf {
        self.root.join("config.json")
    }
    pub fn logs_db(&self) -> PathBuf {
        self.root.join("logs.sqlite")
    }
    pub fn endpoint(&self) -> PathBuf {
        self.root.join("endpoint.json")
    }
    pub fn tool_cache(&self) -> PathBuf {
        self.root.join("tool-cache.json")
    }
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename into {}", path.display()))
}

pub trait SecretStore: Send + Sync {
    fn get(&self, account: &str) -> Result<Option<String>>;
    fn set(&self, account: &str, value: &str) -> Result<()>;
    fn delete(&self, account: &str) -> Result<()>;
}

#[derive(Default)]
pub struct MemorySecrets(Mutex<HashMap<String, String>>);

impl SecretStore for MemorySecrets {
    fn get(&self, account: &str) -> Result<Option<String>> {
        Ok(self.0.lock().unwrap().get(account).cloned())
    }
    fn set(&self, account: &str, value: &str) -> Result<()> {
        self.0.lock().unwrap().insert(account.into(), value.into());
        Ok(())
    }
    fn delete(&self, account: &str) -> Result<()> {
        self.0.lock().unwrap().remove(account);
        Ok(())
    }
}

pub struct KeychainSecrets;

impl SecretStore for KeychainSecrets {
    fn get(&self, account: &str) -> Result<Option<String>> {
        match security_framework::passwords::get_generic_password(KEYCHAIN_SERVICE, account) {
            Ok(bytes) => Ok(Some(String::from_utf8(bytes)?)),
            Err(err) if err.code() == security_framework_sys_not_found() => Ok(None),
            Err(err) => Err(anyhow::anyhow!("keychain read {account}: {err}")),
        }
    }
    fn set(&self, account: &str, value: &str) -> Result<()> {
        security_framework::passwords::set_generic_password(KEYCHAIN_SERVICE, account, value.as_bytes())
            .map_err(|err| anyhow::anyhow!("keychain write {account}: {err}"))
    }
    fn delete(&self, account: &str) -> Result<()> {
        match security_framework::passwords::delete_generic_password(KEYCHAIN_SERVICE, account) {
            Ok(()) => Ok(()),
            Err(err) if err.code() == security_framework_sys_not_found() => Ok(()),
            Err(err) => Err(anyhow::anyhow!("keychain delete {account}: {err}")),
        }
    }
}

const fn security_framework_sys_not_found() -> i32 {
    -25300 // errSecItemNotFound
}

pub struct Store {
    pub paths: Paths,
    secrets: Arc<dyn SecretStore>,
    cache: Mutex<HashMap<String, String>>,
}

impl Store {
    pub fn new(paths: Paths, secrets: Arc<dyn SecretStore>) -> Self {
        Self {
            paths,
            secrets,
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn load(&self) -> Result<State> {
        let path = self.paths.config();
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
        }
    }

    pub fn save(&self, state: &State) -> Result<()> {
        self.paths.ensure()?;
        write_atomic(&self.paths.config(), &serde_json::to_vec_pretty(state)?)
    }

    /// Moves sensitive plain values into the secret store, reusing the ref a key already had.
    pub fn externalize(&self, transport: &mut Transport, previous: Option<&Transport>) -> Result<()> {
        let empty = Default::default();
        let prev = previous.map(Transport::secrets).unwrap_or(&empty);
        for (key, value) in transport.secrets_mut().iter_mut() {
            if value.starts_with(SECRET_PREFIX) || value.is_empty() || !is_sensitive_key(key) {
                continue;
            }
            let account = match prev.get(key).and_then(|v| v.strip_prefix(SECRET_PREFIX)) {
                Some(account) => {
                    if self.secret(account)?.as_deref() != Some(value.as_str()) {
                        self.put_secret(account, value)?;
                    }
                    account.to_string()
                }
                None => {
                    let account = uuid::Uuid::new_v4().to_string();
                    self.put_secret(&account, value)?;
                    account
                }
            };
            *value = format!("{SECRET_PREFIX}{account}");
        }
        let kept: Vec<&String> = transport.secrets().values().collect();
        for old in prev.values().filter(|v| v.starts_with(SECRET_PREFIX)) {
            if !kept.contains(&old) {
                self.delete_secret(&old[SECRET_PREFIX.len()..]);
            }
        }
        Ok(())
    }

    pub fn resolve(&self, transport: &Transport) -> Result<Transport> {
        let mut resolved = transport.clone();
        for value in resolved.secrets_mut().values_mut() {
            if let Some(account) = value.strip_prefix(SECRET_PREFIX) {
                *value = self
                    .secret(account)?
                    .with_context(|| format!("secret {account} missing from keychain"))?;
            }
        }
        Ok(resolved)
    }

    pub fn forget(&self, transport: &Transport) {
        for value in transport.secrets().values() {
            if let Some(account) = value.strip_prefix(SECRET_PREFIX) {
                self.delete_secret(account);
            }
        }
    }

    fn secret(&self, account: &str) -> Result<Option<String>> {
        if let Some(v) = self.cache.lock().unwrap().get(account) {
            return Ok(Some(v.clone()));
        }
        let value = self.secrets.get(account)?;
        if let Some(v) = &value {
            self.cache.lock().unwrap().insert(account.into(), v.clone());
        }
        Ok(value)
    }

    fn put_secret(&self, account: &str, value: &str) -> Result<()> {
        self.secrets.set(account, value)?;
        self.cache.lock().unwrap().insert(account.into(), value.into());
        Ok(())
    }

    fn delete_secret(&self, account: &str) {
        self.cache.lock().unwrap().remove(account);
        if let Err(err) = self.secrets.delete(account) {
            tracing::warn!("{err:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ServerConfig;
    use std::collections::BTreeMap;

    fn store() -> (tempfile::TempDir, Store, Arc<MemorySecrets>) {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Arc::new(MemorySecrets::default());
        let store = Store::new(Paths::new(dir.path()), secrets.clone());
        (dir, store, secrets)
    }

    fn stdio(env: &[(&str, &str)]) -> Transport {
        Transport::Stdio {
            command: "npx".into(),
            args: vec![],
            env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            cwd: None,
        }
    }

    #[test]
    fn load_returns_default_state_when_missing() {
        let (_dir, store, _) = store();
        let state = store.load().unwrap();
        assert_eq!(state.workspaces.len(), 1);
        assert_eq!(state.workspaces[0].name, "Default");
    }

    #[test]
    fn save_then_load_roundtrips_without_leftover_tmp() {
        let (dir, store, _) = store();
        let mut state = State::default();
        let ws = state.active_workspace.clone();
        state.workspace_mut(&ws).unwrap().servers.push(ServerConfig::new("git", stdio(&[]), &[]));
        store.save(&state).unwrap();
        assert_eq!(store.load().unwrap(), state);
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("config.json")]);
    }

    #[test]
    fn externalize_moves_sensitive_values_into_secret_store() {
        let (_dir, store, secrets) = store();
        let mut t = stdio(&[("GITHUB_TOKEN", "ghp_123"), ("MEMORY_FILE_PATH", "/tmp/m.json"), ("EXA_API_KEY", "")]);
        store.externalize(&mut t, None).unwrap();
        let env = t.secrets();
        let token_ref = env["GITHUB_TOKEN"].clone();
        assert!(token_ref.starts_with(SECRET_PREFIX));
        assert_eq!(env["MEMORY_FILE_PATH"], "/tmp/m.json");
        assert_eq!(env["EXA_API_KEY"], "");
        let account = token_ref.trim_start_matches(SECRET_PREFIX);
        assert_eq!(secrets.get(account).unwrap().as_deref(), Some("ghp_123"));
        assert_eq!(store.resolve(&t).unwrap(), stdio(&[("GITHUB_TOKEN", "ghp_123"), ("MEMORY_FILE_PATH", "/tmp/m.json"), ("EXA_API_KEY", "")]));
    }

    #[test]
    fn externalize_reuses_existing_ref_and_drops_orphans() {
        let (_dir, store, secrets) = store();
        let mut first = stdio(&[("GITHUB_TOKEN", "old"), ("API_KEY", "k")]);
        store.externalize(&mut first, None).unwrap();
        let token_ref = first.secrets()["GITHUB_TOKEN"].clone();
        let key_account = first.secrets()["API_KEY"].trim_start_matches(SECRET_PREFIX).to_string();

        let mut second = stdio(&[("GITHUB_TOKEN", "new")]);
        store.externalize(&mut second, Some(&first)).unwrap();
        assert_eq!(second.secrets()["GITHUB_TOKEN"], token_ref);
        assert_eq!(store.resolve(&second).unwrap().secrets()["GITHUB_TOKEN"], "new");
        assert_eq!(secrets.get(&key_account).unwrap(), None);
    }

    #[test]
    fn forget_deletes_all_refs() {
        let (_dir, store, secrets) = store();
        let mut t = Transport::Http {
            url: "https://x".into(),
            headers: BTreeMap::from([("Authorization".into(), "Bearer t".into())]),
        };
        store.externalize(&mut t, None).unwrap();
        let account = t.secrets()["Authorization"].trim_start_matches(SECRET_PREFIX).to_string();
        store.forget(&t);
        assert_eq!(secrets.get(&account).unwrap(), None);
    }

    #[test]
    #[ignore = "touches the login keychain"]
    fn keychain_roundtrip() {
        let k = KeychainSecrets;
        let account = format!("test-{}", uuid::Uuid::new_v4());
        k.set(&account, "v1").unwrap();
        assert_eq!(k.get(&account).unwrap().as_deref(), Some("v1"));
        k.delete(&account).unwrap();
        assert_eq!(k.get(&account).unwrap(), None);
    }
}
