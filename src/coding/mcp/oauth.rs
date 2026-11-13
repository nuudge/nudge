//! OAuth for remote MCP servers. rmcp handles discovery, token exchange and
//! refresh; this module opens the browser, catches the loopback redirect, and
//! persists tokens so the browser flow runs once per server.

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use rmcp::transport::{
    StreamableHttpClientTransport,
    auth::{
        AuthClient, AuthError, AuthorizationManager, CredentialStore, OAuthClientConfig,
        OAuthState, OAuthTokenResponse, StoredCredentials,
    },
    streamable_http_client::StreamableHttpClientTransportConfig,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::core::AgentEvent;

/// Fixed: dynamic client registration registers this exact redirect URI.
const CALLBACK_PORT: u16 = 8765;

type AuthedTransport = StreamableHttpClientTransport<AuthClient<reqwest::Client>>;

/// `Some` for mid-session loads, where stderr is hidden behind the TUI.
type Notify<'a> = Option<&'a mpsc::Sender<AgentEvent>>;

#[derive(Serialize, Deserialize)]
struct PersistedAuth {
    client_id: String,
    token: OAuthTokenResponse,
}

pub async fn authorized_transport(
    name: &str,
    url: &str,
    scopes: &[String],
    client_id: Option<&str>,
    client_secret: Option<&str>,
    notify: Notify<'_>,
) -> Result<AuthedTransport> {
    match client_id {
        Some(cid) => pre_registered_transport(name, url, scopes, cid, client_secret, notify).await,
        None => dcr_transport(name, url, scopes, notify).await,
    }
}

async fn dcr_transport(
    name: &str,
    url: &str,
    scopes: &[String],
    notify: Notify<'_>,
) -> Result<AuthedTransport> {
    // A saved token that can't refresh falls through to a fresh browser flow
    // instead of failing the first request with "Auth required".
    if let Some(saved) = load_tokens(name)?
        && let Some(am) = revive_saved_credentials(name, url, saved).await
    {
        return Ok(make_transport(url, am));
    }

    let mut state = OAuthState::new(url, None)
        .await
        .map_err(|e| anyhow!("oauth discovery failed: {e}"))?;
    run_browser_flow(&mut state, scopes, notify).await?;
    if let Ok((client_id, Some(token))) = state.get_credentials().await {
        // Failing to save just means re-auth later.
        let _ = save_tokens(name, &PersistedAuth { client_id, token });
    }
    let am = state
        .into_authorization_manager()
        .ok_or_else(|| anyhow!("oauth: not authorized after flow"))?;
    Ok(make_transport(url, am))
}

/// `None` means the caller should re-run the browser flow. Re-persists on
/// success because DCR refresh tokens can rotate.
async fn revive_saved_credentials(
    name: &str,
    url: &str,
    saved: PersistedAuth,
) -> Option<AuthorizationManager> {
    let mut state = OAuthState::new(url, None).await.ok()?;
    state
        .set_credentials(&saved.client_id, saved.token)
        .await
        .ok()?;
    let am = state.into_authorization_manager()?;
    // Force a refresh: set_credentials stamps `token_received_at = now`, so
    // get_access_token would hand back a stale token as if it were fresh.
    match am.refresh_token().await {
        Ok(_) => {}
        Err(e) => {
            eprintln!("[mcp] {name}: cached token refresh failed ({e}); re-authorizing");
            return None;
        }
    }
    if let Ok((client_id, Some(token))) = am.get_credentials().await {
        let _ = save_tokens(name, &PersistedAuth { client_id, token });
    }
    Some(am)
}

fn make_transport(url: &str, am: AuthorizationManager) -> AuthedTransport {
    let client = AuthClient::new(reqwest::Client::default(), am);
    StreamableHttpClientTransport::with_client(
        client,
        StreamableHttpClientTransportConfig::with_uri(url),
    )
}

async fn run_browser_flow(
    state: &mut OAuthState,
    scopes: &[String],
    notify: Notify<'_>,
) -> Result<()> {
    let scope_refs: Vec<&str> = scopes.iter().map(String::as_str).collect();

    state
        .start_authorization(&scope_refs, &redirect_uri(), Some("nudge"))
        .await
        .map_err(|e| anyhow!("oauth start_authorization failed: {e}"))?;

    let auth_url = state
        .get_authorization_url()
        .await
        .map_err(|e| anyhow!("oauth get_authorization_url failed: {e}"))?;

    let (code, csrf) = prompt_and_capture(&auth_url, notify).await?;
    state
        .handle_callback(&code, &csrf)
        .await
        .map_err(|e| anyhow!("oauth token exchange failed: {e}"))?;
    Ok(())
}

/// `OAuthState` only does dynamic registration, so this uses the lower-level
/// `AuthorizationManager`.
async fn pre_registered_transport(
    name: &str,
    url: &str,
    scopes: &[String],
    client_id: &str,
    client_secret: Option<&str>,
    notify: Notify<'_>,
) -> Result<AuthedTransport> {
    let mut mgr = AuthorizationManager::new(url)
        .await
        .map_err(|e| anyhow!("oauth discovery failed: {e}"))?;
    let metadata = mgr
        .discover_metadata()
        .await
        .map_err(|e| anyhow!("oauth metadata discovery failed: {e}"))?;
    mgr.set_metadata(metadata);
    mgr.set_credential_store(FileCredentialStore::new(token_path(name)?));

    let mut config = OAuthClientConfig::new(client_id, redirect_uri()).with_scopes(scopes.to_vec());
    if let Some(secret) = client_secret {
        config = config.with_client_secret(secret);
    }
    mgr.configure_client(config)
        .map_err(|e| anyhow!("oauth configure_client failed: {e}"))?;

    let mut have_token = mgr.initialize_from_store().await.unwrap_or(false);
    if have_token && mgr.get_access_token().await.is_err() {
        have_token = false;
    }
    if !have_token {
        let scope_refs: Vec<&str> = scopes.iter().map(String::as_str).collect();
        let auth_url = mgr
            .get_authorization_url(&scope_refs)
            .await
            .map_err(|e| anyhow!("oauth get_authorization_url failed: {e}"))?;
        let (code, csrf) = prompt_and_capture(&auth_url, notify).await?;
        mgr.exchange_code_for_token(&code, &csrf)
            .await
            .map_err(|e| anyhow!("oauth token exchange failed: {e}"))?;
    }

    let client = AuthClient::new(reqwest::Client::default(), mgr);
    Ok(StreamableHttpClientTransport::with_client(
        client,
        StreamableHttpClientTransportConfig::with_uri(url),
    ))
}

fn redirect_uri() -> String {
    format!("http://127.0.0.1:{CALLBACK_PORT}/callback")
}

/// Also sent as a `Notice` when `notify` is set, since stderr is hidden behind
/// the TUI mid-session.
async fn prompt_and_capture(auth_url: &str, notify: Notify<'_>) -> Result<(String, String)> {
    let msg = format!(
        "[mcp] Authorize access in your browser:\n  {auth_url}\n  (waiting for the redirect on {} ...)",
        redirect_uri()
    );
    eprintln!("\n{msg}\n");
    if let Some(tx) = notify {
        let _ = tx.send(AgentEvent::Notice { text: msg }).await;
    }
    open_browser(auth_url);
    wait_for_callback(CALLBACK_PORT).await
}

async fn wait_for_callback(port: u16) -> Result<(String, String)> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| anyhow!("oauth: cannot bind callback port {port}: {e}"))?;
    let (mut stream, _) = listener.accept().await?;

    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).await?;
    let request = String::from_utf8_lossy(&buf[..n]);
    // Request line: `GET /callback?code=...&state=...&iss=... HTTP/1.1`
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("");
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");

    let (mut code, mut csrf) = (None, None);
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            match k {
                "code" => code = Some(percent_decode(v)),
                "state" => csrf = Some(percent_decode(v)),
                _ => {}
            }
        }
    }

    let body = "<!doctype html><html><body style=\"font-family:sans-serif\">\
        <h3>nudge</h3><p>Authorization complete — you can close this tab.</p></body></html>";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;

    Ok((
        code.ok_or_else(|| anyhow!("oauth callback missing `code`"))?,
        csrf.ok_or_else(|| anyhow!("oauth callback missing `state`"))?,
    ))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 3 <= bytes.len() => match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                Ok(byte) => {
                    out.push(byte);
                    i += 3;
                }
                Err(_) => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn open_browser(url: &str) {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(program).arg(url).spawn();
}

