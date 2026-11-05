//! Errors only an organisation's IT admin can fix (Microsoft 365), turned
//! into a plain explanation plus something the user can forward: an
//! admin-consent link or the Exchange Online PowerShell command.

/// The scopes Mail asks for; the consent link requests exactly these.
const SCOPES: &str = "https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send offline_access";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// AADSTS65001 / 90094 / 90095: the organisation must approve the app.
    Consent,
    /// 5.7.139: SMTP AUTH is turned off for the mailbox or tenant.
    SmtpAuthDisabled,
    /// IMAP is turned off for the mailbox.
    ImapDisabled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    pub kind: Kind,
    /// One line, for status rows ("Needs IT admin approval").
    pub short: &'static str,
    /// What happened and what to ask for.
    pub explain: String,
    /// Label for the copy button.
    pub copy_label: &'static str,
    /// The link or command to forward to the admin.
    pub copy_text: String,
}

/// The admin-consent link for a client ID. `organizations` lets the admin
/// sign in to their own tenant; the redirect is the one Mail registers.
pub fn consent_url(client_id: &str) -> String {
    let mut u = url::Url::parse("https://login.microsoftonline.com/organizations/v2.0/adminconsent").unwrap();
    u.query_pairs_mut()
        .append_pair("client_id", client_id.trim())
        .append_pair("scope", SCOPES)
        .append_pair("redirect_uri", "http://localhost");
    u.into()
}

pub fn smtp_command(email: &str) -> String {
    format!("Set-CASMailbox -Identity {} -SmtpClientAuthenticationDisabled $false", identity(email))
}

pub fn imap_command(email: &str) -> String {
    format!("Set-CASMailbox -Identity {} -ImapEnabled $true", identity(email))
}

fn identity(email: &str) -> String {
    let e = email.trim();
    if e.is_empty() {
        "user@yourdomain.com".into()
    } else {
        e.to_string()
    }
}

/// Recognise an admin-only problem in a raw error from sign-in, IMAP or SMTP.
/// `client_id` fills the consent link; `email` fills the commands.
pub fn classify(err: &str, client_id: &str, email: &str) -> Option<Issue> {
    let l = err.to_ascii_lowercase();
    if ["aadsts65001", "aadsts90094", "aadsts90095"].iter().any(|c| l.contains(c)) {
        let (explain, copy_label, copy_text) = if client_id.trim().is_empty() {
            (
                "Your organisation needs to approve Mail before you can sign in. Ask your IT admin to grant admin consent for the app's client ID.".to_string(),
                "Copy instructions",
                "Please grant tenant-wide admin consent for Mail's OAuth app (IMAP.AccessAsUser.All, SMTP.Send, offline_access).".to_string(),
            )
        } else {
            (
                "Your organisation needs to approve Mail before you can sign in. Send this link to your IT admin; once they approve it, sign in again.".to_string(),
                "Copy admin link",
                consent_url(client_id),
            )
        };
        return Some(Issue { kind: Kind::Consent, short: "Needs IT admin approval", explain, copy_label, copy_text });
    }
    if l.contains("5.7.139") || l.contains("smtpclientauthentication is disabled") {
        return Some(Issue {
            kind: Kind::SmtpAuthDisabled,
            short: "Sending blocked by IT admin",
            explain:
                "Your admin needs to enable SMTP AUTH for your mailbox so Mail can send. Send them this Exchange Online PowerShell command:"
                    .into(),
            copy_label: "Copy command",
            copy_text: smtp_command(email),
        });
    }
    if l.contains("user is authenticated but not connected")
        || l.contains("imap is disabled")
        || l.contains("imap access is disabled")
        || l.contains("imap is not enabled")
        || l.contains("imap protocol is disabled")
    {
        return Some(Issue {
            kind: Kind::ImapDisabled,
            short: "IMAP blocked by IT admin",
            explain:
                "Your admin needs to enable IMAP for your mailbox so Mail can read it. Send them this Exchange Online PowerShell command:"
                    .into(),
            copy_label: "Copy command",
            copy_text: imap_command(email),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spots_admin_consent() {
        let e = "access_denied: AADSTS65001: The user or administrator has not consented to use the application";
        let i = classify(e, "11111111-2222-3333-4444-555555555555", "a@contoso.com").unwrap();
        assert_eq!(i.kind, Kind::Consent);
        assert!(i.copy_text.starts_with(
            "https://login.microsoftonline.com/organizations/v2.0/adminconsent?client_id=11111111-2222-3333-4444-555555555555"
        ));
        assert!(i.copy_text.contains("SMTP.Send"));
        for code in ["AADSTS90094", "AADSTS90095"] {
            assert_eq!(classify(&format!("invalid_grant: {code}: Admin approval required"), "x", "").unwrap().kind, Kind::Consent);
        }
    }

    #[test]
    fn spots_disabled_protocols() {
        let smtp = "permanent error (535): 5.7.139 Authentication unsuccessful, SmtpClientAuthentication is disabled for the Mailbox.";
        let i = classify(smtp, "", "jo@contoso.com").unwrap();
        assert_eq!(i.kind, Kind::SmtpAuthDisabled);
        assert_eq!(i.copy_text, "Set-CASMailbox -Identity jo@contoso.com -SmtpClientAuthenticationDisabled $false");
        let imap = "No Response: AUTHENTICATE failed. User is authenticated but not connected.";
        assert_eq!(classify(imap, "", "jo@contoso.com").unwrap().copy_text, "Set-CASMailbox -Identity jo@contoso.com -ImapEnabled $true");
        assert!(classify("535 5.7.8 Username and Password not accepted", "", "").is_none());
    }
}
