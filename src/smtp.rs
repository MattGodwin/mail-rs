//! Outgoing mail over SMTP (implicit TLS or mandatory STARTTLS — never plaintext).

use crate::config::{Account, AuthKind, Security};
use crate::{oauth, secrets};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{SmtpTransport, Transport};
use std::time::Duration;

fn transport(acct: &Account) -> Result<SmtpTransport, String> {
    let host = acct.smtp.host.trim().to_string();
    let tls = TlsParameters::new(host.clone()).map_err(|e| format!("TLS setup failed: {e}"))?;
    let (creds, mechanisms) = match acct.auth {
        AuthKind::Password => {
            (Credentials::new(acct.username.clone(), secrets::get(&acct.id, secrets::PASSWORD)?), vec![Mechanism::Plain, Mechanism::Login])
        }
        AuthKind::OAuthGoogle | AuthKind::OAuthMicrosoft => {
            (Credentials::new(acct.username.clone(), oauth::access_token(acct)?), vec![Mechanism::Xoauth2])
        }
    };
    let transport = SmtpTransport::builder_dangerous(host)
        .port(acct.smtp.port)
        .tls(match acct.smtp.security {
            Security::Tls => Tls::Wrapper(tls),
            Security::StartTls => Tls::Required(tls),
        })
        .credentials(creds)
        .authentication(mechanisms)
        .timeout(Some(Duration::from_secs(60)))
        .build();
    Ok(transport)
}

pub fn send(acct: &Account, msg: &lettre::Message) -> Result<(), String> {
    transport(acct)?.send(msg).map(|_| ()).map_err(|e| format!("Sending failed: {e}"))
}

/// Connect and sign in to the outgoing server without sending anything.
pub fn verify(acct: &Account) -> Result<(), String> {
    match transport(acct)?.test_connection() {
        Ok(true) => Ok(()),
        Ok(false) => Err("The outgoing server closed the connection".into()),
        Err(e) => Err(e.to_string()),
    }
}
