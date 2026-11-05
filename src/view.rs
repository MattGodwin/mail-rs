//! Turns a raw RFC 5322 message into something the reader pane can show.
//!
//! The raw bytes are kept untouched alongside the parsed view; anything the
//! user saves, forwards or opens comes from those original bytes.

use crate::formats::{self, Invite, Notice};
use crate::model;
use mail_parser::{AddressList, Mailbox, MessageParser, PartId, PartKind};
use std::collections::HashMap;
use std::sync::Arc;

pub struct Attachment {
    pub name: String,
    pub mime: String,
    pub size: usize,
    pub part: PartId,
    /// An attached email (message/rfc822) that can be opened in the reader.
    pub is_message: bool,
    /// An image the reader can preview.
    pub is_image: bool,
}

pub fn previewable_image(mime: &str) -> bool {
    matches!(mime, "image/png" | "image/jpeg" | "image/jpg" | "image/pjpeg" | "image/gif" | "image/webp" | "image/bmp")
}

/// A run of body text: plain text, or a line containing clickable links.
pub enum Block {
    Text(String),
    Line(Vec<(bool, String)>),
}

pub struct View {
    pub subject: String,
    pub from: String,
    pub to: String,
    pub cc: String,
    pub date: String,
    pub message_id: String,
    pub references: Vec<String>,
    /// Who to reply to (Reply-To if present, else From), formatted for the To field.
    pub reply_to: Vec<String>,
    pub to_list: Vec<String>,
    pub cc_list: Vec<String>,
    /// Only present in drafts (sent mail never carries Bcc).
    pub bcc_list: Vec<String>,
    pub text: String,
    pub blocks: Vec<Block>,
    pub html: Option<String>,
    pub from_html: bool,
    /// The HTML references remote images/styles/fonts (blocked by default).
    pub remote_content: bool,
    /// Inline parts by Content-ID (lowercase, no angle brackets): (mime, part).
    pub cids: HashMap<String, (String, PartId)>,
    pub attachments: Vec<Attachment>,
    /// A calendar invitation and its raw .ics data.
    pub invite: Option<(Invite, Vec<u8>)>,
    pub notices: Vec<Notice>,
    /// List-Unsubscribe targets (https: or mailto:).
    pub unsubscribe: Vec<String>,
    pub raw: Arc<Vec<u8>>,
}

pub fn fmt_addr(a: &Mailbox) -> String {
    let addr = a.address().unwrap_or("");
    match a.name() {
        Some(n) if !n.trim().is_empty() && n != addr => {
            let n = model::one_line(n);
            if n.chars().any(|c| ",;<>@\"()[]:".contains(c)) {
                format!("\"{}\" <{addr}>", n.replace('\\', "\\\\").replace('"', "\\\""))
            } else {
                format!("{n} <{addr}>")
            }
        }
        _ => addr.to_string(),
    }
}

fn addr_list(a: Option<AddressList>) -> Vec<String> {
    a.map(|a| a.mailboxes().filter(|x| x.address().is_some()).map(|m| fmt_addr(&m)).collect()).unwrap_or_default()
}

