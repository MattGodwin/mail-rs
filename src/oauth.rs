//! OAuth 2.0 for providers that no longer accept passwords over IMAP/SMTP
//! (Microsoft) or prefer not to (Google). Uses the installed-app flow:
//! authorization code + PKCE with a loopback redirect, then XOAUTH2.
//!
//! You must supply your own OAuth client ID (free to register); see README.

use crate::config::{Account, AuthKind};
use crate::secrets;
use base64::Engine;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

struct Endpoints {
    auth: &'static str,
    token: &'static str,
    scope: &'static str,
    redirect_host: &'static str,
}

const GOOGLE: Endpoints = Endpoints {
    auth: "https://accounts.google.com/o/oauth2/v2/auth",
    token: "https://oauth2.googleapis.com/token",
    scope: "https://mail.google.com/",
    redirect_host: "127.0.0.1",
};

const MICROSOFT: Endpoints = Endpoints {
    auth: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
    token: "https://login.microsoftonline.com/common/oauth2/v2.0/token",
    scope: "https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send offline_access",
    redirect_host: "localhost",
};

fn endpoints(kind: AuthKind) -> Result<&'static Endpoints, String> {
    match kind {
        AuthKind::OAuthGoogle => Ok(&GOOGLE),
        AuthKind::OAuthMicrosoft => Ok(&MICROSOFT),
        AuthKind::Password => Err("Account does not use OAuth".into()),
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    refresh_token: Option<String>,
}

fn token_cache() -> &'static Mutex<HashMap<String, (String, Instant)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (String, Instant)>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// One lock per account so the IMAP worker, the push watcher and SMTP never
/// refresh (and rotate) the same refresh token concurrently.
fn refresh_lock(account_id: &str) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    LOCKS.get_or_init(Default::default).lock().unwrap().entry(account_id.to_string()).or_default().clone()
}

/// Did the provider reject our credentials (as opposed to a network hiccup)?
pub fn is_auth_error(e: &str) -> bool {
    ["invalid_grant", "invalid_client", "unauthorized_client", "No saved credential", "HTTP 400", "HTTP 401"].iter().any(|p| e.contains(p))
}

/// Format check so obvious paste mistakes are caught before opening the browser.
pub fn validate_client_id(kind: AuthKind, id: &str) -> Result<(), String> {
    let id = id.trim();
    match kind {
        AuthKind::OAuthMicrosoft => {
            let parts: Vec<&str> = id.split('-').collect();
            let ok = parts.len() == 5
                && [8usize, 4, 4, 4, 12].iter().zip(&parts).all(|(n, p)| p.len() == *n && p.chars().all(|c| c.is_ascii_hexdigit()));
            if ok {
                Ok(())
            } else {
                Err("A Microsoft client ID looks like 1a2b3c4d-1234-abcd-9876-0123456789ab (the \"Application (client) ID\").".into())
            }
        }
        AuthKind::OAuthGoogle => {
            if id.ends_with(".apps.googleusercontent.com") {
                Ok(())
            } else {
                Err("A Google client ID ends with .apps.googleusercontent.com".into())
            }
        }
        AuthKind::Password => Ok(()),
    }
}

fn b64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

fn random_token() -> String {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("OS random source unavailable");
    b64url(&b)
}

fn post_token(url: &str, form: &[(&str, &str)]) -> Result<TokenResponse, String> {
    let mut resp = crate::http::agent(Duration::from_secs(30))
        .post(url)
        .send_form(form.iter().copied())
        .map_err(|e| format!("Token request failed: {e}"))?;
    crate::http::ensure_ok(&mut resp, "Token endpoint")?;
    resp.body_mut().read_json::<TokenResponse>().map_err(|e| format!("Invalid token response: {e}"))
}

fn cache_token(account_id: &str, t: &TokenResponse) {
    let ttl = Duration::from_secs(t.expires_in.unwrap_or(3600));
    token_cache().lock().unwrap().insert(account_id.to_string(), (t.access_token.clone(), Instant::now() + ttl));
}

pub fn forget(account_id: &str) {
    token_cache().lock().unwrap().remove(account_id);
}