fn auth_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| anyhow!("HOME not set"))?;
    let dir = PathBuf::from(home).join(".nudge").join("mcp-auth");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn token_path(name: &str) -> Result<PathBuf> {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Ok(auth_dir()?.join(format!("{safe}.json")))
}

fn load_tokens(name: &str) -> Result<Option<PersistedAuth>> {
    let path = token_path(name)?;
    match std::fs::read_to_string(&path) {
        // A stale/incompatible file just triggers a fresh auth, not an error.
        Ok(text) => Ok(serde_json::from_str(&text).ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow!("reading {}: {e}", path.display())),
    }
}

fn save_tokens(name: &str, auth: &PersistedAuth) -> Result<()> {
    let path = token_path(name)?;
    std::fs::write(&path, serde_json::to_string_pretty(auth)?)?;
    restrict_perms(&path);
    Ok(())
}

#[cfg(unix)]
fn restrict_perms(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn restrict_perms(_path: &std::path::Path) {}

/// rmcp writes back after exchange/refresh, so refreshed tokens persist.
struct FileCredentialStore {
    path: PathBuf,
}

impl FileCredentialStore {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

#[async_trait]
impl CredentialStore for FileCredentialStore {
    async fn load(&self) -> std::result::Result<Option<StoredCredentials>, AuthError> {
        match std::fs::read_to_string(&self.path) {
            // A stale/incompatible file just triggers a fresh auth.
            Ok(text) => Ok(serde_json::from_str(&text).ok()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(AuthError::InternalError(format!("reading token file: {e}"))),
        }
    }

    async fn save(&self, credentials: StoredCredentials) -> std::result::Result<(), AuthError> {
        let json = serde_json::to_string_pretty(&credentials)
            .map_err(|e| AuthError::InternalError(e.to_string()))?;
        std::fs::write(&self.path, json).map_err(|e| AuthError::InternalError(e.to_string()))?;
        restrict_perms(&self.path);
        Ok(())
    }

    async fn clear(&self) -> std::result::Result<(), AuthError> {
        let _ = std::fs::remove_file(&self.path);
        Ok(())
    }
}
