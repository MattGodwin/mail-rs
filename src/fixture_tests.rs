//! End-to-end parser contracts using wire-format mail, never rewritten display text.
use crate::{formats::Notice, html, view};
use std::sync::Arc;

const FIXTURES: &[(&str, &[u8], &str, &str)] = &[
    ("newsletter", include_bytes!("../testdata/emails/newsletter.eml"), "November garden notes", "Garden Weekly <news@example.org>"),
    ("alternative", include_bytes!("../testdata/emails/alternative.eml"), "Re: launch checklist", "Alex Morgan <alex@example.org>"),
    ("related", include_bytes!("../testdata/emails/related.eml"), "Courtyard planting plan", "Alex Morgan <alex@example.org>"),
    (
        "image-attachment",
        include_bytes!("../testdata/emails/image-attachment.eml"),
        "Photo of the courtyard",
        "Alex Morgan <alex@example.org>",
    ),
    ("calendar-request", include_bytes!("../testdata/emails/calendar-request.eml"), "Courtyard review", "Alex Morgan <alex@example.org>"),
    (
        "calendar-cancel",
        include_bytes!("../testdata/emails/calendar-cancel.eml"),
        "Courtyard review cancelled",
        "Alex Morgan <alex@example.org>",
    ),
    ("flowed", include_bytes!("../testdata/emails/flowed.eml"), "Re: weekend planting", "Alex Morgan <alex@example.org>"),
    ("flowed-delsp", include_bytes!("../testdata/emails/flowed-delsp.eml"), "Re: path labels", "Alex Morgan <alex@example.org>"),
    ("smime-signed", include_bytes!("../testdata/emails/smime-signed.eml"), "Signed budget approval", "Alex Morgan <alex@example.org>"),
    (
        "smime-encrypted",
        include_bytes!("../testdata/emails/smime-encrypted.eml"),
        "Confidential supplier terms",
        "Alex Morgan <alex@example.org>",
    ),
    ("pgp-signed", include_bytes!("../testdata/emails/pgp-signed.eml"), "Signed delivery approval", "Alex Morgan <alex@example.org>"),
    (
        "pgp-encrypted",
        include_bytes!("../testdata/emails/pgp-encrypted.eml"),
        "Private planting schedule",
        "Alex Morgan <alex@example.org>",
    ),
    ("pgp-inline", include_bytes!("../testdata/emails/pgp-inline.eml"), "Inline signed order", "Alex Morgan <alex@example.org>"),
    ("tnef", include_bytes!("../testdata/emails/tnef.eml"), "Outlook planting notes", "Alex Morgan <alex@example.org>"),
    ("forwarded", include_bytes!("../testdata/emails/forwarded.eml"), "Fwd: Bulb order confirmation", "Alex Morgan <alex@example.org>"),
    ("latin1", include_bytes!("../testdata/emails/latin1.eml"), "Café résumé", "André Martin <andre@example.org>"),
    ("shift-jis", include_bytes!("../testdata/emails/shift-jis.eml"), "庭の予定", "山田太郎 <taro@example.jp>"),
    (
        "delivery-failure",
        include_bytes!("../testdata/emails/delivery-failure.eml"),
        "Delivery Status Notification (Failure)",
        "Mail Delivery System <mailer-daemon@example.org>",
    ),
];

fn load(name: &str) -> view::View {
    let (_, raw, _, _) = FIXTURES.iter().find(|f| f.0 == name).unwrap();
    view::parse(Arc::new(raw.to_vec()))
}

#[test]
fn corpus_headers_and_original_wire_bytes() {
    for &(name, raw, subject, from) in FIXTURES {
        assert!(raw.windows(4).any(|w| w == b"\r\n\r\n"), "{name}");
        for (i, &b) in raw.iter().enumerate() {
            if b == b'\n' {
                assert!(i > 0 && raw[i - 1] == b'\r', "bare LF: {name}");
            }
            if b == b'\r' {
                assert_eq!(raw.get(i + 1), Some(&b'\n'), "bare CR: {name}");
            }
        }
        let input = Arc::new(raw.to_vec());
        let v = view::parse(input.clone());
        assert_eq!(v.subject, subject, "{name}");
        assert_eq!(v.from, from, "{name}");
        assert_eq!(v.to_list, ["Sam Reader <sam@example.net>"], "{name}");
        assert_eq!(v.raw.as_slice(), raw, "{name}");
        assert!(Arc::ptr_eq(&v.raw, &input), "{name}");
        assert_eq!(v.remote_content, name == "newsletter", "{name}");
        assert_eq!(v.html.is_some(), matches!(name, "newsletter" | "alternative" | "related"), "{name}");
        assert_eq!(v.invite.is_some(), matches!(name, "calendar-request" | "calendar-cancel"), "{name}");
        if !matches!(name, "smime-signed" | "smime-encrypted" | "pgp-signed" | "pgp-encrypted" | "pgp-inline" | "tnef") {
            assert!(v.notices.is_empty(), "unexpected format notice: {name}");
        }
    }
}