/// Interactive sign-in: opens the system browser and waits (up to 5 minutes)
/// for the redirect. Returns the refresh token.
pub fn authorize(kind: AuthKind, client_id: &str, client_secret: &str, login_hint: &str) -> Result<String, String> {
    let ep = endpoints(kind)?;
    if client_id.trim().is_empty() {
        return Err("An OAuth client ID is required; follow the steps above to create one.".into());
    }
    validate_client_id(kind, client_id)?;
    let verifier = random_token();
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    let state = random_token();

    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let redirect = format!("http://{}:{port}", ep.redirect_host);

    let mut params = vec![
        ("client_id", client_id.to_string()),
        ("response_type", "code".into()),
        ("redirect_uri", redirect.clone()),
        ("scope", ep.scope.into()),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256".into()),
        ("state", state.clone()),
    ];
    if !login_hint.is_empty() {
        params.push(("login_hint", login_hint.into()));
    }
    if kind == AuthKind::OAuthGoogle {
        params.push(("access_type", "offline".into()));
        params.push(("prompt", "consent".into()));
    }
    let url = url::Url::parse_with_params(ep.auth, &params).map_err(|e| e.to_string())?;
    open::that(url.as_str()).map_err(|e| format!("Could not open browser: {e}"))?;

    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(300);
    let code = loop {
        if Instant::now() > deadline {
            return Err("Timed out waiting for browser sign-in".into());
        }
        let mut stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(e) => return Err(e.to_string()),
        };
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut buf = vec![0u8; 8192];
        let mut len = 0;
        while len < buf.len() {
            match stream.read(&mut buf[len..]) {
                Ok(0) => break,
                Ok(n) => {
                    len += n;
                    if buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let req = String::from_utf8_lossy(&buf[..len]);
        let path = req.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("/");
        let parsed = url::Url::parse(&format!("http://localhost{path}")).ok();
        let q: HashMap<String, String> = parsed.map(|u| u.query_pairs().into_owned().collect()).unwrap_or_default();
        let (status, body, result) = if let Some(err) = q.get("error") {
            let desc = q.get("error_description").cloned().unwrap_or_default();
            ("400 Bad Request", "Sign-in failed. You can close this tab.", Some(Err(format!("{err}: {desc}"))))
        } else if let Some(code) = q.get("code") {
            if q.get("state") != Some(&state) {
                ("400 Bad Request", "State mismatch. You can close this tab.", Some(Err("OAuth state mismatch".to_string())))
            } else {
                ("200 OK", "Signed in. You can close this tab and return to Mail.", Some(Ok(code.clone())))
            }
        } else {
            ("404 Not Found", "", None)
        };
        let html = format!(
            "<!doctype html><meta charset=utf-8><title>Mail</title><body style=\"font-family:system-ui;padding:3em\"><p>{body}</p>"
        );
        let _ = write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
            html.len()
        );
        match result {
            Some(r) => break r?,
            None => continue,
        }
    };

    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("client_id", client_id),
        ("code_verifier", verifier.as_str()),
    ];
    if !client_secret.is_empty() {
        form.push(("client_secret", client_secret));
    }
    let t = post_token(ep.token, &form)?;
    t.refresh_token.ok_or_else(|| "Provider did not return a refresh token".to_string())
}

/// A valid access token for the account, refreshing it when needed.
pub fn access_token(acct: &Account) -> Result<String, String> {
    let lock = refresh_lock(&acct.id);
    let _guard = lock.lock().unwrap();
    if let Some((tok, exp)) = token_cache().lock().unwrap().get(&acct.id) {
        if *exp > Instant::now() + Duration::from_secs(60) {
            return Ok(tok.clone());
        }
    }
    let ep = endpoints(acct.auth)?;
    let refresh = secrets::get(&acct.id, secrets::REFRESH)?;
    let mut form = vec![("grant_type", "refresh_token"), ("refresh_token", refresh.as_str()), ("client_id", acct.oauth_client_id.as_str())];
    if !acct.oauth_client_secret.is_empty() {
        form.push(("client_secret", acct.oauth_client_secret.as_str()));
    }
    if acct.auth == AuthKind::OAuthMicrosoft {
        form.push(("scope", ep.scope));
    }
    let t = post_token(ep.token, &form)?;
    if let Some(new_refresh) = &t.refresh_token {
        if *new_refresh != refresh {
            let _ = secrets::set(&acct.id, secrets::REFRESH, new_refresh);
        }
    }
    cache_token(&acct.id, &t);
    Ok(t.access_token)
}
