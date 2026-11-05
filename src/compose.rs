//! Composing, replying and forwarding. Outgoing mail is plain text: what you
//! type is exactly what is sent. Attachments are included byte for byte.

use crate::config::Account;
use crate::view::View;
use lettre::address::Envelope;
use lettre::message::header::ContentType;
use lettre::message::{Attachment, Mailbox, MultiPart, SinglePart};
use lettre::Message;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

pub struct Compose {
    /// Runtime id (window identity, event routing).
    pub id: u64,
    /// Stable draft identity, persisted locally and embedded in the draft's Message-ID.
    pub key: String,
    /// How many versions have been saved to the server under `key`.
    pub version: u32,
    pub acct: usize,
    /// Index into the account's identities (0 = primary address).
    pub from: usize,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    pub files: Vec<PathBuf>,
    /// Attachments carried over from a forwarded message or draft: (name, mime, bytes).
    pub carried: Vec<(String, String, Vec<u8>)>,
    pub in_reply_to: Option<String>,
    pub references: Option<String>,
    /// (account index, folder, uid) of the message being replied to.
    pub reply_of: Option<(usize, String, u32)>,
    /// A server draft written by another app that this one replaces: (folder, uidvalidity, uid).
    pub replaces: Option<(String, u32, u32)>,
    pub show_cc: bool,
    pub sending: bool,
    pub error: Option<String>,
    pub note: Option<String>,
    pub focus: bool,
    /// Content hash when last saved (or created); differs => unsaved changes.
    pub clean: u64,
    /// Content hash last written to the local recovery file.
    pub persisted: u64,
    pub carried_dirty: bool,
    /// Server draft save in flight.
    pub saving: Option<u64>,
    pub close_after_save: bool,
    pub recovered: bool,
}

