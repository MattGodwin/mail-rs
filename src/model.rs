//! Shared data types and small formatting helpers.

use serde::{Deserialize, Serialize};

/// What the message list needs to know about a message. Built from a small
/// subset of headers so the list can be synced without downloading bodies.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub uid: u32,
    /// Unix timestamp (server arrival time, falling back to the Date header).
    pub date: i64,
    pub from_name: String,
    pub from_addr: String,
    pub to: String,
    pub subject: String,
    #[serde(default)]
    pub message_id: String,
    pub seen: bool,
    pub flagged: bool,
    pub answered: bool,
    #[serde(default)]
    pub draft: bool,
    pub size: u32,
    pub attach: bool,
}

impl Summary {
    pub fn sender(&self) -> &str {
        if self.from_name.trim().is_empty() {
            &self.from_addr
        } else {
            &self.from_name
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Special {
    Inbox,
    Drafts,
    Sent,
    Archive,
    All,
    Junk,
    Trash,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FolderInfo {
    /// Raw (modified UTF-7) mailbox name as the server knows it.
    pub name: String,
    /// Decoded last path component.
    pub display: String,
    pub depth: usize,
    pub special: Option<Special>,
    pub selectable: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FolderIndex {
    pub uidvalidity: u32,
    /// Number of messages in the folder on the server at last sync.
    #[serde(default)]
    pub exists: u32,
    /// Unread messages in the whole folder on the server (not just the cached window).
    #[serde(default)]
    pub unseen: Option<u32>,
    /// Newest first.
    pub msgs: Vec<Summary>,
}

impl FolderIndex {
    pub fn sort(&mut self) {
        self.msgs.sort_unstable_by(|a, b| b.date.cmp(&a.date).then(b.uid.cmp(&a.uid)));
    }
    pub fn min_uid(&self) -> u32 {
        self.msgs.iter().map(|m| m.uid).min().unwrap_or(0)
    }
    pub fn max_uid(&self) -> u32 {
        self.msgs.iter().map(|m| m.uid).max().unwrap_or(0)
    }
    /// Unread count: the server's figure when known, else what's cached.
    pub fn unread(&self) -> usize {
        match self.unseen {
            Some(n) => n as usize,
            None => self.msgs.iter().filter(|m| !m.seen).count(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsgFlag {
    Seen,
    Flagged,
    Answered,
}

impl MsgFlag {
    pub fn imap(self) -> &'static str {
        match self {
            MsgFlag::Seen => "\\Seen",
            MsgFlag::Flagged => "\\Flagged",
            MsgFlag::Answered => "\\Answered",
        }
    }
    pub fn apply(self, s: &mut Summary, on: bool) {
        match self {
            MsgFlag::Seen => s.seen = on,
            MsgFlag::Flagged => s.flagged = on,
            MsgFlag::Answered => s.answered = on,
        }
    }
}

/// Classify a mailbox from its SPECIAL-USE attributes (RFC 6154), falling
/// back to well-known names. Returns (special, came_from_attribute).
pub fn classify(name: &str, delim: &str, attrs: &[String]) -> Option<(Special, bool)> {
    if name.eq_ignore_ascii_case("INBOX") {
        return Some((Special::Inbox, true));
    }
    for a in attrs {
        let s = match a.to_ascii_lowercase().as_str() {
            "\\sent" => Special::Sent,
            "\\drafts" => Special::Drafts,
            "\\trash" => Special::Trash,
            "\\junk" => Special::Junk,
            "\\archive" => Special::Archive,
            "\\all" => Special::All,
            _ => continue,
        };
        return Some((s, true));
    }
    let leaf = if delim.is_empty() { name } else { name.rsplit(delim).next().unwrap_or(name) };
    let s = match leaf.to_ascii_lowercase().as_str() {
        "sent" | "sent items" | "sent mail" | "sent messages" => Special::Sent,
        "drafts" | "draft" => Special::Drafts,
        "trash" | "deleted items" | "deleted messages" | "bin" => Special::Trash,
        "junk" | "spam" | "junk email" | "junk e-mail" | "bulk mail" => Special::Junk,
        "archive" | "archives" => Special::Archive,
        _ => return None,
    };
    Some((s, false))
}

/// Decode an IMAP modified UTF-7 mailbox name (RFC 3501 §5.1.3) for display.
pub fn decode_mutf7(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a' + 26) as u32,
            b'0'..=b'9' => (c - b'0' + 52) as u32,
            b'+' => 62,
            b',' => 63,
            _ => return None,
        })
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let Some(dash) = after.find('-') else {
            out.push_str(&rest[amp..]);
            return out;
        };
        let enc = &after[..dash];
        if enc.is_empty() {
            out.push('&');
        } else {
            let mut bits = 0u32;
            let mut nbits = 0;
            let mut units = Vec::new();
            let mut ok = true;
            for &c in enc.as_bytes() {
                match val(c) {
                    Some(v) => {
                        bits = (bits << 6) | v;
                        nbits += 6;
                        if nbits >= 16 {
                            nbits -= 16;
                            units.push(((bits >> nbits) & 0xFFFF) as u16);
                        }
                    }
                    None => ok = false,
                }
            }
            if ok {
                out.push_str(&String::from_utf16_lossy(&units));
            } else {
                out.push('&');
                out.push_str(enc);
                out.push('-');
            }
        }
        rest = &after[dash + 1..];
    }
    out.push_str(rest);
    out
}

/// Compact a list of UIDs into an IMAP sequence set ("1:4,7,9:12").
pub fn uid_set(uids: &[u32]) -> String {
    let mut v = uids.to_vec();
    v.sort_unstable();
    v.dedup();
    let mut out = String::new();
    let mut i = 0;
    while i < v.len() {
        let start = v[i];
        let mut end = start;
        while i + 1 < v.len() && v[i + 1] == end + 1 {
            i += 1;
            end = v[i];
        }
        if !out.is_empty() {
            out.push(',');
        }
        if start == end {
            out.push_str(&start.to_string());
        } else {
            out.push_str(&format!("{start}:{end}"));
        }
        i += 1;
    }
    out
}

/// Replace control characters (folded headers, tabs) with spaces for one-line display.
pub fn one_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = false;
    for c in s.chars() {
        let c = if c.is_control() { ' ' } else { c };
        if c == ' ' && last_space {
            continue;
        }
        last_space = c == ' ';
        out.push(c);
    }
    out.trim().to_string()
}

pub fn fmt_date_short(ts: i64) -> String {
    use chrono::{Datelike, Local, TimeZone};
    let Some(dt) = Local.timestamp_opt(ts, 0).single() else {
        return String::new();
    };
    let now = Local::now();
    if dt.date_naive() == now.date_naive() {
        dt.format("%H:%M").to_string()
    } else if dt.year() == now.year() {
        dt.format("%b %-d").to_string()
    } else {
        dt.format("%Y-%m-%d").to_string()
    }
}

pub fn fmt_date_long(ts: i64) -> String {
    use chrono::{Local, TimeZone};
    Local.timestamp_opt(ts, 0).single().map(|d| d.format("%a, %-d %b %Y %H:%M").to_string()).unwrap_or_default()
}

pub fn fmt_size(n: usize) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.0} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutf7() {
        assert_eq!(decode_mutf7("INBOX"), "INBOX");
        assert_eq!(decode_mutf7("Entw&APw-rfe"), "Entwürfe");
        assert_eq!(decode_mutf7("&-x"), "&x");
        assert_eq!(decode_mutf7("&ZeVnLIqe-"), "日本語");
    }

    #[test]
    fn uidsets() {
        assert_eq!(uid_set(&[5, 1, 2, 3, 9, 10, 7]), "1:3,5,7,9:10");
        assert_eq!(uid_set(&[4]), "4");
    }

    #[test]
    fn classify_names() {
        assert_eq!(classify("INBOX", "/", &[]), Some((Special::Inbox, true)));
        assert_eq!(classify("[Gmail]/Sent Mail", "/", &["\\Sent".into()]), Some((Special::Sent, true)));
        assert_eq!(classify("Deleted Items", "/", &[]), Some((Special::Trash, false)));
        assert_eq!(classify("Projects", "/", &[]), None);
    }
}
