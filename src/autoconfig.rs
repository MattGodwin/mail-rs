//! Finding server settings from an email address, the way Thunderbird does:
//! Mozilla's ISP database, the domain's own autoconfig file, then the
//! domain's MX records (to spot Google Workspace / Microsoft 365 hosting).
//! Only the domain is sent anywhere; never the address or credentials.

use crate::config::{Provider, Security, Server};
use std::sync::OnceLock;
use std::time::Duration;

pub enum Found {
    /// A known provider (from the domain or its mail hosting).
    Provider(Provider, &'static str),
    /// Server settings from an autoconfig document; `local_user` = the login is
    /// the part before '@' rather than the full address.
    Servers { imap: Server, smtp: Server, local_user: bool, source: &'static str },
}

fn domain(email: &str) -> Option<String> {
    let d = email.rsplit_once('@')?.1.trim().to_ascii_lowercase();
    (!d.is_empty() && d.contains('.') && d.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')).then_some(d)
}

fn fetch(url: &str) -> Option<String> {
    let agent = crate::http::agent(Duration::from_secs(6));
    let mut resp = agent.get(url).header("Accept", "application/xml, application/dns-json, */*").call().ok()?;
    if !(200..300).contains(&resp.status().as_u16()) {
        return None;
    }
    resp.body_mut().with_config().limit(256 * 1024).read_to_string().ok()
}

fn tag<'a>(block: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = block.find(&open)? + open.len();
    let end = block[start..].find(&format!("</{name}>"))? + start;
    Some(block[start..end].trim())
}

fn server_block<'a>(xml: &'a str, kind: &str, typ: &str) -> Option<&'a str> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r#"(?s)<(incomingServer|outgoingServer)\s+type="([a-z0-9]+)"\s*>(.*?)</(?:incomingServer|outgoingServer)>"#)
            .unwrap()
    });
    re.captures_iter(xml).find(|c| &c[1] == kind && &c[2] == typ).map(|c| c.get(3).unwrap().as_str())
}

fn parse_server(block: &str) -> Option<(Server, bool)> {
    let host = tag(block, "hostname")?.to_string();
    let port: u16 = tag(block, "port")?.parse().ok()?;
    let security = match tag(block, "socketType")?.to_ascii_uppercase().as_str() {
        "SSL" | "TLS" => Security::Tls,
        "STARTTLS" => Security::StartTls,
        _ => return None, // never plaintext
    };
    let local = tag(block, "username").is_some_and(|u| u.contains("%EMAILLOCALPART%"));
    Some((Server { host, port, security }, local))
}

/// Parse a Mozilla autoconfig (config-v1.1.xml) document.
pub fn parse_autoconfig(xml: &str) -> Option<(Server, Server, bool)> {
    let (imap, local) = parse_server(server_block(xml, "incomingServer", "imap")?)?;
    let (smtp, _) = parse_server(server_block(xml, "outgoingServer", "smtp")?)?;
    Some((imap, smtp, local))
}

/// Recognise mail hosting from MX host names.
pub fn provider_from_mx(hosts: &[String]) -> Option<Provider> {
    let any = |suffixes: &[&str]| hosts.iter().any(|h| suffixes.iter().any(|s| h.trim_end_matches('.').ends_with(s)));
    if any(&["google.com", "googlemail.com"]) {
        Some(Provider::Gmail)
    } else if any(&["protection.outlook.com", "outlook.com", "hotmail.com"]) {
        Some(Provider::Outlook)
    } else if any(&["icloud.com", "me.com"]) {
        Some(Provider::ICloud)
    } else if any(&["messagingengine.com"]) {
        Some(Provider::Fastmail)
    } else if any(&["yahoodns.net"]) {
        Some(Provider::Yahoo)
    } else {
        None
    }
}

fn mx_hosts(domain: &str) -> Vec<String> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r#""data"\s*:\s*"\d+\s+([^"]+)""#).unwrap());
    let Some(json) = fetch(&format!("https://cloudflare-dns.com/dns-query?name={domain}&type=MX")) else { return vec![] };
    re.captures_iter(&json).map(|c| c[1].to_ascii_lowercase()).collect()
}

/// Blocking lookup; run on a background thread.
pub fn lookup(email: &str) -> Option<Found> {
    if let Some(p) = Provider::guess(email) {
        return Some(Found::Provider(p, "known provider"));
    }
    let d = domain(email)?;
    let urls = [
        (format!("https://autoconfig.thunderbird.net/v1.1/{d}"), "Mozilla's settings database"),
        (format!("https://autoconfig.{d}/mail/config-v1.1.xml"), "your domain"),
        (format!("https://{d}/.well-known/autoconfig/mail/config-v1.1.xml"), "your domain"),
    ];
    for (url, source) in urls {
        if let Some((imap, smtp, local_user)) = fetch(&url).as_deref().and_then(parse_autoconfig) {
            return Some(Found::Servers { imap, smtp, local_user, source });
        }
    }
    provider_from_mx(&mx_hosts(&d)).map(|p| Found::Provider(p, "your domain's mail hosting"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mozilla_autoconfig() {
        let xml = r#"<clientConfig version="1.1"><emailProvider id="example.net">
          <incomingServer type="pop3"><hostname>pop.example.net</hostname><port>995</port><socketType>SSL</socketType></incomingServer>
          <incomingServer type="imap"><hostname>imap.example.net</hostname><port>993</port><socketType>SSL</socketType><username>%EMAILLOCALPART%</username></incomingServer>
          <outgoingServer type="smtp"><hostname>smtp.example.net</hostname><port>587</port><socketType>STARTTLS</socketType></outgoingServer>
        </emailProvider></clientConfig>"#;
        let (imap, smtp, local) = parse_autoconfig(xml).unwrap();
        assert_eq!((imap.host.as_str(), imap.port, imap.security), ("imap.example.net", 993, Security::Tls));
        assert_eq!((smtp.host.as_str(), smtp.port, smtp.security), ("smtp.example.net", 587, Security::StartTls));
        assert!(local);
        // Plaintext servers are never accepted.
        assert!(parse_autoconfig(&xml.replace("STARTTLS", "plain")).is_none());
    }

    #[test]
    fn recognises_hosted_domains() {
        assert_eq!(provider_from_mx(&["aspmx.l.google.com.".into()]), Some(Provider::Gmail));
        assert_eq!(provider_from_mx(&["example-com.mail.protection.outlook.com".into()]), Some(Provider::Outlook));
        assert_eq!(provider_from_mx(&["mx.example.net".into()]), None);
    }
}
