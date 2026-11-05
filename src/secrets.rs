//! Credentials are stored in the OS keychain (Windows Credential Manager,
//! macOS Keychain, Linux kernel keyring) — never in the config file.

const SERVICE: &str = "mail-rs";

fn entry(account_id: &str, kind: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, &format!("{account_id}:{kind}")).map_err(|e| e.to_string())
}

pub fn set(account_id: &str, kind: &str, secret: &str) -> Result<(), String> {
    entry(account_id, kind)?.set_password(secret).map_err(|e| format!("Could not store credential in the OS keychain: {e}"))
}

pub fn get(account_id: &str, kind: &str) -> Result<String, String> {
    entry(account_id, kind)?.get_password().map_err(|e| match e {
        keyring::Error::NoEntry => "No saved credential for this account; edit the account to sign in again".to_string(),
        e => format!("Could not read credential from the OS keychain: {e}"),
    })
}

pub fn delete(account_id: &str, kind: &str) {
    if let Ok(e) = entry(account_id, kind) {
        let _ = e.delete_credential();
    }
}

pub const PASSWORD: &str = "password";
pub const REFRESH: &str = "refresh";
