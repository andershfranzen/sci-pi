use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs::{self, File, OpenOptions}, io::Write, os::unix::fs::{OpenOptionsExt, PermissionsExt}, path::{Path, PathBuf}, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const REDIRECT: &str = "http://localhost:54545/callback";
const SKEW_MS: i64 = 300_000;

// Keep OAuth refresh, bootstrap and inference fingerprints on the same OMP profile.
pub const CLAUDE_CODE_VERSION: &str = "2.1.280";
pub const CLAUDE_SDK_VERSION: &str = "0.112.1";

#[derive(Clone, Serialize, Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: String,
    expires_at_ms: i64,
    #[serde(default)] account_id: Option<String>,
    #[serde(default)] email: Option<String>,
    #[serde(default)] org_id: Option<String>,
    #[serde(default)] org_name: Option<String>,
}

fn store_path() -> PathBuf { crate::config::config_dir().join("anthropic-oauth.json") }
fn lock_store() -> Result<File> { lock_file("anthropic-oauth.lock") }
fn lock_file(name: &str) -> Result<File> {
    let dir = crate::config::config_dir();
    fs::create_dir_all(&dir).context("create OAuth configuration directory")?;
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600)
        .open(dir.join(name)).context("open OAuth lock")?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.lock().context("lock Anthropic OAuth store")?;
    Ok(file)
}
async fn async_lock() -> Result<File> { tokio::task::spawn_blocking(lock_store).await.context("OAuth lock task failed")? }
fn load() -> Result<Option<Tokens>> {
    let path = store_path();
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("open Anthropic OAuth credentials"),
    };
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    let tokens: Tokens = serde_json::from_reader(file).context("invalid Anthropic OAuth store; log in again")?;
    ensure!(!tokens.access_token.is_empty() && !tokens.refresh_token.is_empty(), "incomplete Anthropic OAuth store; log in again");
    Ok(Some(tokens))
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("OAuth file has no parent")?;
    let temp = parent.join(format!(".anthropic-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() { let _ = fs::remove_file(&temp); }
    result.context("persist private Anthropic OAuth credentials")
}
fn save(tokens: &Tokens) -> Result<()> { atomic_write(&store_path(), &serde_json::to_vec_pretty(tokens)?) }
fn expires_soon(tokens: &Tokens, now: i64) -> bool { tokens.expires_at_ms <= now.saturating_add(SKEW_MS) }
fn text(value: &Value, pointer: &str) -> Option<String> {
    value.pointer(pointer).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned)
}
fn merge_response(value: &Value, previous: Option<&Tokens>, now: i64) -> Result<Tokens> {
    let access_token = text(value, "/access_token").context("Anthropic OAuth response omitted access token")?;
    let refresh_token = text(value, "/refresh_token").or_else(|| previous.map(|p| p.refresh_token.clone()))
        .context("Anthropic OAuth response omitted refresh token")?;
    let seconds = value.get("expires_in").and_then(Value::as_i64).filter(|s| *s > 0)
        .context("Anthropic OAuth response has invalid expiry")?;
    let expires_at_ms = seconds.checked_mul(1000).and_then(|ms| now.checked_add(ms))
        .context("Anthropic OAuth expiry out of range")?;
    Ok(Tokens {
        access_token, refresh_token, expires_at_ms,
        account_id: text(value, "/account/uuid").or_else(|| previous.and_then(|p| p.account_id.clone())),
        email: text(value, "/account/email_address").or_else(|| previous.and_then(|p| p.email.clone())),
        // Organization scope is fixed at login, never overwritten by refresh.
        org_id: previous.and_then(|p| p.org_id.clone()).or_else(|| if previous.is_none() { text(value, "/organization/uuid") } else { None }),
        org_name: previous.and_then(|p| p.org_name.clone()).or_else(|| if previous.is_none() { text(value, "/organization/name") } else { None }),
    })
}
async fn exchange(http: &reqwest::Client, body: Value, refresh: bool) -> Result<Value> {
    exchange_at(http, body, refresh, "https://api.anthropic.com/v1/oauth/token").await
}
async fn exchange_at(http: &reqwest::Client, body: Value, refresh: bool, url: &str) -> Result<Value> {
    let mut request = http.post(url)
        .timeout(Duration::from_secs(30)).json(&body);
    if refresh { request = request.header("anthropic-beta", "oauth-2025-04-20")
        .header("User-Agent", format!("anthropic-sdk-typescript/{CLAUDE_SDK_VERSION} userOAuthProvider")); }
    let response = request.send().await.map_err(|_| anyhow::anyhow!("Anthropic OAuth token request failed (network or proxy); retry login/refresh"))?;
    ensure!(response.status().is_success(), "Anthropic OAuth {} failed (HTTP {}); log in again if authorization expired", if refresh { "refresh" } else { "exchange" }, response.status());
    response.json().await.map_err(|_| anyhow::anyhow!("Anthropic OAuth returned invalid JSON"))
}
async fn enrich(http: &reqwest::Client, tokens: &mut Tokens, login: bool) {
    if tokens.account_id.is_some() && tokens.email.is_some() && (!login || tokens.org_id.is_some()) { return; }
    // OMP treats bootstrap identity as best effort; inference tokens remain usable without it.
    let result = async {
        let response = http.get("https://api.anthropic.com/api/claude_cli/bootstrap?entrypoint=cli&model=claude-opus-4-8")
            .bearer_auth(&tokens.access_token).header("Accept", "application/json, text/plain, */*")
            .header("Content-Type", "application/json").header("User-Agent", format!("claude-code/{CLAUDE_CODE_VERSION}"))
            .header("anthropic-beta", "oauth-2025-04-20").timeout(Duration::from_secs(30)).send().await?;
        response.error_for_status()?.json::<Value>().await
    }.await;
    if let Ok(value) = result {
        tokens.account_id = tokens.account_id.take().or_else(|| text(&value, "/oauth_account/account_uuid"));
        tokens.email = tokens.email.take().or_else(|| text(&value, "/oauth_account/account_email"));
        if login {
            tokens.org_id = tokens.org_id.take().or_else(|| text(&value, "/oauth_account/organization_uuid"));
            tokens.org_name = tokens.org_name.take().or_else(|| text(&value, "/oauth_account/organization_name"));
        }
    }
}

