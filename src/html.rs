//! Faithful HTML email rendering in the platform web engine (WebView2 on
//! Windows, WKWebView on macOS), locked down for privacy:
//!
//! - The message is served from memory over a private `mailview:` scheme;
//!   inline `cid:` images come from the message's own MIME parts.
//! - A Content-Security-Policy blocks all scripts, forms, frames and plugins,
//!   and blocks remote images, styles and fonts (tracking pixels) unless the
//!   user chooses "Load remote content" for that message.
//! - Navigation never happens inside the view: clicked links open in the
//!   system browser, and `mailto:` links open a compose window.
//!
//! Only a display copy of the HTML is adjusted (meta refresh removed, `cid:`
//! references pointed at the local scheme); the stored message is untouched.

// Linux has no embedded web view; these helpers are only used where it exists.
#![cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]

use std::collections::HashMap;
use std::sync::OnceLock;

/// Remove things that could navigate or load without the user acting.
fn strip_auto_navigation(html: &str) -> String {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r#"(?is)<meta[^>]*http-equiv\s*=\s*["']?\s*refresh[^>]*>"#).unwrap());
    re.replace_all(html, "").into_owned()
}

/// Point `cid:` references at the message's own parts (served relative to the page).
fn rewrite_cid(html: &str) -> String {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r#"(?i)(["'(=\s])cid:"#).unwrap());
    re.replace_all(html, "${1}cid/").into_owned()
}

/// Does the HTML reference remote resources (images, styles, fonts)?
pub fn has_remote_content(html: &str) -> bool {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r#"(?i)(?:src|background|srcset|poster)\s*=\s*["']?\s*(?:https?:)?//|url\(\s*["']?\s*(?:https?:)?//|<link[^>]+href\s*=\s*["']?\s*(?:https?:)?//"#)
            .unwrap()
    });
    re.is_match(html)
}

/// The display copy of an HTML body.
pub fn prepare(html: &str) -> String {
    rewrite_cid(&strip_auto_navigation(html))
}

/// Content-Security-Policy for a message page.
pub fn csp(allow_remote: bool) -> String {
    let remote = if allow_remote { " https: http:" } else { "" };
    format!(
        "default-src 'none'; img-src 'self' data:{remote}; style-src 'self' 'unsafe-inline'{remote}; \
         font-src 'self' data:{remote}; media-src 'none'; script-src 'none'; object-src 'none'; \
         frame-src 'none'; form-action 'none'; base-uri 'none'"
    )
}

/// Normalise a Content-ID (or a `cid:` URL path) for lookup.
pub fn cid_key(s: &str) -> String {
    s.trim().trim_start_matches('<').trim_end_matches('>').to_ascii_lowercase()
}

/// What the view is showing: one prepared page plus the parts it may load.
pub struct Page {
    pub html: String,
    pub raw: std::sync::Arc<Vec<u8>>,
    /// cid key -> (mime type, part id)
    pub cids: HashMap<String, (String, mail_parser::PartId)>,
    pub allow_remote: bool,
}

#[cfg(any(windows, target_os = "macos"))]
pub use imp::HtmlView;

#[cfg(any(windows, target_os = "macos"))]
mod imp {
    use super::{cid_key, csp, Page};
    use std::borrow::Cow;
    use std::sync::{Arc, Mutex};
    use wry::dpi::{PhysicalPosition, PhysicalSize};
    use wry::http::{header, Response, StatusCode};
    use wry::{NewWindowResponse, Rect, WebContext, WebView, WebViewBuilder};

    #[cfg(windows)]
    const ORIGIN: &str = "http://mailview.localhost";
    #[cfg(not(windows))]
    const ORIGIN: &str = "mailview://localhost";

    pub struct HtmlView {
        webview: WebView,
        _context: WebContext,
        page: Arc<Mutex<Option<(u64, Page)>>>,
        generation: u64,
        shown: Option<u64>,
        visible: bool,
        bounds: Option<(i32, i32, u32, u32)>,
    }