#[test]
fn newsletter_privacy_and_unsubscribe() {
    let v = load("newsletter");
    assert!(v.from_html);
    assert!(v.text.contains("Plant bulbs before frost."));
    assert_eq!(v.unsubscribe, ["https://news.example.org/unsubscribe?id=42", "mailto:leave@example.org?subject=unsubscribe"]);
    let source = v.html.as_deref().unwrap();
    let prepared = html::prepare(source);
    assert!(!prepared.to_ascii_lowercase().contains("http-equiv=\"refresh\""));
    assert!(prepared.contains("src=\"cid/Logo@Garden\""));
    assert!(source.contains("cid:Logo@Garden"));
    // Independently exercise the tracking pixel and CSS resource detection.
    assert!(html::has_remote_content("<img src=\"https://track.example.org/open.gif?id=42\">"));
    assert!(html::has_remote_content("<div style=\"background-image: url(https://cdn.example.org/leaves.png)\">"));
    assert!(html::csp(false).contains("img-src 'self' data:;"));
    assert!(html::csp(true).contains("img-src 'self' data: https: http:"));
    assert!(html::csp(true).contains("script-src 'none'"));
}

#[test]
fn alternative_prefers_plain_and_preserves_recipients() {
    let v = load("alternative");
    assert_eq!(v.text, "The launch checklist is ready.\nPlease review by Friday.\n");
    assert!(!v.from_html);
    assert!(v.html.as_deref().unwrap().contains("<b>launch checklist</b>"));
    assert_eq!(v.cc_list, ["Priya Shah <priya@example.org>", "QA <qa@example.org>"]);
    assert_eq!(v.bcc_list, ["Archive <archive@example.net>"]);
    assert_eq!(v.reply_to, ["Launch Team <launch@example.org>"]);
    assert_eq!(v.references, ["kickoff@example.org", "review@example.org"]);
    assert!(v.attachments.is_empty());
}

fn assert_png(bytes: &[u8]) {
    assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    let image = image::load_from_memory(bytes).expect("valid, decodable PNG");
    assert_eq!((image.width(), image.height()), (1, 1));
}

#[test]
fn related_cids_resolve_to_valid_local_images() {
    let v = load("related");
    assert!(v.text.contains("Our courtyard plan."));
    assert!(v.from_html);
    assert_eq!(v.cids.len(), 1);
    let (mime, part) = &v.cids[&html::cid_key("<Plan@Garden>")];
    assert_eq!(mime, "image/png");
    assert_png(&view::part_bytes(&v.raw, *part).unwrap());
    assert!(html::prepare(v.html.as_deref().unwrap()).contains("src=\"cid/Plan@Garden\""));
    assert!(view::part_bytes(&v.raw, mail_parser::PartId::MAX).is_none());
}

#[test]
fn image_attachment_decodes_without_altering_wire_data() {
    let v = load("image-attachment");
    assert!(v.text.contains("Here is the courtyard photo."));
    assert_eq!(v.attachments.len(), 1);
    let a = &v.attachments[0];
    assert_eq!(a.name, "courtyard.png");
    assert_eq!(a.mime, "image/png");
    assert!(a.is_image);
    assert!(!a.is_message);
    let bytes = view::part_bytes(&v.raw, a.part).unwrap();
    assert_eq!(a.size, bytes.len());
    assert_png(&bytes);
    assert_eq!(view::all_attachments(&v)[0], (a.name.clone(), a.mime.clone(), bytes));
}

