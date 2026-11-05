//! Handling for common email formats beyond plain text and HTML:
//! format=flowed text (RFC 3676), calendar invitations (iCalendar),
//! and recognising signed/encrypted/Outlook-only messages.

use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone, Utc};

/// Reflow `format=flowed` text: lines ending in a space are soft breaks.
/// Quote depth is preserved and space-stuffing removed, so quoted replies
/// read naturally at any window width.
pub fn unflow(text: &str, delsp: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut para = String::new();
    let mut depth: Option<usize> = None;
    let flush = |out: &mut String, para: &mut String, depth: &mut Option<usize>| {
        if let Some(d) = depth.take() {
            out.push_str(&">".repeat(d));
            if d > 0 && !para.is_empty() {
                out.push(' ');
            }
            out.push_str(para);
            out.push('\n');
            para.clear();
        }
    };
    // A terminal newline ends the last line; it does not add an empty line.
    for raw in text.split_terminator('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let d = line.chars().take_while(|&c| c == '>').count();
        let mut content = &line[d..];
        if let Some(c) = content.strip_prefix(' ') {
            content = c; // space-stuffing / quote separator
        }
        if depth.is_some() && depth != Some(d) {
            flush(&mut out, &mut para, &mut depth);
        }
        depth = Some(d);
        let is_sig = content == "-- ";
        if content.ends_with(' ') && !is_sig {
            para.push_str(if delsp { &content[..content.len() - 1] } else { content });
        } else {
            para.push_str(content);
            flush(&mut out, &mut para, &mut depth);
        }
    }
    flush(&mut out, &mut para, &mut depth);
    if !text.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    out
}

/// A calendar invitation, summarised for display.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Invite {
    /// REQUEST, CANCEL, REPLY, PUBLISH…
    pub method: String,
    pub summary: String,
    pub when: String,
    pub location: String,
    pub organizer: String,
}

impl Invite {
    pub fn headline(&self) -> &'static str {
        match self.method.to_ascii_uppercase().as_str() {
            "CANCEL" => "Event cancelled",
            "REPLY" => "Invitation reply",
            "REQUEST" => "Invitation",
            _ => "Calendar event",
        }
    }
}

/// Unfold and unescape iCalendar content lines into (name, params, value).
fn ics_lines(ics: &str) -> Vec<(String, String, String)> {
    let mut unfolded: Vec<String> = Vec::new();
    for line in ics.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if (line.starts_with(' ') || line.starts_with('\t')) && !unfolded.is_empty() {
            unfolded.last_mut().unwrap().push_str(&line[1..]);
        } else {
            unfolded.push(line.to_string());
        }
    }
    unfolded
        .into_iter()
        .filter_map(|l| {
            let colon = {
                // The first ':' not inside a quoted parameter value.
                let mut q = false;
                l.char_indices()
                    .find(|&(_, c)| {
                        if c == '"' {
                            q = !q;
                        }
                        c == ':' && !q
                    })?
                    .0
            };
            let (head, value) = (&l[..colon], &l[colon + 1..]);
            let (name, params) = head.split_once(';').unwrap_or((head, ""));
            let value = value.replace("\\n", "\n").replace("\\N", "\n").replace("\\,", ",").replace("\\;", ";").replace("\\\\", "\\");
            Some((name.to_ascii_uppercase(), params.to_string(), value))
        })
        .collect()
}

fn param<'a>(params: &'a str, key: &str) -> Option<&'a str> {
    params.split(';').find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.eq_ignore_ascii_case(key).then(|| v.trim_matches('"'))
    })
}

/// Human-readable date/time for an iCalendar DTSTART/DTEND value.
fn ics_time(params: &str, v: &str) -> Option<String> {
    let v = v.trim();
    if v.len() == 8 || param(params, "VALUE").is_some_and(|x| x.eq_ignore_ascii_case("DATE")) {
        let d = NaiveDate::parse_from_str(&v[..8.min(v.len())], "%Y%m%d").ok()?;
        return Some(d.format("%a %-d %b %Y (all day)").to_string());
    }
    if let Some(utc) = v.strip_suffix('Z') {
        let t = NaiveDateTime::parse_from_str(utc, "%Y%m%dT%H%M%S").ok()?;
        return Some(Utc.from_utc_datetime(&t).with_timezone(&Local).format("%a %-d %b %Y, %H:%M").to_string());
    }
    let t = NaiveDateTime::parse_from_str(v, "%Y%m%dT%H%M%S").ok()?;
    let tz = param(params, "TZID").map(|z| format!(" ({z})")).unwrap_or_default();
    Some(format!("{}{tz}", t.format("%a %-d %b %Y, %H:%M")))
}

/// "Tue 10 Nov 2026, 14:30 (Z)" + "Tue 10 Nov 2026, 15:30 (Z)" -> "Tue 10 Nov 2026 · 14:30 – 15:30 (Z)".
fn join_range(s: &str, e: &str) -> String {
    match (s.split_once(", "), e.split_once(", ")) {
        (Some((d1, t1)), Some((d2, t2))) if d1 == d2 => {
            let tz = |t: &str| t.find(" (").map(|i| t[i..].to_string()).unwrap_or_default();
            let (z1, z2) = (tz(t1), tz(t2));
            let t1 = if z1 == z2 { t1.trim_end_matches(z1.as_str()) } else { t1 };
            format!("{d1} · {t1} – {t2}")
        }
        _ => format!("{s} – {e}"),
    }
}