/// API keys retain priority. OAuth refresh uses the caller's configured HTTP/proxy client.
pub async fn credential(http: &reqwest::Client) -> Result<String> {
    if let Some(key) = crate::config::credential("anthropic", None) { return Ok(key); }
    let tokens = load()?.context("Anthropic credentials missing; run `sci-pi auth login anthropic` or set ANTHROPIC_API_KEY")?;
    if !expires_soon(&tokens, crate::config::now_ms()) { return Ok(tokens.access_token); }
    let _lock = async_lock().await?;
    // Another process may already have rotated the refresh token while we waited.
    let tokens = load()?.context("Anthropic OAuth was logged out; log in again")?;
    if !expires_soon(&tokens, crate::config::now_ms()) { return Ok(tokens.access_token); }
    let value = exchange(http, json!({"grant_type":"refresh_token", "client_id":CLIENT_ID, "refresh_token":tokens.refresh_token}), true).await?;
    let mut refreshed = merge_response(&value, Some(&tokens), crate::config::now_ms())?;
    // Commit rotation before optional identity enrichment, so bootstrap failure cannot lose it.
    save(&refreshed)?;
    enrich(http, &mut refreshed, false).await;
    save(&refreshed)?;
    Ok(refreshed.access_token)
}

fn pkce() -> (String, String) {
    // Three v4 UUIDs contain 366 random bits (version/variant bits excluded).
    let verifier = (0..3).map(|_| uuid::Uuid::new_v4().simple().to_string()).collect::<String>();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}