pub fn parse(raw: Arc<Vec<u8>>) -> View {
    let parser = MessageParser::default();
    let Some(msg) = parser.parse(raw.as_slice()) else {
        let text = String::from_utf8_lossy(&raw).into_owned();
        return View {
            subject: "(unparseable message)".into(),
            from: String::new(),
            to: String::new(),
            cc: String::new(),
            date: String::new(),
            message_id: String::new(),
            references: vec![],
            reply_to: vec![],
            to_list: vec![],
            cc_list: vec![],
            bcc_list: vec![],
            blocks: linkify(&text),
            text,
            html: None,
            from_html: false,
            remote_content: false,
            cids: HashMap::new(),
            attachments: vec![],
            invite: None,
            notices: vec![],
            unsubscribe: vec![],
            raw,
        };
    };

    let from_list = addr_list(msg.from());
    let to_list = addr_list(msg.to());
    let cc_list = addr_list(msg.cc());
    let bcc_list = addr_list(msg.bcc());
    let reply_to = {
        let r = addr_list(msg.reply_to());
        if r.is_empty() {
            from_list.clone()
        } else {
            r
        }
    };

    let mime_of = |ct: Option<mail_parser::ContentType>| {
        ct.map(|ct| match ct.subtype() {
            Some(st) => format!("{}/{}", ct.ctype(), st),
            None => ct.ctype().to_string(),
        })
        .unwrap_or_else(|| "text/plain".into())
        .to_ascii_lowercase()
    };

    // Body: prefer a real text/plain part (reflowing format=flowed text).
    // HTML is kept separately for the web view; nothing remote is fetched.
    let mut text = String::new();
    let mut has_plain = false;
    for p in msg.text_body() {
        if !matches!(p.kind(), PartKind::Text) || mime_of(p.content_type()) == "text/calendar" {
            continue;
        }
        if let Some(t) = p.text() {
            let ct = p.content_type();
            let flowed = ct.as_ref().and_then(|c| c.attribute("format")).is_some_and(|f| f.eq_ignore_ascii_case("flowed"));
            let delsp = ct.as_ref().and_then(|c| c.attribute("delsp")).is_some_and(|f| f.eq_ignore_ascii_case("yes"));
            let t = if flowed { formats::unflow(&t, delsp) } else { t.into_owned() };
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&t);
            has_plain = true;
        }
    }
    let html = msg.html_body().find(|p| matches!(p.kind(), PartKind::Html)).and_then(|p| p.text()).map(|h| h.into_owned());
    let mut from_html = false;
    if !has_plain {
        if let Some(h) = &html {
            // If conversion fails, fall back to mail-parser's simpler HTML-to-text.
            text = html2text::from_read(h.as_bytes(), 110).unwrap_or_else(|_| msg.body_text(0).map(|t| t.into_owned()).unwrap_or_default());
            from_html = true;
        }
    }
    // Normalise line endings for display only.
    if text.contains('\r') {
        text = text.replace("\r\n", "\n");
    }

    // Inline parts, calendar data and formats that need explaining.
    let mut cids = HashMap::new();
    let mut invite = None;
    let mut notices = Vec::new();
    for p in msg.parts() {
        let ct = p.content_type();
        let mime = mime_of(ct.clone());
        if let Some(id) = p.content_id() {
            cids.insert(crate::html::cid_key(id), (mime.clone(), p.id()));
        }
        if mime == "text/calendar" && invite.is_none() {
            let data = p.decoded().into_owned();
            if let Some(inv) = formats::parse_invite(&String::from_utf8_lossy(&data)) {
                invite = Some((inv, data));
            }
        }
        let protocol = ct.as_ref().and_then(|c| c.attribute("protocol"));
        let smime = ct.as_ref().and_then(|c| c.attribute("smime-type"));
        let tnef_name = p.attachment_name().is_some_and(|n| n.eq_ignore_ascii_case("winmail.dat"));
        let notice = if tnef_name { Some(Notice::Tnef) } else { formats::notice_for(&mime, protocol, smime) };
        if let Some(n) = notice {
            if !notices.contains(&n) {
                notices.push(n);
            }
        }
    }
    if text.contains("-----BEGIN PGP MESSAGE-----") && !notices.iter().any(|n| matches!(n, Notice::Encrypted(_))) {
        notices.push(Notice::Encrypted("PGP"));
    } else if text.contains("-----BEGIN PGP SIGNED MESSAGE-----") && !notices.iter().any(|n| matches!(n, Notice::Signed(_))) {
        notices.push(Notice::Signed("PGP"));
    }

    let attachments = msg
        .attachments()
        .map(|p| {
            let mime = mime_of(p.content_type());
            let mime = if p.content_type().is_none() { "application/octet-stream".to_string() } else { mime };
            let is_message = matches!(p.kind(), PartKind::Message(_));
            let name = p.attachment_name().map(model::one_line).unwrap_or_else(|| match p.kind() {
                PartKind::Message(m) => format!("{}.eml", m.subject().map(sanitize_filename).unwrap_or_else(|| "message".into())),
                _ => format!("attachment-{}", p.id()),
            });
            Attachment { is_image: previewable_image(&mime), name, mime, size: p.decoded_len(), part: p.id(), is_message }
        })
        .collect();

    let unsubscribe = msg
        .list_unsubscribe()
        .map(|l| {
            l.mailboxes()
                .filter_map(|m| m.address().map(|a| a.trim().to_string()))
                .filter(|u| u.starts_with("https://") || u.starts_with("http://") || u.starts_with("mailto:"))
                .collect()
        })
        .unwrap_or_default();

    View {
        subject: model::one_line(msg.subject().unwrap_or("(no subject)")),
        from: from_list.join(", "),
        to: to_list.join(", "),
        cc: cc_list.join(", "),
        date: msg.date().map(|d| model::fmt_date_long(d.to_timestamp())).unwrap_or_default(),
        message_id: msg.message_id().unwrap_or("").to_string(),
        references: msg.references().map(|t| t.iter().map(String::from).collect()).unwrap_or_default(),
        reply_to,
        to_list,
        cc_list,
        bcc_list,
        blocks: linkify(&text),
        text,
        remote_content: html.as_deref().is_some_and(crate::html::has_remote_content),
        html,
        from_html,
        cids,
        attachments,
        invite,
        notices,
        unsubscribe,
        raw: raw.clone(),
    }
}