#[test]
fn calendar_request_and_cancellation_keep_ics_and_timezone() {
    for (name, method, summary, headline) in [
        ("calendar-request", "REQUEST", "Courtyard review", "Invitation"),
        ("calendar-cancel", "CANCEL", "Courtyard review cancelled", "Event cancelled"),
    ] {
        let v = load(name);
        assert!(v.text.contains("The courtyard meeting"));
        assert!(!v.text.contains("BEGIN:VCALENDAR"));
        let (inv, data) = v.invite.as_ref().unwrap();
        assert_eq!(inv.method, method);
        assert_eq!(inv.summary, summary);
        assert_eq!(inv.headline(), headline);
        assert_eq!(inv.location, "Room 4, garden office");
        assert_eq!(inv.organizer, "Priya Shah <priya@example.org>");
        assert_eq!(inv.when, "Tue 10 Nov 2026, 14:30 (Europe/London) – Tue 10 Nov 2026, 15:30 (Europe/London)");
        assert!(data.starts_with(b"BEGIN:VCALENDAR\r\n"));
        assert!(String::from_utf8_lossy(data).contains(&format!("METHOD:{method}\r\n")));
    }
}

#[test]
fn flowed_quotes_stuffing_and_delsp() {
    assert_eq!(load("flowed").text, "We can plant the bulbs this Saturday.\n\n> Bring the spare gardening gloves.\n>literal greater-than\nFrom the garden team\n-- \nAlex\n");
    assert_eq!(load("flowed-delsp").text, "The label reads greenhouse.\n> Water the seedlings.\n-- \nSam\n");
    for text in ["", "\n", "\n\n", "Last line", "Last line\n", "Last line\n\n"] {
        assert_eq!(crate::formats::unflow(text, false), text, "preserve line endings: {text:?}");
    }
}

#[test]
fn security_and_outlook_notices_are_classified_once() {
    for (name, notice, text) in [
        ("smime-signed", Notice::Signed("S/MIME"), "I approve the garden budget."),
        ("smime-encrypted", Notice::Encrypted("S/MIME"), ""),
        ("pgp-signed", Notice::Signed("PGP"), "The delivery is approved."),
        ("pgp-encrypted", Notice::Encrypted("PGP"), ""),
        ("pgp-inline", Notice::Signed("PGP"), "Please order twenty bulbs."),
        ("tnef", Notice::Tnef, "Please see the planting notes from Outlook."),
    ] {
        let v = load(name);
        assert_eq!(v.notices, [notice], "{name}");
        if text.is_empty() {
            assert!(v.text.is_empty(), "encrypted data must not become display text: {name}");
        } else {
            assert!(v.text.contains(text), "{name}");
        }
        for a in &v.attachments {
            assert!(!a.is_image && !a.is_message, "{name}: {}", a.name);
            assert!(!view::part_bytes(&v.raw, a.part).unwrap().is_empty());
        }
    }
    assert!(load("tnef").attachments.iter().any(|a| a.name == "winmail.dat"));
    assert!(load("smime-signed").attachments.iter().any(|a| a.name == "smime.p7s"));
}

#[test]
fn legacy_charsets_decode_headers_and_bodies() {
    assert_eq!(load("latin1").text, "Un café pour André.\n");
    assert_eq!(load("shift-jis").text.trim_end(), "土曜日に花を植えます。");
}

#[test]
fn forwarded_message_and_delivery_report_open_nested_original() {
    for (name, text) in
        [("forwarded", "Here is the nursery confirmation."), ("delivery-failure", "Delivery to missing@example.net failed permanently.")]
    {
        let v = load(name);
        assert!(v.text.contains(text));
        let attached: Vec<_> = v.attachments.iter().filter(|a| a.is_message).collect();
        assert_eq!(attached.len(), 1, "{name}");
        let a = attached[0];
        assert!(!a.is_image);
        assert_eq!(a.mime, "message/rfc822");
        let bytes = view::part_bytes(&v.raw, a.part).unwrap();
        assert!(bytes.starts_with(b"From: Nursery Sales"));
        assert!(v.raw.windows(bytes.len()).any(|w| w == bytes), "nested wire bytes must be preserved");
        let nested = view::parse(Arc::new(bytes.clone()));
        assert_eq!(nested.subject, "Bulb order confirmation");
        assert_eq!(nested.from, "Nursery Sales <sales@nursery.example>");
        assert_eq!(nested.text, "Your order of twenty tulip bulbs is confirmed.\n");
        assert_eq!(nested.raw.as_slice(), bytes);
        assert!(nested.attachments.is_empty());
        assert!(v.notices.is_empty());
    }
}