fn parse_code(input: &str, expected_state: &str, require_state: bool) -> Result<String> {
    let input = input.trim();
    let (code, state) = if input.contains("://") {
        let url = reqwest::Url::parse(input).map_err(|_| anyhow::anyhow!("invalid OAuth callback URL"))?;
        ensure!(url.scheme() == "http" && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")) && url.port() == Some(54545) && url.path() == "/callback", "unexpected OAuth callback URL");
        let mut code = None;
        let mut state = None;
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "error" => bail!("Anthropic browser authorization was denied"),
                "code" => { ensure!(code.is_none(), "duplicate OAuth code"); code = Some(value.into_owned()); }
                "state" => { ensure!(state.is_none(), "duplicate OAuth state"); state = Some(value.into_owned()); }
                _ => {}
            }
        }
        let code = code.context("callback omitted authorization code")?;
        ensure!(state.is_some() || code.contains('#'), "OAuth callback omitted state");
        let (code, fragment_state) = split_code(&code);
        if let (Some(a), Some(b)) = (&state, &fragment_state) { ensure!(a == b, "conflicting OAuth state"); }
        (code, fragment_state.or(state))
    } else { split_code(input) };
    if let Some(state) = state { ensure!(state == expected_state, "OAuth state mismatch; restart login"); }
    else { ensure!(!require_state, "OAuth callback omitted state"); }
    ensure!(!code.is_empty() && !code.chars().any(char::is_whitespace), "invalid authorization code");
    Ok(code)
}
fn split_code(code: &str) -> (String, Option<String>) {
    match code.split_once('#') { Some((code, state)) => (code.into(), Some(state.into())), None => (code.into(), None) }
}
async fn callback(listener: tokio::net::TcpListener, state: &str) -> Result<String> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let mut bytes = Vec::new();
        let read = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let mut chunk = [0; 1024];
                let n = stream.read(&mut chunk).await?;
                if n == 0 { break; }
                bytes.extend_from_slice(&chunk[..n]);
                if bytes.windows(4).any(|w| w == b"\r\n\r\n") || bytes.len() >= 8192 { break; }
            }
            Ok::<_, std::io::Error>(())
        }).await;
        let parsed = if matches!(read, Ok(Ok(()))) && bytes.len() < 8192 {
            std::str::from_utf8(&bytes).ok().and_then(|s| s.lines().next()).and_then(|line| {
                let mut parts = line.split_whitespace();
                if parts.next()? != "GET" { return None; }
                let target = parts.next()?;
                if !target.starts_with("/callback?") { return None; }
                Some(parse_code(&format!("http://localhost:54545{target}"), state, true))
            })
        } else { None };
        let success = matches!(&parsed, Some(Ok(_)));
        let message = if success { "Login received. You can close this browser tab." } else { "Invalid callback. Return to your terminal and retry." };
        let status = if success { "200 OK" } else { "400 Bad Request" };
        let response = format!("HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{message}", message.len());
        let _ = tokio::time::timeout(Duration::from_secs(2), stream.write_all(response.as_bytes())).await;
        if let Some(Ok(code)) = parsed { return Ok(code); }
    }
}