pub fn parse_invite(ics: &str) -> Option<Invite> {
    let mut inv = Invite::default();
    let mut in_event = false;
    let (mut start, mut end) = (None, None);
    let mut seen_event = false;
    for (name, params, value) in ics_lines(ics) {
        match name.as_str() {
            "METHOD" if !in_event => inv.method = value,
            "BEGIN" if value.eq_ignore_ascii_case("VEVENT") && !seen_event => in_event = true,
            "END" if value.eq_ignore_ascii_case("VEVENT") && in_event => {
                in_event = false;
                seen_event = true;
            }
            "SUMMARY" if in_event => inv.summary = value,
            "LOCATION" if in_event => inv.location = value,
            "DTSTART" if in_event => start = ics_time(&params, &value),
            "DTEND" if in_event => end = ics_time(&params, &value),
            "ORGANIZER" if in_event => {
                let addr = value.trim_start_matches("mailto:").trim_start_matches("MAILTO:");
                inv.organizer = match param(&params, "CN") {
                    Some(cn) if !cn.is_empty() => format!("{cn} <{addr}>"),
                    _ => addr.to_string(),
                };
            }
            _ => {}
        }
    }
    if !seen_event && !in_event {
        return None;
    }
    inv.when = match (start, end) {
        (Some(s), Some(e)) if s != e => join_range(&s, &e),
        (Some(s), _) => s,
        _ => String::new(),
    };
    if inv.summary.is_empty() {
        inv.summary = "(untitled event)".into();
    }
    Some(inv)
}

/// Something about the message the reader should explain.
#[derive(Clone, Debug, PartialEq)]
pub enum Notice {
    /// Digitally signed; the signature is not verified. ("S/MIME" or "PGP")
    Signed(&'static str),
    /// Encrypted content Mail can't open.
    Encrypted(&'static str),
    /// Outlook's winmail.dat (TNEF) format.
    Tnef,
}

impl Notice {
    pub fn text(&self) -> String {
        match self {
            Notice::Signed(k) => format!("🔏 Digitally signed ({k}). Mail shows the content but doesn't verify the signature yet."),
            Notice::Encrypted(k) => format!(
                "🔒 This message is encrypted ({k}). Mail can't decrypt it yet; use \"Save as .eml\" to open it in an app that has your key."
            ),
            Notice::Tnef => "📎 Part of this message is in winmail.dat, an Outlook-only format. Ask the sender to send it as plain text or HTML to see everything.".into(),
        }
    }
}

/// Classify a content type (lowercase "type/subtype") plus parameters.
pub fn notice_for(mime: &str, protocol: Option<&str>, smime_type: Option<&str>) -> Option<Notice> {
    let proto = protocol.unwrap_or("").to_ascii_lowercase();
    match mime {
        "multipart/signed" => Some(Notice::Signed(if proto.contains("pgp") { "PGP" } else { "S/MIME" })),
        "multipart/encrypted" => Some(Notice::Encrypted(if proto.contains("pgp") || proto.is_empty() { "PGP" } else { "S/MIME" })),
        "application/pkcs7-mime" | "application/x-pkcs7-mime" => {
            if smime_type.is_some_and(|t| t.eq_ignore_ascii_case("signed-data")) {
                Some(Notice::Signed("S/MIME"))
            } else {
                Some(Notice::Encrypted("S/MIME"))
            }
        }
        "application/ms-tnef" | "application/vnd.ms-tnef" => Some(Notice::Tnef),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unflows_paragraphs_and_quotes() {
        let t = "Hello there, this is \r\na flowed line.\r\n\r\n> quoted text that \r\n> continues here\r\n-- \r\nSig";
        assert_eq!(unflow(t, false), "Hello there, this is a flowed line.\n\n> quoted text that continues here\n-- \nSig");
        assert_eq!(unflow("ab \r\ncd", true), "abcd");
        assert_eq!(unflow(" From the start", false), "From the start");
    }

    #[test]
    fn parses_invites() {
        let ics = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nSUMMARY:Q4 roadmap\r\n  review\r\nDTSTART;TZID=Europe/London:20261110T143000\r\nDTEND;TZID=Europe/London:20261110T153000\r\nLOCATION:Room 4\\, 2nd floor\r\nORGANIZER;CN=\"Priya Shah\":mailto:priya@example.org\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let inv = parse_invite(ics).unwrap();
        assert_eq!(inv.headline(), "Invitation");
        assert_eq!(inv.summary, "Q4 roadmap review");
        assert_eq!(inv.location, "Room 4, 2nd floor");
        assert_eq!(inv.organizer, "Priya Shah <priya@example.org>");
        assert_eq!(inv.when, "Tue 10 Nov 2026 · 14:30 – 15:30 (Europe/London)");
        let all_day = parse_invite("BEGIN:VEVENT\nSUMMARY:Holiday\nDTSTART;VALUE=DATE:20261224\nEND:VEVENT").unwrap();
        assert_eq!(all_day.when, "Thu 24 Dec 2026 (all day)");
        assert!(parse_invite("BEGIN:VCALENDAR\nEND:VCALENDAR").is_none());
    }

    #[test]
    fn recognises_secure_and_outlook_formats() {
        assert_eq!(notice_for("multipart/signed", Some("application/pgp-signature"), None), Some(Notice::Signed("PGP")));
        assert_eq!(notice_for("multipart/signed", Some("application/pkcs7-signature"), None), Some(Notice::Signed("S/MIME")));
        assert_eq!(notice_for("application/pkcs7-mime", None, Some("enveloped-data")), Some(Notice::Encrypted("S/MIME")));
        assert_eq!(notice_for("multipart/encrypted", Some("application/pgp-encrypted"), None), Some(Notice::Encrypted("PGP")));
        assert_eq!(notice_for("application/ms-tnef", None, None), Some(Notice::Tnef));
        assert_eq!(notice_for("text/plain", None, None), None);
    }
}
