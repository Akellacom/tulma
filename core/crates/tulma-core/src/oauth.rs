//! OAuth sign-in for remote servers. rmcp handles discovery, client registration, PKCE and
//! token refresh; this module keeps the tokens in the secret store and runs the loopback
//! redirect the browser comes back to.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use rmcp::transport::{
    AuthClient, AuthError, AuthorizationManager, AuthorizationRequest, AuthorizationSession, CredentialStore,
    StoredCredentials,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::app::Tulma;
use crate::model::Transport;
use crate::store::SecretStore;

const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);
const CLIENT_NAME: &str = "Tulma";
const MAX_REQUEST_HEAD: usize = 16 * 1024;

/// The upstream answered 401: the user has to sign in before it can run.
#[derive(Debug)]
pub(crate) struct NeedsAuth;

impl std::fmt::Display for NeedsAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("sign-in required")
    }
}

impl std::error::Error for NeedsAuth {}

fn account(server_id: &str) -> String {
    format!("oauth:{server_id}")
}

struct SecretCredentialStore {
    secrets: Arc<dyn SecretStore>,
    account: String,
}

fn store_error(err: impl std::fmt::Display) -> AuthError {
    AuthError::CredentialStoreError(err.to_string())
}

#[async_trait]
impl CredentialStore for SecretCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let raw = self.secrets.get(&self.account).map_err(store_error)?;
        raw.map(|json| serde_json::from_str(&json)).transpose().map_err(store_error)
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let json = serde_json::to_string(&credentials).map_err(store_error)?;
        self.secrets.set(&self.account, &json).map_err(store_error)
    }

    async fn clear(&self) -> Result<(), AuthError> {
        self.secrets.delete(&self.account).map_err(store_error)
    }
}

/// Same settings rmcp uses for its own default client: no pooling, no redirects that
/// would replay custom headers to another host.
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("reqwest client with static settings")
}

impl Tulma {
    fn credential_store(&self, server_id: &str) -> SecretCredentialStore {
        SecretCredentialStore { secrets: self.store.secret_store(), account: account(server_id) }
    }

    /// An HTTP client that attaches and refreshes the stored tokens, or `NeedsAuth` when
    /// there are none left to use.
    pub(crate) async fn authorized_client(&self, server_id: &str, url: &str) -> Result<AuthClient<reqwest::Client>> {
        let mut manager = AuthorizationManager::new(url).await?;
        manager.set_credential_store(self.credential_store(server_id));
        if !manager.initialize_from_store().await.context("load sign-in")? {
            return Err(NeedsAuth.into());
        }
        Ok(AuthClient::new(http_client(), manager))
    }

    /// Starts a browser sign-in and returns the URL to open. The result arrives later as a
    /// `server.auth` notification; a successful sign-in restarts the server.
    pub async fn authorize(&self, id: &str) -> Result<String> {
        let config = self.server_config(id).context("server not found")?;
        let Transport::Http { url, .. } = &config.transport else {
            bail!("sign-in is only available for remote (HTTP) servers");
        };
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.context("open the sign-in callback port")?;
        let redirect = format!("http://127.0.0.1:{}/callback", listener.local_addr()?.port());

        let mut manager = AuthorizationManager::new(url.as_str()).await?;
        manager.set_credential_store(self.credential_store(id));
        let resolution = manager.resolve_metadata().await.context("discover the authorization server")?;
        manager.set_metadata(resolution.metadata);
        let request = AuthorizationRequest::new(redirect).with_client_name(CLIENT_NAME);
        let session = AuthorizationSession::new(manager, request)
            .await
            .map_err(|(_, err)| anyhow!(err).context("register with the authorization server"))?;
        let auth_url = session.get_authorization_url().to_string();

        let app = self.arc();
        let sid = id.to_string();
        let task = tokio::spawn(async move {
            let outcome = complete_sign_in(&listener, &session).await;
            app.finish_sign_in(&sid, outcome).await;
        });
        if let Some(previous) = self.sign_ins.lock().unwrap().insert(id.to_string(), task) {
            previous.abort();
        }
        self.log_event("server.auth", Some(id), "info", "sign-in started in the browser".into());
        Ok(auth_url)
    }

    async fn finish_sign_in(&self, id: &str, outcome: Result<()>) {
        self.sign_ins.lock().unwrap().remove(id);
        let outcome = match outcome {
            Ok(()) => self.set_signed_in(id, true).await,
            Err(err) => Err(err),
        };
        match &outcome {
            Ok(()) => {
                self.log_event("server.auth", Some(id), "info", "signed in".into());
                self.emit("server.auth", serde_json::json!({"serverId": id, "ok": true}));
            }
            Err(err) => {
                let message = format!("{err:#}");
                self.log_event("server.auth", Some(id), "error", format!("sign-in failed: {message}"));
                self.emit("server.auth", serde_json::json!({"serverId": id, "ok": false, "error": message}));
            }
        }
    }

    /// Drops the tokens; the server restarts and will ask to sign in again.
    pub async fn sign_out(&self, id: &str) -> Result<()> {
        self.server_config(id).context("server not found")?;
        self.forget_sign_in(id);
        self.set_signed_in(id, false).await?;
        self.log_event("server.auth", Some(id), "info", "signed out".into());
        Ok(())
    }

    async fn set_signed_in(&self, id: &str, signed_in: bool) -> Result<()> {
        let _guard = self.ops.lock().await;
        self.mutate(|s| {
            s.find_server_mut(id).context("server not found")?.oauth = signed_in;
            Ok(())
        })?;
        let runnable = self.is_active_server(id) && self.server_config(id).is_some_and(|c| c.enabled);
        self.halt_server(id).await;
        if runnable {
            self.spawn_supervisor(id)?;
        }
        self.emit_state();
        Ok(())
    }