pub async fn login() -> Result<()> {
    let (verifier, challenge) = pkce();
    let state = uuid::Uuid::new_v4().to_string();
    let mut url = reqwest::Url::parse("https://claude.ai/oauth/authorize")?;
    url.query_pairs_mut().extend_pairs([
        ("client_id", CLIENT_ID), ("response_type", "code"), ("redirect_uri", REDIRECT),
        ("scope", "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload"),
        ("code_challenge", &challenge), ("code_challenge_method", "S256"), ("state", &state), ("code", "true"),
    ]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:54545").await.ok();
    println!("Open this URL in your browser:\n{url}\nComplete login, or paste the final callback URL / code#state (a bare code is also accepted):");
    // A detached input thread avoids Tokio's non-cancellable stdin read keeping the CLI alive after a callback.
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut input = String::new();
        let result = std::io::stdin().read_line(&mut input).map(|n| if n == 0 { None } else { Some(input) });
        let _ = tx.send(result);
    });
    let code = tokio::time::timeout(Duration::from_secs(600), async {
        if let Some(listener) = listener {
            let callback = callback(listener, &state);
            tokio::pin!(callback);
            tokio::select! {
                result = &mut callback => result,
                input = &mut rx => {
                    match input.context("OAuth input thread failed")?.context("read authorization code")? {
                        Some(input) => parse_code(&input, &state, false),
                        None => callback.await,
                    }
                }
            }
        } else {
            let input = rx.await.context("OAuth input thread failed")?.context("read authorization code")?.context("OAuth input closed; rerun login with a terminal")?;
            parse_code(&input, &state, false)
        }
    }).await.context("Anthropic login timed out; restart login")??;
    let http = reqwest::Client::builder().build().context("create OAuth HTTP client")?;
    let value = exchange(&http, json!({"grant_type":"authorization_code", "client_id":CLIENT_ID, "code":code,
        "redirect_uri":REDIRECT, "code_verifier":verifier, "state":state}), false).await?;
    let mut tokens = merge_response(&value, None, crate::config::now_ms())?;
    enrich(&http, &mut tokens, true).await;
    let _lock = async_lock().await?;
    save(&tokens)?;
    println!("Anthropic OAuth login saved. Configured API keys still take priority.");
    Ok(())
}
pub fn logout() -> Result<()> {
    let _lock = lock_store()?;
    match fs::remove_file(store_path()) {
        Ok(()) => { File::open(crate::config::config_dir())?.sync_all()?; Ok(()) }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("remove Anthropic OAuth credentials"),
    }
}
pub fn status() -> Result<Option<String>> {
    Ok(load()?.map(|tokens| {
        let account = tokens.email.or(tokens.account_id).unwrap_or_else(|| "account identity unavailable".into());
        let org = tokens.org_name.or(tokens.org_id).map(|name| format!("; organization: {name}")).unwrap_or_default();
        let expiry = if tokens.expires_at_ms <= crate::config::now_ms() { "expired; refresh on next request" } else { "saved; refresh automatically before expiry" };
        format!("Anthropic OAuth: {account}{org}; {expiry}")
    }))
}
/// Stable, sci-pi-owned installation identity, and the saved account UUID (never tokens).
pub fn wire_identity() -> Result<(String, Option<String>)> {
    // Never wait on a credential refresh's network request from a Tokio worker.
    let _lock = lock_file("anthropic-installation.lock")?;
    let path = crate::config::config_dir().join("anthropic-installation-id");
    let id = match fs::read_to_string(&path) {
        Ok(id) => { uuid::Uuid::parse_str(id.trim()).context("invalid Anthropic installation identity")?; fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?; id.trim().to_owned() }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => { let id = uuid::Uuid::new_v4().to_string(); atomic_write(&path, id.as_bytes())?; id }
        Err(e) => return Err(e).context("read Anthropic installation identity"),
    };
    Ok((id, load()?.and_then(|tokens| tokens.account_id)))
}