    fn respond(status: StatusCode, mime: &str, body: Vec<u8>, csp: Option<String>) -> Response<Cow<'static, [u8]>> {
        let mut r = Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, mime)
            .header(header::CACHE_CONTROL, "no-store")
            .header("X-Content-Type-Options", "nosniff")
            .header("Referrer-Policy", "no-referrer");
        if let Some(c) = csp {
            r = r.header("Content-Security-Policy", c);
        }
        r.body(Cow::Owned(body)).unwrap()
    }

    impl HtmlView {
        /// `on_link` receives every URL the user clicks.
        pub fn new<W: wry::raw_window_handle::HasWindowHandle>(
            window: &W,
            data_dir: std::path::PathBuf,
            on_link: impl Fn(String) + 'static,
        ) -> Result<HtmlView, String> {
            let page: Arc<Mutex<Option<(u64, Page)>>> = Arc::new(Mutex::new(None));
            let served = page.clone();
            let mut context = WebContext::new(Some(data_dir));
            let on_link = std::rc::Rc::new(on_link);
            let on_link2 = on_link.clone();
            let webview = WebViewBuilder::new_with_web_context(&mut context)
                .with_custom_protocol("mailview".into(), move |_id, req| {
                    let path = req.uri().path().to_string();
                    let guard = served.lock().unwrap();
                    let Some((gen, page)) = guard.as_ref() else {
                        return respond(StatusCode::NOT_FOUND, "text/plain", vec![], None);
                    };
                    let prefix = format!("/m/{gen}/");
                    let Some(rest) = path.strip_prefix(&prefix) else {
                        return respond(StatusCode::NOT_FOUND, "text/plain", vec![], None);
                    };
                    if rest.is_empty() {
                        return respond(
                            StatusCode::OK,
                            "text/html; charset=utf-8",
                            page.html.clone().into_bytes(),
                            Some(csp(page.allow_remote)),
                        );
                    }
                    if let Some(cid) = rest.strip_prefix("cid/") {
                        let cid = percent_encoding::percent_decode_str(cid).decode_utf8_lossy();
                        if let Some((mime, part)) = page.cids.get(&cid_key(&cid)) {
                            if let Some(bytes) = crate::view::part_bytes(&page.raw, *part) {
                                return respond(StatusCode::OK, mime, bytes, None);
                            }
                        }
                    }
                    respond(StatusCode::NOT_FOUND, "text/plain", vec![], None)
                })
                .with_navigation_handler(move |url| {
                    if url.starts_with(ORIGIN) || url == "about:blank" {
                        return true;
                    }
                    on_link(url);
                    false
                })
                .with_new_window_req_handler(move |url, _| {
                    on_link2(url);
                    NewWindowResponse::Deny
                })
                .with_devtools(false)
                .with_autoplay(false)
                .with_visible(false)
                .with_bounds(Rect { position: PhysicalPosition::new(0, 0).into(), size: PhysicalSize::new(1u32, 1u32).into() })
                .build_as_child(window)
                .map_err(|e| format!("The system web view isn't available ({e})"))?;
            Ok(HtmlView { webview, _context: context, page, generation: 0, shown: None, visible: false, bounds: None })
        }

        /// Show a page. `key` identifies the content; unchanged keys don't reload.
        pub fn show(&mut self, key: u64, page: impl FnOnce() -> Page) {
            if self.shown == Some(key) {
                return;
            }
            self.generation += 1;
            *self.page.lock().unwrap() = Some((self.generation, page()));
            let _ = self.webview.load_url(&format!("{ORIGIN}/m/{}/", self.generation));
            self.shown = Some(key);
        }

        /// Position in physical pixels relative to the window's client area.
        pub fn place(&mut self, x: i32, y: i32, w: u32, h: u32) {
            let b = (x, y, w.max(1), h.max(1));
            if self.bounds != Some(b) {
                self.bounds = Some(b);
                let _ = self
                    .webview
                    .set_bounds(Rect { position: PhysicalPosition::new(b.0, b.1).into(), size: PhysicalSize::new(b.2, b.3).into() });
            }
        }

        pub fn set_visible(&mut self, visible: bool) {
            if self.visible != visible {
                self.visible = visible;
                let _ = self.webview.set_visible(visible);
                if !visible {
                    // Give keyboard focus back to the main window.
                    let _ = self.webview.focus_parent();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepares_display_copy() {
        let html = r#"<META HTTP-EQUIV="Refresh" CONTENT="0; url=https://evil.example"><img src="cid:logo@x"><div style="background:url('cid:bg')">"#;
        let out = prepare(html);
        assert!(!out.to_lowercase().contains("refresh"));
        assert!(out.contains(r#"src="cid/logo@x""#));
        assert!(out.contains("url('cid/bg')"));
    }

    #[test]
    fn detects_remote_content() {
        assert!(has_remote_content(r#"<img src="https://t.example/p.gif">"#));
        assert!(has_remote_content(r#"<td background='http://x/y.png'>"#));
        assert!(has_remote_content(r#"<div style="background-image: url(https://x/y.png)">"#));
        assert!(has_remote_content(r#"<link rel=stylesheet href="https://x/s.css">"#));
        assert!(!has_remote_content(r#"<a href="https://example.org">link</a><img src="cid:a">"#));
    }

    #[test]
    fn policy_blocks_scripts_and_remote_by_default() {
        let p = csp(false);
        assert!(p.contains("script-src 'none'"));
        assert!(p.contains("img-src 'self' data:;"));
        assert!(csp(true).contains("img-src 'self' data: https: http:"));
    }
}