pub fn new_key() -> String {
    let mut b = [0u8; 9];
    let _ = getrandom::fill(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl Compose {
    pub fn new(id: u64, acct: usize, signature: &str) -> Compose {
        let body = if signature.trim().is_empty() { String::new() } else { format!("\n\n-- \n{}", signature.trim_end()) };
        let mut c = Compose {
            id,
            key: new_key(),
            version: 0,
            acct,
            from: 0,
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            subject: String::new(),
            body,
            files: vec![],
            carried: vec![],
            in_reply_to: None,
            references: None,
            reply_of: None,
            replaces: None,
            show_cc: false,
            sending: false,
            error: None,
            note: None,
            focus: true,
            clean: 0,
            persisted: 0,
            carried_dirty: false,
            saving: None,
            close_after_save: false,
            recovered: false,
        };
        c.mark_clean();
        c
    }

    pub fn title(&self) -> String {
        let t = if self.subject.trim().is_empty() { "New message".to_string() } else { self.subject.clone() };
        if self.recovered {
            format!("{t} (recovered)")
        } else {
            t
        }
    }

    pub fn content_hash(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (self.acct, self.from, &self.to, &self.cc, &self.bcc, &self.subject, &self.body, &self.files).hash(&mut h);
        for (n, m, d) in &self.carried {
            (n, m, d.len()).hash(&mut h);
        }
        h.finish()
    }

    /// The current contents count as saved; a new, untouched message is not "dirty".
    pub fn mark_clean(&mut self) {
        self.clean = self.content_hash();
        self.persisted = self.clean;
    }

    pub fn dirty(&self) -> bool {
        self.content_hash() != self.clean
    }

    pub fn draft_msg_id(&self, version: u32) -> String {
        format!("<draft-{}-{}@mail-rs.local>", self.key, version)
    }
}

/// Recognise our own draft Message-IDs: returns (key, version).
pub fn parse_draft_id(message_id: &str) -> Option<(String, u32)> {
    let id = message_id.trim().trim_start_matches('<').trim_end_matches('>');
    let rest = id.strip_prefix("draft-")?.strip_suffix("@mail-rs.local")?;
    let (key, v) = rest.rsplit_once('-')?;
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some((key.to_string(), v.parse().ok()?))
}

fn prefixed(prefix: &str, subject: &str) -> String {
    let lower = subject.trim_start().to_lowercase();
    if lower.starts_with(&format!("{}:", prefix.to_lowercase())) {
        subject.to_string()
    } else {
        format!("{prefix}: {subject}")
    }
}

fn quote(v: &View) -> String {
    let mut q = String::new();
    let mut blank = 0;
    for line in v.text.lines() {
        // Collapse runs of blank lines (common in text converted from HTML).
        blank = if line.trim().is_empty() { blank + 1 } else { 0 };
        if blank > 1 {
            continue;
        }
        q.push_str(if line.starts_with('>') { ">" } else { "> " });
        q.push_str(line);
        q.push('\n');
    }
    q
}

/// Lowercase bare address from "Name <a@b>" or "a@b".
pub fn bare(addr: &str) -> String {
    match (addr.rfind('<'), addr.rfind('>')) {
        (Some(a), Some(b)) if a < b => addr[a + 1..b].trim().to_ascii_lowercase(),
        _ => addr.trim().to_ascii_lowercase(),
    }
}

/// `own`: every address belonging to the user (all accounts and aliases).
pub fn reply(c: &mut Compose, me: &Account, own: &[String], v: &View, all: bool, of: (usize, String, u32)) {
    // Reply from the identity the message was addressed to, if it was an alias.
    let addressed: Vec<String> = v.to_list.iter().chain(&v.cc_list).map(|a| bare(a)).collect();
    if let Some(i) = me.identities().iter().position(|id| addressed.contains(&bare(id))) {
        c.from = i;
    }
    let mut to: Vec<String> = v.reply_to.clone();
    let mut cc: Vec<String> = Vec::new();
    if all {
        to.extend(v.to_list.iter().cloned());
        cc.extend(v.cc_list.iter().cloned());
    }
    let mut seen = std::collections::HashSet::new();
    let mut keep = |a: &String| {
        let b = bare(a);
        // Drop our own addresses on reply-all (a plain reply to our own mail keeps it).
        (!all || !own.contains(&b)) && seen.insert(b)
    };
    to.retain(|a| keep(a));
    cc.retain(|a| keep(a));
    if to.is_empty() && !cc.is_empty() {
        to.push(cc.remove(0));
    }
    c.to = to.join(", ");
    c.cc = cc.join(", ");
    c.show_cc = !c.cc.is_empty();
    c.subject = prefixed("Re", &v.subject);
    if !v.message_id.is_empty() {
        let id = format!("<{}>", v.message_id);
        c.in_reply_to = Some(id.clone());
        let mut refs: Vec<String> = v.references.iter().map(|r| format!("<{r}>")).collect();
        refs.push(id);
        c.references = Some(refs.join(" "));
    }
    let header = format!(
        "On {}, {} wrote:\n",
        if v.date.is_empty() { "an earlier date" } else { &v.date },
        if v.from.is_empty() { "someone" } else { &v.from }
    );
    // Room to type at the top, then the signature (if any), then the quote.
    let sig = if c.body.is_empty() { "\n".to_string() } else { c.body.clone() };
    c.body = format!("{sig}\n\n{header}{}", quote(v));
    c.reply_of = Some(of);
    c.mark_clean();
}

pub fn forward(c: &mut Compose, v: &View, carried: Vec<(String, String, Vec<u8>)>) {
    c.subject = prefixed("Fwd", &v.subject);
    let mut fwd = String::from("---------- Forwarded message ----------\n");
    fwd.push_str(&format!("From: {}\n", v.from));
    if !v.date.is_empty() {
        fwd.push_str(&format!("Date: {}\n", v.date));
    }
    fwd.push_str(&format!("Subject: {}\n", v.subject));
    fwd.push_str(&format!("To: {}\n", v.to));
    if !v.cc.is_empty() {
        fwd.push_str(&format!("Cc: {}\n", v.cc));
    }
    fwd.push('\n');
    fwd.push_str(&v.text);
    c.body = format!("{}\n\n{fwd}", c.body.trim_end_matches('\n'));
    c.carried = carried;
    c.carried_dirty = true;
    c.mark_clean();
}

/// Split an address field on commas/semicolons, respecting quotes and <...>.
pub fn split_addrs(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut in_quote, mut in_angle, mut escaped) = (false, false, false);
    for ch in s.chars() {
        if escaped {
            cur.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_quote => {
                escaped = true;
                cur.push(ch);
            }
            '"' => {
                in_quote = !in_quote;
                cur.push(ch);
            }
            '<' if !in_quote => {
                in_angle = true;
                cur.push(ch);
            }
            '>' if !in_quote => {
                in_angle = false;
                cur.push(ch);
            }
            ',' | ';' if !in_quote && !in_angle => {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

pub fn parse_addrs(field: &str, s: &str) -> Result<Vec<Mailbox>, String> {
    split_addrs(s).into_iter().map(|a| a.parse::<Mailbox>().map_err(|_| format!("{field}: \"{a}\" is not a valid address"))).collect()
}

fn from_mailbox(acct: &Account, c: &Compose) -> Result<Mailbox, String> {
    let ids = acct.identities();
    let id = ids.get(c.from).unwrap_or(&ids[0]);
    id.parse::<Mailbox>().map_err(|_| format!("Sending identity \"{id}\" is not a valid address"))
}

/// Build the message. `draft` messages may have no recipients, keep their Bcc
/// header, and carry a Message-ID that identifies every saved version.
pub fn build(acct: &Account, c: &Compose, draft: Option<u32>) -> Result<Message, String> {
    let from = from_mailbox(acct, c)?;
    let to = parse_addrs("To", &c.to)?;
    let cc = parse_addrs("Cc", &c.cc)?;
    let bcc = parse_addrs("Bcc", &c.bcc)?;
    if draft.is_none() && to.is_empty() && cc.is_empty() && bcc.is_empty() {
        return Err("Add at least one recipient".into());
    }
    let mut b = Message::builder().from(from.clone()).subject(c.subject.clone()).date_now();
    for m in to {
        b = b.to(m);
    }
    for m in cc {
        b = b.cc(m);
    }
    for m in bcc {
        b = b.bcc(m);
    }
    if let Some(id) = &c.in_reply_to {
        b = b.in_reply_to(id.clone());
    }
    if let Some(r) = &c.references {
        b = b.references(r.clone());
    }
    if let Some(version) = draft {
        // Never transmitted: the envelope only satisfies the builder.
        let env = Envelope::new(Some(from.email.clone()), vec![from.email.clone()]).map_err(|e| e.to_string())?;
        b = b.message_id(Some(c.draft_msg_id(version))).envelope(env).keep_bcc();
    }
    let text = SinglePart::plain(c.body.clone());
    if c.files.is_empty() && c.carried.is_empty() {
        return b.singlepart(text).map_err(|e| e.to_string());
    }
    let mut mp = MultiPart::mixed().singlepart(text);
    for path in &c.files {
        let data = std::fs::read(path).map_err(|e| format!("Reading {}: {e}", path.display()))?;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "attachment".into());
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        let ct = ContentType::parse(mime.as_ref()).unwrap_or_else(|_| ContentType::parse("application/octet-stream").unwrap());
        mp = mp.singlepart(Attachment::new(name).body(data, ct));
    }
    for (name, mime, data) in &c.carried {
        let ct = ContentType::parse(mime).unwrap_or_else(|_| ContentType::parse("application/octet-stream").unwrap());
        mp = mp.singlepart(Attachment::new(name.clone()).body(data.clone(), ct));
    }
    b.multipart(mp).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuthKind, Security, Server};

    fn acct() -> Account {
        let s = Server { host: "h".into(), port: 1, security: Security::Tls };
        Account {
            id: "a".into(),
            name: "Alex".into(),
            email: "alex@example.com".into(),
            username: "alex@example.com".into(),
            imap: s.clone(),
            smtp: s,
            auth: AuthKind::Password,
            oauth_client_id: String::new(),
            oauth_client_secret: String::new(),
            save_sent: true,
            signature: String::new(),
            paused: false,
            notify: true,
            aliases: vec!["Alex Work <alex@work.example>".into()],
        }
    }

    #[test]
    fn splits_addresses() {
        assert_eq!(split_addrs("\"Doe, Jane\" <j@x.org>, bob@y.org; <c@z.org>"), vec!["\"Doe, Jane\" <j@x.org>", "bob@y.org", "<c@z.org>"]);
        assert!(parse_addrs("To", "\"Doe, Jane\" <j@x.org>, bob@y.org").is_ok());
        assert!(parse_addrs("To", "not an address").is_err());
    }

    #[test]
    fn subject_prefix() {
        assert_eq!(prefixed("Re", "Hello"), "Re: Hello");
        assert_eq!(prefixed("Re", "RE: Hello"), "RE: Hello");
    }

    #[test]
    fn drafts_keep_bcc_and_need_no_recipients() {
        let a = acct();
        let mut c = Compose::new(1, 0, "");
        c.bcc = "secret@example.org".into();
        c.subject = "Plan".into();
        let raw = String::from_utf8(build(&a, &c, Some(3)).unwrap().formatted()).unwrap();
        assert!(raw.contains("Bcc: secret@example.org"));
        assert!(raw.contains(&format!("Message-ID: <draft-{}-3@mail-rs.local>", c.key)));
        assert_eq!(parse_draft_id(&format!("draft-{}-3@mail-rs.local", c.key)), Some((c.key.clone(), 3)));

        let empty = Compose::new(2, 0, "");
        assert!(build(&a, &empty, Some(1)).is_ok());
        assert!(build(&a, &empty, None).is_err());
        // Sent mail never exposes Bcc.
        let raw = String::from_utf8(build(&a, &c, None).unwrap().formatted()).unwrap();
        assert!(!raw.contains("Bcc:"));
    }

    #[test]
    fn sends_from_selected_alias() {
        let a = acct();
        let mut c = Compose::new(1, 0, "");
        c.to = "x@y.org".into();
        c.from = 1;
        let raw = String::from_utf8(build(&a, &c, None).unwrap().formatted()).unwrap();
        assert!(raw.contains("alex@work.example"), "{raw}");
        assert!(!raw.contains("From: Alex <alex@example.com>"));
    }

    #[test]
    fn untouched_compose_is_clean() {
        let mut c = Compose::new(1, 0, "sig");
        assert!(!c.dirty());
        c.body.push('x');
        assert!(c.dirty());
    }
}