#[cfg(test)]
mod tests {
    #[test]
    fn atomic_storage_replaces_tokens_privately() {
        let dir = std::env::temp_dir().join(format!("sci-pi-oauth-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("tokens.json");
        atomic_write(&path, b"old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        atomic_write(&path, b"rotated").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"rotated");
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_file(path).unwrap();
        fs::remove_dir(dir).unwrap();
    }
    use super::*;
    #[tokio::test]
    async fn refresh_exchange_uses_real_http_and_preserves_rotation() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let body = loop {
                let mut buffer = [0; 1024];
                let n = stream.read(&mut buffer).await.unwrap();
                assert!(n > 0);
                request.extend_from_slice(&buffer[..n]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&request[..end]).unwrap().to_lowercase();
                    let len: usize = headers.lines().find_map(|line| line.strip_prefix("content-length: ")).unwrap().parse().unwrap();
                    if request.len() < end + 4 + len { continue; }
                    assert!(headers.contains("anthropic-beta: oauth-2025-04-20"));
                    assert!(headers.contains("user-agent: anthropic-sdk-typescript/0.112.1 useroauthprovider"));
                    break serde_json::from_slice::<Value>(&request[end + 4..end + 4 + len]).unwrap();
                }
            };
            assert_eq!(body["grant_type"], "refresh_token");
            assert_eq!(body["refresh_token"], "old-refresh");
            assert_eq!(body["client_id"], CLIENT_ID);
            let body = r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#;
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let old = merge_response(&json!({"access_token":"old-access","refresh_token":"old-refresh","expires_in":3600,
            "account":{"uuid":"account","email_address":"email"},"organization":{"uuid":"org"}}), None, 0).unwrap();
        let value = exchange_at(&reqwest::Client::builder().no_proxy().build().unwrap(),
            json!({"grant_type":"refresh_token","client_id":CLIENT_ID,"refresh_token":old.refresh_token}), true, &url).await.unwrap();
        let refreshed = merge_response(&value, Some(&old), 1000).unwrap();
        assert_eq!(refreshed.access_token, "new-access");
        assert_eq!(refreshed.refresh_token, "new-refresh");
        assert_eq!(refreshed.account_id.as_deref(), Some("account"));
        assert_eq!(refreshed.org_id.as_deref(), Some("org"));
        server.await.unwrap();
    }
    #[test]
    fn callback_state_and_code_conventions() {
        assert_eq!(parse_code("http://localhost:54545/callback?code=abc&state=expected", "expected", true).unwrap(), "abc");
        assert_eq!(parse_code("abc#expected", "expected", false).unwrap(), "abc");
        assert_eq!(parse_code("abc", "expected", false).unwrap(), "abc");
        for input in ["abc#wrong", "abc#", "http://localhost:54545/callback?code=abc", "http://localhost:54545/callback?code=abc&state=wrong", "http://localhost:54545/callback?code=abc%23wrong&state=expected", "http://localhost:54545/callback?code=abc&state=expected&state=expected", "https://evil.example/callback?code=abc&state=expected"] {
            assert!(parse_code(input, "expected", true).is_err());
        }
    }
    #[test]
    fn refresh_preserves_identity_and_rotates_tokens() {
        let old = merge_response(&json!({"access_token":"old", "refresh_token":"refresh", "expires_in":3600,
            "account":{"uuid":"account","email_address":"email"}, "organization":{"uuid":"org","name":"name"}}), None, 1000).unwrap();
        let new = merge_response(&json!({"access_token":"new", "refresh_token":"rotated", "expires_in":3600, "organization":{"uuid":"other"}}), Some(&old), 2000).unwrap();
        assert_eq!(new.access_token, "new"); assert_eq!(new.refresh_token, "rotated");
        assert_eq!(new.account_id, old.account_id); assert_eq!(new.org_id, old.org_id);
        let retained = merge_response(&json!({"access_token":"new", "expires_in":3600}), Some(&old), 2000).unwrap();
        assert_eq!(retained.refresh_token, "refresh");
        assert!(!expires_soon(&new, new.expires_at_ms - SKEW_MS - 1));
        assert!(expires_soon(&new, new.expires_at_ms - SKEW_MS));
        assert!(merge_response(&json!({"access_token":"new", "expires_in":0}), Some(&old), 2000).is_err());
        assert!(merge_response(&json!({"access_token":"new", "expires_in":i64::MAX}), Some(&old), 2000).is_err());
    }
}