/// The decoded bytes of one MIME part (for saving an attachment). A nested
/// message decodes to its original bytes.
pub fn part_bytes(raw: &[u8], part: PartId) -> Option<Vec<u8>> {
    let msg = MessageParser::default().parse(raw)?;
    Some(msg.part(part)?.decoded().into_owned())
}

/// (name, mime, bytes) of every attachment, for forwarding.
pub fn all_attachments(v: &View) -> Vec<(String, String, Vec<u8>)> {
    v.attachments.iter().filter_map(|a| Some((a.name.clone(), a.mime.clone(), part_bytes(&v.raw, a.part)?))).collect()
}

pub fn sanitize_filename(s: &str) -> String {
    let s: String = s.chars().map(|c| if "\\/:*?\"<>|".contains(c) || c.is_control() { '_' } else { c }).collect();
    let s = s.trim().trim_matches('.').to_string();
    if s.is_empty() {
        "file".into()
    } else {
        s.chars().take(120).collect()
    }
}

fn link_end(s: &str) -> usize {
    let end = s.find(|c: char| c.is_whitespace() || "<>\"'".contains(c)).unwrap_or(s.len());
    let mut e = end;
    // Trailing punctuation is almost never part of the URL.
    while e > 0 && ".,;:!?)]}".contains(&s[..e].chars().last().unwrap().to_string()) {
        e -= s[..e].chars().last().unwrap().len_utf8();
    }
    e
}

fn find_link(s: &str) -> Option<usize> {
    ["https://", "http://", "mailto:"].iter().filter_map(|p| s.find(p)).min()
}

/// Split body text into blocks so URLs can be rendered as clickable links.
pub fn linkify(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut plain = String::new();
    for line in text.split('\n') {
        if find_link(line).is_none() {
            plain.push_str(line);
            plain.push('\n');
            continue;
        }
        if !plain.is_empty() {
            plain.pop();
            blocks.push(Block::Text(std::mem::take(&mut plain)));
        }
        let mut segs = Vec::new();
        let mut rest = line;
        while let Some(start) = find_link(rest) {
            if start > 0 {
                segs.push((false, rest[..start].to_string()));
            }
            let tail = &rest[start..];
            let len = link_end(tail);
            let url = &tail[..len];
            if url.len() <= "https://".len() {
                segs.push((false, tail[..len.max(1)].to_string()));
                rest = &tail[len.max(1)..];
                continue;
            }
            segs.push((true, url.to_string()));
            rest = &tail[len..];
        }
        if !rest.is_empty() {
            segs.push((false, rest.to_string()));
        }
        blocks.push(Block::Line(segs));
    }
    if !plain.is_empty() {
        plain.pop();
        blocks.push(Block::Text(plain));
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &[u8] = b"From: \"Doe, Jane\" <jane@example.com>\r\n\
To: bob@example.com\r\n\
Subject: =?UTF-8?Q?Caf=C3=A9_plans?=\r\n\
Message-ID: <abc@example.com>\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=XX\r\n\
\r\n\
--XX\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
See https://example.com/a?b=c. Thanks!\r\n\
--XX\r\n\
Content-Type: application/pdf; name=\"plan.pdf\"\r\n\
Content-Disposition: attachment; filename=\"plan.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
JVBERi0xLjQK\r\n\
--XX--\r\n";

    #[test]
    fn parses_sample() {
        let v = parse(Arc::new(SAMPLE.to_vec()));
        assert_eq!(v.subject, "Café plans");
        assert_eq!(v.from, "\"Doe, Jane\" <jane@example.com>");
        assert_eq!(v.message_id, "abc@example.com");
        assert_eq!(v.attachments.len(), 1);
        assert_eq!(v.attachments[0].name, "plan.pdf");
        let bytes = part_bytes(&v.raw, v.attachments[0].part).unwrap();
        assert_eq!(bytes, b"%PDF-1.4\n");
        // Raw bytes are untouched.
        assert_eq!(v.raw.as_slice(), SAMPLE);
    }

    #[test]
    fn links() {
        let b = linkify("Go to https://x.org/p. now\nplain");
        match &b[0] {
            Block::Line(s) => {
                assert_eq!(s[1], (true, "https://x.org/p".to_string()));
                assert_eq!(s[2], (false, ". now".to_string()));
            }
            _ => panic!(),
        }
        assert!(matches!(&b[1], Block::Text(t) if t == "plain"));
    }
}