    /// Removes stored tokens and abandons a sign-in still waiting for the browser.
    pub(crate) fn forget_sign_in(&self, id: &str) {
        if let Some(task) = self.sign_ins.lock().unwrap().remove(id) {
            task.abort();
        }
        if let Err(err) = self.store.secret_store().delete(&account(id)) {
            tracing::warn!("{err:#}");
        }
    }
}

async fn complete_sign_in(listener: &TcpListener, session: &AuthorizationSession) -> Result<()> {
    let (target, mut stream) = tokio::time::timeout(SIGN_IN_TIMEOUT, accept_callback(listener))
        .await
        .map_err(|_| anyhow!("sign-in timed out after {} minutes", SIGN_IN_TIMEOUT.as_secs() / 60))??;
    let port = listener.local_addr()?.port();
    let outcome = match parse_callback(&target) {
        Ok(()) => session
            .handle_callback_url(&format!("http://127.0.0.1:{port}{target}"))
            .await
            .map(|_| ())
            .map_err(|err| anyhow!(err).context("exchange the authorization code")),
        Err(err) => Err(err),
    };
    let page = match &outcome {
        Ok(()) => result_page("Signed in", "Tulma is connected. You can close this tab."),
        Err(err) => result_page("Sign-in failed", &format!("{err:#}")),
    };
    let _ = respond(&mut stream, "200 OK", &page).await;
    outcome
}

/// Waits for the browser's `GET /callback?...`, answering anything else (favicon) with 404.
async fn accept_callback(listener: &TcpListener) -> Result<(String, TcpStream)> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let Ok(head) = read_head(&mut stream).await else { continue };
        match request_target(&head) {
            Some(target) if target.starts_with("/callback") => return Ok((target.to_string(), stream)),
            _ => {
                let _ = respond(&mut stream, "404 Not Found", "").await;
            }
        }
    }
}

async fn read_head(stream: &mut TcpStream) -> Result<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut chunk)).await??;
        if n == 0 || buf.len() > MAX_REQUEST_HEAD {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn request_target(head: &str) -> Option<&str> {
    let mut parts = head.lines().next()?.split_whitespace();
    (parts.next()? == "GET").then_some(())?;
    parts.next()
}

/// Turns an `error=` redirect into a readable message; a `code` redirect passes through.
fn parse_callback(target: &str) -> Result<()> {
    let url = reqwest::Url::parse(&format!("http://127.0.0.1{target}"))?;
    let param = |name: &str| url.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned());
    if let Some(error) = param("error") {
        match param("error_description") {
            Some(description) => bail!("{error}: {description}"),
            None => bail!("{error}"),
        }
    }
    if param("code").is_none() {
        bail!("the authorization server sent no code");
    }
    Ok(())
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

fn result_page(title: &str, message: &str) -> String {
    let escape = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!(
        "<!doctype html><meta charset=utf-8><title>{t}</title>\
         <body style=\"font:15px -apple-system,sans-serif;margin:15vh auto;max-width:34em;padding:0 16px\">\
         <h2>{t}</h2><p>{m}</p></body>",
        t = escape(title),
        m = escape(message)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemorySecrets;

    #[test]
    fn request_target_reads_get_lines_only() {
        assert_eq!(request_target("GET /callback?code=1 HTTP/1.1\r\nHost: x\r\n\r\n"), Some("/callback?code=1"));
        assert_eq!(request_target("POST /callback HTTP/1.1\r\n\r\n"), None);
        assert_eq!(request_target(""), None);
    }

    #[test]
    fn parse_callback_surfaces_provider_errors() {
        assert!(parse_callback("/callback?code=abc&state=s").is_ok());
        let err = parse_callback("/callback?error=access_denied&error_description=User+said+no").unwrap_err();
        assert_eq!(err.to_string(), "access_denied: User said no");
        assert!(parse_callback("/callback?state=s").is_err());
    }

    #[test]
    fn result_page_escapes_messages() {
        let page = result_page("Sign-in failed", "<script>x</script>");
        assert!(page.contains("&lt;script&gt;"));
        assert!(!page.contains("<script>"));
    }

    #[tokio::test]
    async fn credentials_roundtrip_through_the_secret_store() {
        let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecrets::default());
        let store = SecretCredentialStore { secrets: secrets.clone(), account: account("s1") };
        assert!(store.load().await.unwrap().is_none());
        store.save(StoredCredentials::new("client-1".into(), None, vec!["read".into()], Some(7))).await.unwrap();
        let loaded = store.load().await.unwrap().unwrap();
        assert_eq!(loaded.client_id, "client-1");
        assert_eq!(loaded.granted_scopes, vec!["read".to_string()]);
        assert!(secrets.get("oauth:s1").unwrap().is_some());
        store.clear().await.unwrap();
        assert!(store.load().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn accept_callback_skips_other_paths() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut favicon = TcpStream::connect(addr).await.unwrap();
            favicon.write_all(b"GET /favicon.ico HTTP/1.1\r\n\r\n").await.unwrap();
            let mut reply = String::new();
            favicon.read_to_string(&mut reply).await.unwrap();
            assert!(reply.starts_with("HTTP/1.1 404"));
            let mut callback = TcpStream::connect(addr).await.unwrap();
            callback.write_all(b"GET /callback?code=c&state=s HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
            let mut reply = String::new();
            callback.read_to_string(&mut reply).await.unwrap();
            reply
        });
        let (target, mut stream) = accept_callback(&listener).await.unwrap();
        assert_eq!(target, "/callback?code=c&state=s");
        respond(&mut stream, "200 OK", "done").await.unwrap();
        drop(stream);
        assert!(client.await.unwrap().ends_with("done"));
    }
}
