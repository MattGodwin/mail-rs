//! Opening an authenticated IMAP session. Shared by the per-account worker
//! and the IDLE (push) watcher.

use crate::config::{Account, AuthKind, Security};
use crate::{oauth, secrets};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::time::Duration;

pub type Session = imap::Session<native_tls::TlsStream<TcpStream>>;

#[derive(Clone, Copy, Default, Debug)]
pub struct Caps {
    pub mv: bool,
    pub uidplus: bool,
    pub idle: bool,
}

pub enum ConnErr {
    /// Credentials rejected. Retrying automatically would only risk a lockout.
    Auth(String),
    /// The server refused because too many connections are open.
    Limit(String),
    /// Network, DNS, TLS or a temporary server problem.
    Net(String),
}

impl ConnErr {
    pub fn message(&self) -> &str {
        match self {
            ConnErr::Auth(s) | ConnErr::Limit(s) | ConnErr::Net(s) => s,
        }
    }
}

pub struct Conn {
    pub sess: Session,
    /// A second handle on the same socket, used to force-close a stuck connection
    /// from another thread (shutdown unblocks any pending read).
    pub sock: TcpStream,
    pub caps: Caps,
}

struct XOAuth2 {
    user: String,
    token: String,
}

impl imap::Authenticator for XOAuth2 {
    type Response = String;
    fn process(&self, _challenge: &[u8]) -> String {
        format!("user={}\x01auth=Bearer {}\x01\x01", self.user, self.token)
    }
}

fn is_limit(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("too many") || m.contains("simultaneous") || m.contains("maximum number of connections")
}

pub fn shutdown(sock: &TcpStream) {
    let _ = sock.shutdown(Shutdown::Both);
}

pub fn connect(a: &Account) -> Result<Conn, ConnErr> {
    let host = a.imap.host.trim();
    let port = a.imap.port;
    let addrs: Vec<_> = (host, port).to_socket_addrs().map_err(|e| ConnErr::Net(format!("Could not resolve {host}: {e}")))?.collect();
    let mut last_err = String::from("no addresses");
    let mut tcp = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, Duration::from_secs(15)) {
            Ok(s) => {
                tcp = Some(s);
                break;
            }
            Err(e) => last_err = e.to_string(),
        }
    }
    let tcp = tcp.ok_or_else(|| ConnErr::Net(format!("Could not connect to {host}:{port}: {last_err}")))?;
    let _ = tcp.set_read_timeout(Some(Duration::from_secs(120)));
    let _ = tcp.set_write_timeout(Some(Duration::from_secs(60)));
    let _ = tcp.set_nodelay(true);
    let sock = tcp.try_clone().map_err(|e| ConnErr::Net(e.to_string()))?;
    let net = |m: String| ConnErr::Net(m);
    let tls = native_tls::TlsConnector::new().map_err(|e| net(e.to_string()))?;
    let client = match a.imap.security {
        Security::Tls => {
            let stream = tls.connect(host, tcp).map_err(|e| net(format!("TLS handshake with {host} failed: {e}")))?;
            let mut c = imap::Client::new(stream);
            c.read_greeting().map_err(|e| net(format!("IMAP greeting failed: {e}")))?;
            c
        }
        Security::StartTls => {
            let mut c = imap::Client::new(tcp);
            c.read_greeting().map_err(|e| net(format!("IMAP greeting failed: {e}")))?;
            c.secure(host, &tls).map_err(|e| net(format!("STARTTLS failed: {e}")))?
        }
    };
    let user = a.username.clone();
    let login = match a.auth {
        AuthKind::Password => {
            let pw = secrets::get(&a.id, secrets::PASSWORD).map_err(ConnErr::Auth)?;
            client.login(&user, &pw).map_err(|(e, _)| e)
        }
        AuthKind::OAuthGoogle | AuthKind::OAuthMicrosoft => {
            let token = oauth::access_token(a).map_err(|e| {
                if oauth::is_auth_error(&e) {
                    ConnErr::Auth(format!("Sign-in expired or was revoked: {e}"))
                } else {
                    ConnErr::Net(e)
                }
            })?;
            client.authenticate("XOAUTH2", &XOAuth2 { user: user.clone(), token }).map_err(|(e, _)| e)
        }
    };
    let mut sess = match login {
        Ok(s) => s,
        Err(e @ (imap::Error::No(_) | imap::Error::Bad(_))) => {
            let msg = format!("Sign-in to {host} failed: {e}");
            if is_limit(&msg) {
                return Err(ConnErr::Limit(msg));
            }
            oauth::forget(&a.id);
            return Err(ConnErr::Auth(msg));
        }
        Err(e) => return Err(ConnErr::Net(format!("Sign-in to {host} failed: {e}"))),
    };
    let mut caps = Caps::default();
    if let Ok(c) = sess.capabilities() {
        caps.mv = c.has_str("MOVE");
        caps.uidplus = c.has_str("UIDPLUS");
        caps.idle = c.has_str("IDLE");
    }
    Ok(Conn { sess, sock, caps })
}
