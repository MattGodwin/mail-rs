//! Account configuration and provider presets.
//!
//! Non-secret settings live in a JSON file in the platform config directory.
//! Passwords and OAuth refresh tokens never touch this file — see `secrets.rs`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Security {
    /// Implicit TLS (IMAPS 993, SMTPS 465).
    Tls,
    /// Plain connection upgraded with STARTTLS (usually SMTP 587). TLS is required.
    StartTls,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthKind {
    /// Password or app-specific password.
    Password,
    /// OAuth 2.0 (XOAUTH2) against Google.
    OAuthGoogle,
    /// OAuth 2.0 (XOAUTH2) against Microsoft (Outlook.com, Hotmail, Microsoft 365).
    OAuthMicrosoft,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Server {
    pub host: String,
    pub port: u16,
    pub security: Security,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    /// Display name used in the From header.
    pub name: String,
    pub email: String,
    /// Login user name (usually the email address).
    pub username: String,
    pub imap: Server,
    pub smtp: Server,
    pub auth: AuthKind,
    #[serde(default)]
    pub oauth_client_id: String,
    #[serde(default)]
    pub oauth_client_secret: String,
    /// Append sent mail to the Sent folder over IMAP. Off for providers that
    /// already store a copy of everything sent through their SMTP server.
    #[serde(default)]
    pub save_sent: bool,
    #[serde(default)]
    pub signature: String,
    /// Paused accounts keep their cache and credentials but hold no connections.
    #[serde(default)]
    pub paused: bool,
    /// Show desktop notifications for new mail in this account's inbox.
    #[serde(default = "yes")]
    pub notify: bool,
    /// Extra sending identities for this mailbox ("Name <addr>" or "addr").
    #[serde(default)]
    pub aliases: Vec<String>,
}

fn yes() -> bool {
    true
}

impl Account {
    /// The primary identity followed by aliases, formatted for a From field.
    pub fn identities(&self) -> Vec<String> {
        let primary = if self.name.trim().is_empty() { self.email.clone() } else { format!("{} <{}>", self.name.trim(), self.email) };
        let mut v = vec![primary];
        v.extend(self.aliases.iter().map(|a| a.trim().to_string()).filter(|a| !a.is_empty()));
        v
    }

    /// Bare lowercase addresses this mailbox sends or receives as.
    pub fn own_addresses(&self) -> Vec<String> {
        self.identities().iter().map(|i| crate::compose::bare(i)).collect()
    }

    pub fn label(&self) -> &str {
        if self.email.is_empty() {
            &self.name
        } else {
            &self.email
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub poll_secs: u64,
    pub mark_read_on_open: bool,
    /// How many of the newest messages to fetch per folder on first sync / "load more".
    pub initial_fetch: u32,
    pub google_client_id: String,
    pub google_client_secret: String,
    pub microsoft_client_id: String,
    /// Desktop notifications for new inbox mail.
    pub notifications: bool,
    /// Include sender and subject in notifications.
    pub notify_preview: bool,
    /// Account id used for new messages from the unified view.
    pub default_account: String,
    /// Download and install new releases automatically.
    pub auto_update: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            poll_secs: 30,
            mark_read_on_open: true,
            initial_fetch: 200,
            google_client_id: String::new(),
            google_client_secret: String::new(),
            microsoft_client_id: String::new(),
            notifications: true,
            notify_preview: true,
            default_account: String::new(),
            auto_update: true,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub accounts: Vec<Account>,
    #[serde(default)]
    pub settings: Settings,
}

fn project_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", "mail-rs")
}

pub fn config_dir() -> PathBuf {
    project_dirs().map(|d| d.config_dir().to_path_buf()).unwrap_or_else(|| PathBuf::from(".mail-rs"))
}

pub fn cache_dir() -> PathBuf {
    project_dirs().map(|d| d.cache_dir().to_path_buf()).unwrap_or_else(|| PathBuf::from(".mail-rs/cache"))
}

fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

impl Config {
    pub fn load() -> Config {
        std::fs::read(config_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let json = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        crate::store::write_atomic(&config_path(), &json).map_err(|e| format!("Saving config: {e}"))
    }
}

pub fn new_account_id() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let mut r = [0u8; 4];
    let _ = getrandom::fill(&mut r);
    format!("{:x}{:08x}", nanos, u32::from_le_bytes(r))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Gmail,
    Outlook,
    ICloud,
    Yahoo,
    Fastmail,
    Custom,
}

impl Provider {
    pub const ALL: [Provider; 6] =
        [Provider::Gmail, Provider::Outlook, Provider::ICloud, Provider::Yahoo, Provider::Fastmail, Provider::Custom];

    pub fn label(self) -> &'static str {
        match self {
            Provider::Gmail => "Gmail / Google Workspace",
            Provider::Outlook => "Outlook.com / Hotmail / Microsoft 365",
            Provider::ICloud => "iCloud Mail",
            Provider::Yahoo => "Yahoo Mail",
            Provider::Fastmail => "Fastmail",
            Provider::Custom => "Other (IMAP / SMTP)",
        }
    }

    /// Setup steps and helpful links, shown in the account editor.
    pub fn guide(self, auth: AuthKind) -> (&'static str, &'static [(&'static str, &'static str)]) {
        match (self, auth) {
            (_, AuthKind::OAuthMicrosoft) | (Provider::Outlook, _) => (
                "Microsoft only allows OAuth sign-in. Register an app once: choose \"Accounts in any organizational directory and personal Microsoft accounts\", add the redirect URI http://localhost under \"Mobile and desktop applications\", and add the delegated permissions IMAP.AccessAsUser.All, SMTP.Send and offline_access. Paste the Application (client) ID below.",
                &[("Register an app in Microsoft Entra", "https://entra.microsoft.com/#view/Microsoft_AAD_RegisteredApps/ApplicationsListBlade")],
            ),
            (_, AuthKind::OAuthGoogle) => (
                "Google sign-in needs your own OAuth client: in Google Cloud, enable the Gmail API, add yourself as a test user on the consent screen with the scope https://mail.google.com/, then create an OAuth client of type \"Desktop app\" and paste its ID and secret below. Simpler: switch Sign-in to \"Password / app password\".",
                &[("Google Cloud credentials", "https://console.cloud.google.com/apis/credentials"), ("Enable the Gmail API", "https://console.cloud.google.com/apis/library/gmail.googleapis.com")],
            ),
            (Provider::Gmail, _) => (
                "Use a Google app password (needs 2-Step Verification): create one and paste the 16-character code as the password. Some work and school accounts can't create app passwords; use Google (OAuth) sign-in for those.",
                &[("Create an app password", "https://myaccount.google.com/apppasswords"), ("2-Step Verification", "https://myaccount.google.com/signinoptions/two-step-verification")],
            ),
            (Provider::ICloud, _) => (
                "Use an app-specific password generated in your Apple Account settings (requires two-factor authentication).",
                &[("Apple Account sign-in & security", "https://account.apple.com/account/manage")],
            ),
            (Provider::Yahoo, _) => (
                "Use an app password generated in Yahoo account security settings.",
                &[("Yahoo account security", "https://login.yahoo.com/account/security")],
            ),
            (Provider::Fastmail, _) => (
                "Use an app password from Fastmail Settings > Privacy & Security > Manage app passwords and access.",
                &[("Fastmail settings", "https://app.fastmail.com/settings/security")],
            ),
            (Provider::Custom, _) => ("Enter your provider's IMAP and SMTP server details; your provider's help pages list them.", &[]),
        }
    }

    /// (imap, smtp, default auth, save_sent)
    pub fn preset(self) -> (Server, Server, AuthKind, bool) {
        let s = |host: &str, port, security| Server { host: host.to_string(), port, security };
        use Security::*;
        match self {
            Provider::Gmail => (s("imap.gmail.com", 993, Tls), s("smtp.gmail.com", 465, Tls), AuthKind::OAuthGoogle, false),
            Provider::Outlook => {
                (s("outlook.office365.com", 993, Tls), s("smtp.office365.com", 587, StartTls), AuthKind::OAuthMicrosoft, false)
            }
            Provider::ICloud => (s("imap.mail.me.com", 993, Tls), s("smtp.mail.me.com", 587, StartTls), AuthKind::Password, true),
            Provider::Yahoo => (s("imap.mail.yahoo.com", 993, Tls), s("smtp.mail.yahoo.com", 465, Tls), AuthKind::Password, true),
            Provider::Fastmail => (s("imap.fastmail.com", 993, Tls), s("smtp.fastmail.com", 465, Tls), AuthKind::Password, true),
            Provider::Custom => (s("", 993, Tls), s("", 465, Tls), AuthKind::Password, true),
        }
    }

    /// Guess the provider from an email address domain.
    pub fn guess(email: &str) -> Option<Provider> {
        let domain = email.rsplit('@').next()?.to_ascii_lowercase();
        Some(match domain.as_str() {
            "gmail.com" | "googlemail.com" => Provider::Gmail,
            "outlook.com" | "hotmail.com" | "live.com" | "msn.com" | "hotmail.co.uk" | "outlook.co.uk" | "live.co.uk" => Provider::Outlook,
            "icloud.com" | "me.com" | "mac.com" => Provider::ICloud,
            "yahoo.com" | "ymail.com" | "yahoo.co.uk" | "rocketmail.com" => Provider::Yahoo,
            "fastmail.com" | "fastmail.fm" => Provider::Fastmail,
            _ => return None,
        })
    }
}
