//! Small HTTPS client setup shared by OAuth and the updater.
//! Uses the platform TLS stack (SChannel / Security.framework / OpenSSL).

use std::time::Duration;

pub fn agent(timeout: Duration) -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder().provider(ureq::tls::TlsProvider::NativeTls).build();
    ureq::Agent::config_builder()
        .tls_config(tls)
        .timeout_global(Some(timeout))
        // We read error bodies ourselves (OAuth errors explain what went wrong).
        .http_status_as_error(false)
        .user_agent(concat!("mail-rs/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// Fail with the status (and a short body excerpt) for non-2xx responses.
pub fn ensure_ok(resp: &mut ureq::http::Response<ureq::Body>, what: &str) -> Result<(), String> {
    let status = resp.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(());
    }
    let body = resp.body_mut().with_config().limit(64 * 1024).read_to_string().unwrap_or_default();
    let body: String = body.chars().take(500).collect();
    Err(format!("{what} returned HTTP {status}: {body}"))
}
