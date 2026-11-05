//! The user interface. Everything here runs on the UI thread and only ever
//! talks to the network through the per-account workers, so the window stays
//! responsive no matter how slow a server is.

use crate::compose::{self, Compose};
use crate::config::{Account, AuthKind, Config, Provider, Security, Server};
use crate::drafts;
use crate::kit::{self, ic, Tone};
use crate::model::{self, FolderIndex, FolderInfo, MsgFlag, Special, Summary};
use crate::store::Store;
use crate::theme::{self, ty};
use crate::update::{self, Staged};
use crate::view::{self, Block, View};
use crate::watcher::{self, Watcher};
use crate::worker::{self, AcctState, Cmd, Ev, Event, OpErr, OpId, Shared};
use crate::{oauth, secrets};
use eframe::egui::{
    self, Align, Align2, Color32, FontFamily, FontId, Id, Key as K, Layout, Modifiers, Order, Rect, RichText, Sense, Stroke,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

const ROW_H: f32 = 60.0;
const ROW_H_COMPACT: f32 = 48.0;
const BODY_CACHE: usize = 48;
const UNDO_WINDOW: Duration = Duration::from_secs(10);
const TOAST_FOR: Duration = Duration::from_secs(6);
const PERSIST_AFTER: Duration = Duration::from_millis(1500);
/// File types that are never opened directly from an email (save only).
const RISKY_EXT: &[&str] = &[
    "exe", "com", "bat", "cmd", "scr", "pif", "msi", "msp", "js", "jse", "vbs", "vbe", "wsf", "wsh", "ps1", "psm1", "hta", "lnk", "jar",
    "reg", "cpl", "dll", "iso", "img", "vhd", "appx", "msix", "sh", "app", "url", "scf", "chm",
];
const SHORTCUTS: &[(&str, &str)] = &[
    ("J / ↓, K / ↑", "Next / previous message"),
    ("PgDn / PgUp", "Jump 10 messages"),
    ("C or N", "Compose"),
    ("R / A / F", "Reply / Reply all / Forward"),
    ("E", "Archive"),
    ("Del, Backspace, #", "Delete"),
    ("Ctrl+Z", "Undo archive / delete / move"),
    ("U / S", "Toggle read / star"),
    ("/", "Filter (Enter searches the server, Esc clears)"),
    ("F5", "Check for new mail"),
    ("Ctrl+Enter", "Send (in a compose window)"),
    ("Tab / Shift+Tab", "Move between fields"),
    ("?", "Show this list"),
];

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Key {
    acct: usize,
    folder: String,
    uid: u32,
}

#[derive(Clone, PartialEq, Debug)]
enum Sel {
    Unified,
    Folder(usize, String),
    Search(String),
}

struct Row {
    key: Key,
    s: Summary,
}

struct SearchState {
    req: u64,
    folder: String,
    uv: u32,
    query: String,
    msgs: Vec<Summary>,
    total: usize,
    loading: bool,
}

struct Acct {
    cfg: Account,
    /// Incremented whenever the connection is restarted; stale events are ignored.
    gen: u64,
    worker: Option<worker::Handle>,
    watcher: Option<Watcher>,
    shared: Arc<Shared>,
    store: Store,
    folders: Vec<FolderInfo>,
    idx: HashMap<String, FolderIndex>,
    /// Server (unseen, total) per folder from STATUS.
    counts: HashMap<String, (u32, u32)>,
    busy: bool,
    state: AcctState,
    push: bool,
    last_sync: Option<SystemTime>,
    search: Option<SearchState>,
    /// Removed this session. Kept as a tombstone so account indexes never shift.
    removed: bool,
}

impl Acct {
    fn live(&self) -> bool {
        !self.removed
    }
    fn special(&self, s: Special) -> Option<String> {
        self.folders.iter().find(|f| f.special == Some(s) && f.selectable).map(|f| f.name.clone())
    }
    fn inbox(&self) -> String {
        self.special(Special::Inbox).unwrap_or_else(|| "INBOX".into())
    }
    fn folder_special(&self, name: &str) -> Option<Special> {
        if name.eq_ignore_ascii_case("INBOX") {
            return Some(Special::Inbox);
        }
        self.folders.iter().find(|f| f.name == name).and_then(|f| f.special)
    }
    fn folder_display(&self, name: &str) -> String {
        self.folders.iter().find(|f| f.name == name).map(|f| f.display.clone()).unwrap_or_else(|| model::decode_mutf7(name))
    }
    fn ensure_index(&mut self, folder: &str) {
        if !self.idx.contains_key(folder) {
            if let Some(i) = self.store.load_index(folder) {
                self.idx.insert(folder.to_string(), i);
            }
        }
    }
    fn unread(&self, folder: &str) -> usize {
        match (self.idx.get(folder), self.counts.get(folder)) {
            (Some(i), _) if i.unseen.is_some() => i.unread(),
            (_, Some((u, _))) => *u as usize,
            (Some(i), None) => i.unread(),
            (None, None) => 0,
        }
    }
    fn uv(&self, folder: &str) -> u32 {
        self.idx
            .get(folder)
            .map(|i| i.uidvalidity)
            .or_else(|| self.search.as_ref().filter(|s| s.folder == folder).map(|s| s.uv))
            .unwrap_or(0)
    }
    fn send(&self, c: Cmd) -> bool {
        match &self.worker {
            Some(w) => {
                w.send(c);
                true
            }
            None => false,
        }
    }
}

enum UpdateState {
    Idle,
    Checking,
    UpToDate,
    Available(update::Release),
    Downloading(String),
    Ready,
    Failed(String),
}

enum OAuthState {
    Idle,
    Waiting,
    Done(String),
    Failed(String),
}

struct Editor {
    id: u64,
    editing: Option<usize>,
    provider: Provider,
    name: String,
    email: String,
    username: String,
    username_touched: bool,
    password: String,
    auth: AuthKind,
    imap: Server,
    smtp: Server,
    client_id: String,
    client_secret: String,
    save_sent: bool,
    signature: String,
    aliases: String,
    notify: bool,
    oauth: OAuthState,
    error: Option<String>,
    /// Account id reserved by the setup wizard (credentials are stored under it while verifying).
    new_id: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SetupStep {
    Email,
    Detecting,
    SignIn,
    Manual,
    Verify,
    Done,
}

#[derive(Clone, PartialEq)]
enum Check {
    Waiting,
    Running,
    Ok,
    Failed(String),
}

/// The add-account wizard: Email → Sign in → Verify → Done.
struct Setup {
    id: u64,
    step: SetupStep,
    ed: Editor,
    detected: String,
    show_password: bool,
    incoming: Check,
    outgoing: Check,
    added: Option<usize>,
    error: Option<String>,
    /// Verification hit something only the user's IT admin can fix.
    admin: Option<crate::admin::Issue>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsPage {
    Appearance,
    Accounts,
    Reading,
    Privacy,
    Notifications,
    Sync,
    Advanced,
    About,
}

/// Where Settings goes once unsaved account edits are resolved.
#[derive(Clone, Copy)]
enum Leave {
    Page(SettingsPage),
    Back,
    Close,
}

enum Confirm {
    RemoveAccount(usize),
    DeleteForever(Vec<Key>),
    CloseCompose(u64),
}

/// An archive/delete/move waiting out its undo window.
struct Pending {
    label: String,
    moves: Vec<(Key, Option<String>)>,
    deadline: Instant,
}

/// What an in-flight server operation was, so its result can be applied.
enum Op {
    Triage(Vec<Key>),
    Flag(Vec<Key>, MsgFlag, bool),
    SentCopy,
    DraftSave { compose: u64, version: u32, hash: u64 },
    DraftDelete,
}

enum Action {
    Select(Sel),
    Open(Key),
    Flag(Vec<Key>, MsgFlag, bool),
    Trash(Vec<Key>),
    Archive(Vec<Key>),
    MoveTo(Vec<Key>, String),
    Undo,
    Compose,
    ComposeTo(String),
    Reply(bool),
    Forward,
    EditAsNew(bool),
    Refresh,
    Retry(usize),
    Pause(usize, bool),
    LoadMore,
    ServerSearch,
    AddAccount,
    EditAccount(usize),
    RemoveAccount(usize),
    ViewSource,
    SaveEml,
    OpenHtml,
    SaveAttachment(usize),
    OpenAttachment(usize),
    Send(u64),
    SaveDraft(u64, bool),
    AttachFiles(u64),
    RequestClose(u64),
    CloseNow(u64),
    Move(i32),
    Back,
    ToggleHtml(Key),
    AllowRemote(Key),
    OpenNested(usize),
    AddToCalendar,
    SaveIcs,
    Unsubscribe,
    CloseAccountDetail,
}

pub struct App {
    cfg: Config,
    accts: Vec<Acct>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    ctx: egui::Context,
    focused: Arc<AtomicBool>,
    was_focused: bool,
    next_gen: u64,
    sel: Sel,
    rows: Vec<Row>,
    rows_dirty: bool,
    /// Messages removed locally but not yet confirmed (or still undoable).
    hidden: HashSet<Key>,
    pending: Vec<Pending>,
    ops: HashMap<OpId, (usize, Op)>,
    cur: Option<Key>,
    view: Option<View>,
    loading: Option<Key>,
    view_error: Option<String>,
    bodies: HashMap<Key, Arc<Vec<u8>>>,
    body_order: VecDeque<Key>,
    filter: String,
    focus_filter: bool,
    search_req: u64,
    composes: Vec<Compose>,
    /// compose id -> (last seen content hash, when it changed)
    edit_track: HashMap<u64, (u64, Instant)>,
    next_id: u64,
    editor: Option<Editor>,
    settings_open: bool,
    shortcuts_open: bool,
    source: Option<(String, String)>,
    confirm: Option<Confirm>,
    status: String,
    last_folder_poll: Instant,
    scroll_to_cur: bool,
    list_scroll: (f32, f32),
    title_unread: usize,
    mono: bool,
    quitting: bool,
    force_quit: bool,
    update: UpdateState,
    staged: Option<Staged>,
    started: Instant,
    last_update_check: Option<Instant>,
    restart_requested: bool,
    /// Folder pane shown (wide windows) / drawer open (narrow windows).
    nav_open: bool,
    nav_drawer: bool,
    /// Multi-selection in the message list (Ctrl/Shift+click) and its anchor row.
    selection: HashSet<Key>,
    sel_anchor: Option<usize>,
    /// Server search scope: true = all inboxes, false = folder on screen.
    search_all: bool,
    /// Transient message shown as a toast (replaces the old status bar).
    toast_text: String,
    toast_at: Option<Instant>,
    last_status: String,
    banner_dismissed: bool,
    setup: Option<Setup>,
    settings_page: SettingsPage,
    /// Fingerprint of the account editor when opened, to detect unsaved edits.
    editor_baseline: Option<String>,
    leave_pending: Option<Leave>,
    /// Attached emails opened on top of the selected one.
    nested: Vec<View>,
    /// Bumped whenever the message on screen changes.
    view_seq: u64,
    previews: HashMap<(u64, mail_parser::PartId), Preview>,
    /// Messages whose remote content the user allowed (this session).
    remote_ok: HashSet<Key>,
    /// Messages shown in the non-default (HTML vs text) mode.
    text_toggle: HashSet<Key>,
    fonts_loaded: HashSet<&'static str>,
    #[cfg(any(windows, target_os = "macos"))]
    html_view: Option<crate::html::HtmlView>,
    html_error: Option<String>,
    html_target: Option<(Rect, u64)>,
    acts: Vec<Action>,
}

enum Preview {
    Loading,
    Ready(egui::TextureHandle),
    Failed,
}

// ---------------------------------------------------------------- setup ---

/// Turn a raw sign-in failure into advice a person can act on.
fn explain_signin_error(e: &str, provider: Provider) -> String {
    if let Some(issue) = crate::admin::classify(e, "", "") {
        return issue.short.to_string();
    }
    let l = e.to_ascii_lowercase();
    let app_pw = matches!(provider, Provider::Gmail | Provider::ICloud | Provider::Yahoo | Provider::Fastmail);
    if l.contains("authenticat")
        || l.contains("invalid credentials")
        || l.contains("login")
        || l.contains("sign-in")
        || l.contains("535")
        || l.contains("password")
    {
        if app_pw {
            format!("The password was rejected. {} needs an app password, not your normal password.", provider.label())
        } else {
            "The user name or password was rejected. Check them and try again.".into()
        }
    } else if l.contains("resolve") || l.contains("dns") || l.contains("no such host") {
        "Couldn't find that server. Check the server name in the server settings.".into()
    } else if l.contains("tls") || l.contains("certificate") || l.contains("handshake") {
        "A secure connection couldn't be established. Check the security setting and port.".into()
    } else if l.contains("timed out") || l.contains("connect") {
        "Couldn't reach the server. Check your connection, the server name and port.".into()
    } else {
        e.to_string()
    }
}

/// An admin-only problem: what to ask for, plus the link or command to forward.
fn admin_card(ui: &mut egui::Ui, issue: &crate::admin::Issue) {
    let p = kit::pal(ui);
    if kit::notice(ui, Tone::Warning, &issue.explain, Some(issue.copy_label)) {
        ui.ctx().copy_text(issue.copy_text.clone());
        ui.ctx().data_mut(|d| d.insert_temp(Id::unique("admin-copied"), ui.ctx().input(|i| i.time)));
    }
    ui.add_space(6.0);
    egui::Frame::new()
        .fill(p.input)
        .stroke(Stroke::new(1.0, p.divider))
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(
                egui::Label::new(RichText::new(&issue.copy_text).font(FontId::monospace(ty::META)).color(p.text)).wrap().selectable(true),
            );
        });
    let copied = ui.ctx().data(|d| d.get_temp::<f64>(Id::unique("admin-copied")));
    if copied.is_some_and(|t| ui.ctx().input(|i| i.time) - t < 2.0) {
        ui.label(RichText::new("Copied to the clipboard").size(ty::META).color(p.success));
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
    }
}

/// A borderless single-line field for the compose rows.
fn flat_edit<'a>(val: &'a mut String, hint: &str, w: f32) -> egui::TextEdit<'a> {
    egui::TextEdit::singleline(val)
        .hint_text(hint)
        .frame(egui::Frame::NONE)
        .margin(egui::Margin::symmetric(0, 0))
        .align(Align2::LEFT_CENTER)
        .min_size(egui::vec2(0.0, 40.0))
        .desired_width(w)
}

/// "Name <addr>" -> (name, addr); a bare address gives ("", addr).
fn split_mailbox(s: &str) -> (String, String) {
    match (s.rfind('<'), s.rfind('>')) {
        (Some(a), Some(b)) if a < b => (s[..a].trim().trim_matches('"').to_string(), s[a + 1..b].trim().to_string()),
        _ => (String::new(), s.trim().to_string()),
    }
}

/// Short human file type for attachment tiles ("PDF", "PNG image", …).
fn file_kind(name: &str, mime: &str) -> String {
    match mime {
        "message/rfc822" => return "Email".into(),
        "text/calendar" => return "Invitation".into(),
        m if m.starts_with("image/") => return format!("{} image", m.trim_start_matches("image/").to_uppercase()),
        _ => {}
    }
    match name.rsplit_once('.') {
        Some((_, ext)) if ext.len() <= 5 => ext.to_uppercase(),
        _ => "File".into(),
    }
}

/// Window icon: an envelope on the app colour, drawn procedurally.
pub fn icon() -> egui::IconData {
    let n = 64usize;
    let rgba = crate::appicon::render(n);
    egui::IconData { rgba, width: n as u32, height: n as u32 }
}

/// What the trailing hover actions on a list row asked for.
enum RowAct {
    Archive,
    Delete,
    ToggleRead,
    ToggleStar,
}

struct RowState {
    selected: bool,
    checked: bool,
    compact: bool,
    acct_label: String,
}

fn paint_row(ui: &mut egui::Ui, r: &Row, st: &RowState) -> (egui::Response, Option<RowAct>) {
    let p = kit::pal(ui);
    let h = if st.compact { ROW_H_COMPACT } else { ROW_H };
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), Sense::click());
    // Screen readers: one selectable item per message with everything it shows.
    let label = format!(
        "{}{}{}{}, {}, {}{}",
        if r.s.seen { "" } else { "Unread, " },
        if r.s.flagged { "starred, " } else { "" },
        if r.s.attach { "has attachment, " } else { "" },
        r.s.sender(),
        if r.s.subject.is_empty() { "no subject" } else { &r.s.subject },
        model::fmt_date_long(r.s.date),
        if st.acct_label.is_empty() { String::new() } else { format!(", {}", st.acct_label) }
    );
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::ListBoxOption, true, st.selected || st.checked, &label));
    let mut act = None;
    if !ui.is_rect_visible(rect) {
        return (resp, act);
    }
    let hovered = ui.rect_contains_pointer(rect);
    let painter = ui.painter().clone();
    if st.selected || st.checked {
        painter.rect_filled(rect, 0.0, p.selected);
        painter.rect_filled(Rect::from_min_size(rect.min, egui::vec2(3.0, h)), 0.0, p.accent);
    } else if hovered {
        painter.rect_filled(rect, 0.0, p.hover);
    }
    if resp.has_focus() {
        painter.rect_stroke(rect.shrink(1.0), egui::CornerRadius::same(4), Stroke::new(2.0, p.accent), egui::StrokeKind::Inside);
    }
    let unread = !r.s.seen;
    let left = rect.left() + 24.0;
    let right = rect.right() - 16.0;
    if unread {
        painter.circle_filled(egui::pos2(rect.left() + 12.0, rect.top() + if st.compact { 16.0 } else { 20.0 }), 4.0, p.accent);
    }
    let line1_y = rect.top() + if st.compact { 7.0 } else { 10.0 };
    let line2_y = rect.top() + if st.compact { 26.0 } else { 32.0 };

    // Trailing area: date + indicators, or hover actions (same width, so text never shifts).
    let trailing_w = 136.0;
    let trailing = Rect::from_min_max(egui::pos2(right - trailing_w, rect.top()), egui::pos2(right, rect.top() + 30.0));
    if hovered {
        let acts: [(egui_material_icons::MaterialIcon, &str, RowAct); 4] = [
            (ic::ICON_ARCHIVE, "Archive (E)", RowAct::Archive),
            (ic::ICON_DELETE, "Delete (Del)", RowAct::Delete),
            (
                if unread { ic::ICON_DRAFTS } else { ic::ICON_MARK_EMAIL_UNREAD },
                if unread { "Mark as read (U)" } else { "Mark as unread (U)" },
                RowAct::ToggleRead,
            ),
            (ic::ICON_STAR, if r.s.flagged { "Unstar (S)" } else { "Star (S)" }, RowAct::ToggleStar),
        ];
        let mut x = right - 4.0 * 32.0;
        for (icon, tip, a) in acts {
            let br = Rect::from_min_size(egui::pos2(x, rect.top() + (if st.compact { 2.0 } else { 6.0 })), egui::vec2(32.0, 32.0));
            let id = resp.id.with(tip);
            let b = ui.interact(br, id, Sense::click()).on_hover_text(tip);
            if b.hovered() {
                painter.rect_filled(br.shrink(2.0), egui::CornerRadius::same(6), if st.selected { p.hover } else { p.selected });
            }
            let starred = matches!(a, RowAct::ToggleStar) && r.s.flagged;
            let color = if starred { p.warning } else { p.text_secondary };
            let font = FontId::new(18.0, if starred { icon.filled().font_family() } else { icon.outlined().font_family() });
            painter.text(br.center(), Align2::CENTER_CENTER, <&str>::from(icon), font, color);
            if b.clicked() {
                act = Some(a);
            }
            x += 32.0;
        }
    } else {
        let date = kit::one_line(
            ui,
            model::fmt_date_short(r.s.date),
            theme::regular(ty::META),
            if unread { p.text } else { p.text_secondary },
            120.0,
        );
        let dx = right - date.size().x;
        painter.galley(egui::pos2(dx, line1_y + 1.0), date, p.text);
        let mut ix = dx - 6.0;
        let mut indicator = |icon: egui_material_icons::MaterialIcon, filled: bool, color: Color32| {
            ix -= 18.0;
            let font = FontId::new(16.0, if filled { icon.filled().font_family() } else { icon.outlined().font_family() });
            painter.text(egui::pos2(ix + 8.0, line1_y + 9.0), Align2::CENTER_CENTER, <&str>::from(icon), font, color);
        };
        if r.s.flagged {
            indicator(ic::ICON_STAR, true, p.warning);
        }
        if r.s.attach {
            indicator(ic::ICON_ATTACH_FILE, false, p.text_secondary);
        }
        if r.s.answered {
            indicator(ic::ICON_REPLY, false, p.text_secondary);
        }
    }
    let sender = if r.s.sender().is_empty() { "(unknown sender)".to_string() } else { r.s.sender().to_string() };
    let from_font = if unread { theme::semibold(ty::UI) } else { theme::regular(ty::UI) };
    let from = kit::one_line(ui, sender, from_font, p.text, (trailing.left() - left - 8.0).max(10.0));
    painter.galley(egui::pos2(left, line1_y), from, p.text);
    let subject = if r.s.subject.is_empty() { "(no subject)".to_string() } else { r.s.subject.clone() };
    let acct_w = if st.acct_label.is_empty() { 0.0 } else { 120.0 };
    let subj = kit::one_line(
        ui,
        subject,
        if unread { theme::semibold(ty::UI - 1.0) } else { theme::regular(ty::UI - 1.0) },
        if unread { p.text } else { p.text_secondary },
        (right - left - acct_w).max(10.0),
    );
    painter.galley(egui::pos2(left, line2_y), subj, p.text);
    if !st.acct_label.is_empty() {
        let g = kit::one_line(ui, st.acct_label.clone(), theme::regular(ty::META), p.text_disabled, acct_w - 8.0);
        painter.galley(egui::pos2(right - g.size().x, line2_y + 1.0), g, p.text);
    }
    painter.hline((rect.left() + 16.0)..=(rect.right()), rect.bottom() - 0.5, Stroke::new(1.0, p.divider));
    (resp, act)
}

fn compose_layer(id: u64) -> egui::LayerId {
    egui::LayerId::new(Order::Middle, Id::unique(("compose", id)))
}

fn smtp_hint(e: &str) -> String {
    let l = e.to_ascii_lowercase();
    if l.contains("timed out") || l.contains("timeout") || l.contains("connection") || l.contains("io error") || l.contains("network") {
        format!("{e}\nIf you're not sure whether it went out, check your Sent folder before sending again.")
    } else {
        e.to_string()
    }
}

// ------------------------------------------------------------------ app ---

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> App {
        let cfg = Config::load();
        theme::install_fonts(&cc.egui_ctx);
        theme::install(&cc.egui_ctx, &cfg.settings.theme);
        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App {
            cfg: cfg.clone(),
            accts: Vec::new(),
            tx,
            rx,
            ctx: cc.egui_ctx.clone(),
            focused: Arc::new(AtomicBool::new(true)),
            was_focused: true,
            next_gen: 1,
            sel: Sel::Unified,
            rows: Vec::new(),
            rows_dirty: true,
            hidden: HashSet::new(),
            pending: Vec::new(),
            ops: HashMap::new(),
            cur: None,
            view: None,
            loading: None,
            view_error: None,
            bodies: HashMap::new(),
            body_order: VecDeque::new(),
            filter: String::new(),
            focus_filter: false,
            search_req: 0,
            composes: Vec::new(),
            edit_track: HashMap::new(),
            next_id: 1,
            editor: None,
            settings_open: false,
            shortcuts_open: false,
            source: None,
            confirm: None,
            status: String::new(),
            last_folder_poll: Instant::now(),
            scroll_to_cur: false,
            list_scroll: (0.0, 0.0),
            title_unread: usize::MAX,
            mono: false,
            quitting: false,
            force_quit: false,
            update: UpdateState::Idle,
            staged: None,
            started: Instant::now(),
            last_update_check: None,
            restart_requested: false,
            nav_open: true,
            nav_drawer: false,
            selection: HashSet::new(),
            sel_anchor: None,
            search_all: false,
            toast_text: String::new(),
            toast_at: None,
            last_status: String::new(),
            banner_dismissed: false,
            setup: None,
            settings_page: SettingsPage::Appearance,
            editor_baseline: None,
            leave_pending: None,
            nested: Vec::new(),
            view_seq: 0,
            previews: HashMap::new(),
            remote_ok: HashSet::new(),
            text_toggle: HashSet::new(),
            fonts_loaded: HashSet::new(),
            #[cfg(any(windows, target_os = "macos"))]
            html_view: None,
            html_error: if cfg!(any(windows, target_os = "macos")) { None } else { Some("no web view on this platform".into()) },
            html_target: None,
            acts: Vec::new(),
        };
        for (i, a) in cfg.accounts.into_iter().enumerate() {
            app.add_account(a, Duration::from_millis(700 * i as u64));
        }
        app.recover_composes();
        if app.live_count() == 0 {
            app.open_editor(None);
        }
        app
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn live_count(&self) -> usize {
        self.accts.iter().filter(|a| a.live()).count()
    }

    fn first_live(&self) -> Option<usize> {
        self.accts.iter().position(|a| a.live())
    }

    fn new_shared(&self, a: &Account) -> Arc<Shared> {
        let s = &self.cfg.settings;
        Arc::new(Shared {
            poll_secs: AtomicU64::new(s.poll_secs),
            push: AtomicBool::new(false),
            notify: AtomicBool::new(s.notifications && a.notify),
            preview: AtomicBool::new(s.notify_preview),
            focused: self.focused.clone(),
        })
    }

    /// Push current settings into every account's live worker settings.
    fn sync_shared(&self) {
        let s = &self.cfg.settings;
        for a in &self.accts {
            a.shared.poll_secs.store(s.poll_secs, Ordering::Relaxed);
            a.shared.notify.store(s.notifications && a.cfg.notify, Ordering::Relaxed);
            a.shared.preview.store(s.notify_preview, Ordering::Relaxed);
        }
    }

    fn add_account(&mut self, a: Account, delay: Duration) {
        let store = Store::new(&a.id);
        let folders = store.load_folders().unwrap_or_default();
        let shared = self.new_shared(&a);
        let paused = a.paused;
        let mut acct = Acct {
            cfg: a,
            gen: 0,
            worker: None,
            watcher: None,
            shared,
            store,
            folders,
            idx: HashMap::new(),
            counts: HashMap::new(),
            busy: false,
            state: AcctState::Connecting,
            push: false,
            last_sync: None,
            search: None,
            removed: false,
        };
        let inbox = acct.inbox();
        acct.ensure_index(&inbox);
        self.accts.push(acct);
        if !paused {
            self.start_conn(self.accts.len() - 1, delay, None);
        }
        self.rows_dirty = true;
    }

    fn start_conn(&mut self, i: usize, delay: Duration, wait_for: Option<JoinHandle<()>>) {
        let gen = self.next_gen;
        self.next_gen += 1;
        let initial_fetch = self.cfg.settings.initial_fetch;
        let (events, ctx) = (self.tx.clone(), self.ctx.clone());
        let a = &mut self.accts[i];
        a.gen = gen;
        a.state = AcctState::Connecting;
        let handle = worker::spawn(worker::Spawn {
            acct: a.cfg.clone(),
            gen,
            initial_fetch,
            start_delay: delay,
            wait_for,
            shared: a.shared.clone(),
            events: events.clone(),
            ctx: ctx.clone(),
        });
        let inbox = a.inbox();
        handle.send(Cmd::Folders);
        handle.send(Cmd::Sync { folder: inbox.clone(), more: false });
        a.watcher = Some(watcher::spawn(watcher::Spawn {
            acct: a.cfg.clone(),
            gen,
            folder: inbox,
            start_delay: delay + Duration::from_secs(3),
            worker: handle.tx.clone(),
            shared: a.shared.clone(),
            events,
            ctx,
        }));
        a.worker = Some(handle);
    }

    /// Stop the account's connections; returns the worker thread to wait on.
    fn stop_conn(&mut self, i: usize) -> Option<JoinHandle<()>> {
        let a = &mut self.accts[i];
        a.watcher = None; // dropping stops it
        a.push = false;
        a.shared.push.store(false, Ordering::SeqCst);
        a.busy = false;
        a.worker.take().and_then(|w| w.shutdown())
    }

    fn acct_index(&self, id: &str) -> Option<usize> {
        self.accts.iter().position(|a| a.cfg.id == id && a.live())
    }

    fn shows_folder(&self, i: usize, folder: &str) -> bool {
        match &self.sel {
            Sel::Unified => self.accts[i].inbox() == folder,
            Sel::Folder(a, f) => *a == i && f == folder,
            Sel::Search(_) => false,
        }
    }

    fn cache_body(&mut self, key: Key, raw: Arc<Vec<u8>>) {
        if self.bodies.insert(key.clone(), raw).is_none() {
            self.body_order.push_back(key);
            while self.body_order.len() > BODY_CACHE {
                if let Some(old) = self.body_order.pop_front() {
                    self.bodies.remove(&old);
                }
            }
        }
    }

    fn save_config(&mut self) {
        self.cfg.accounts = self.accts.iter().filter(|a| a.live()).map(|a| a.cfg.clone()).collect();
        if let Err(e) = self.cfg.save() {
            self.status = e;
        }
    }

    // ---------------------------------------------------------- events ---

    fn handle_events(&mut self) {
        while let Ok(Event { acct, gen, kind }) = self.rx.try_recv() {
            // Acknowledgements are matched by operation id, whichever worker sent them.
            if let Ev::Done { op, result } = kind {
                if let Some((ai, op_kind)) = self.ops.remove(&op) {
                    self.op_done(ai, op_kind, result);
                }
                continue;
            }
            match kind {
                Ev::Sent { compose, result } => {
                    self.sent(compose, result);
                    continue;
                }
                Ev::Autoconfig { setup, found } => {
                    let settings = self.cfg.settings.clone();
                    if let Some(st) = self.setup.as_mut().filter(|x| x.id == setup && x.step == SetupStep::Detecting) {
                        match found {
                            Some(crate::autoconfig::Found::Provider(pv, source)) => {
                                Self::apply_preset(&mut st.ed, pv, &settings);
                                st.detected = format!("{} · found via {source}", pv.label());
                                st.step = SetupStep::SignIn;
                            }
                            Some(crate::autoconfig::Found::Servers { imap, smtp, local_user, source }) => {
                                Self::apply_preset(&mut st.ed, Provider::Custom, &settings);
                                st.ed.imap = imap;
                                st.ed.smtp = smtp;
                                if local_user {
                                    st.ed.username = st.ed.email.split('@').next().unwrap_or("").to_string();
                                    st.ed.username_touched = true;
                                }
                                st.detected = format!("{} · found via {source}", st.ed.imap.host);
                                st.step = SetupStep::SignIn;
                            }
                            None => {
                                Self::apply_preset(&mut st.ed, Provider::Custom, &settings);
                                st.detected = "We couldn't find settings for this address automatically.".into();
                                st.step = SetupStep::Manual;
                            }
                        }
                    }
                    continue;
                }
                Ev::Verified { setup, imap, smtp } => {
                    let mut done = false;
                    if let Some(st) = self.setup.as_mut().filter(|x| x.id == setup) {
                        let (cid, email) = (st.ed.client_id.clone(), st.ed.email.trim().to_string());
                        st.admin =
                            [&imap, &smtp].into_iter().find_map(|r| r.as_ref().err().and_then(|e| crate::admin::classify(e, &cid, &email)));
                        st.incoming = match imap {
                            Ok(()) => Check::Ok,
                            Err(e) => Check::Failed(explain_signin_error(&e, st.ed.provider)),
                        };
                        st.outgoing = match smtp {
                            Ok(()) => Check::Ok,
                            Err(e) => Check::Failed(explain_signin_error(&e, st.ed.provider)),
                        };
                        done = st.incoming == Check::Ok && st.outgoing == Check::Ok;
                    }
                    if done {
                        self.finish_setup();
                    }
                    continue;
                }
                Ev::Update(msg) => {
                    self.update = match msg {
                        update::Msg::UpToDate => UpdateState::UpToDate,
                        update::Msg::Available(r) => UpdateState::Available(r),
                        update::Msg::Downloading(v) => UpdateState::Downloading(v.to_string()),
                        update::Msg::Ready(s) => {
                            self.staged = Some(s);
                            UpdateState::Ready
                        }
                        update::Msg::Failed(e) => UpdateState::Failed(e),
                    };
                    continue;
                }
                Ev::Link(url) => {
                    let lower = url.to_ascii_lowercase();
                    if lower.starts_with("mailto:") {
                        self.acts.push(Action::ComposeTo(url));
                    } else if lower.starts_with("https://") || lower.starts_with("http://") {
                        if let Err(e) = open::that(&url) {
                            self.status = format!("Couldn't open link: {e}");
                        }
                    }
                    continue;
                }
                Ev::Preview { seq, part, image } => {
                    if seq == self.view_seq {
                        let p = match image {
                            Some(img) => {
                                Preview::Ready(self.ctx.load_texture(format!("preview-{seq}-{part}"), img, egui::TextureOptions::LINEAR))
                            }
                            None => Preview::Failed,
                        };
                        self.previews.insert((seq, part), p);
                    }
                    continue;
                }
                Ev::OAuth { editor, result } => {
                    let wizard = self.setup.as_mut().map(|s| &mut s.ed).filter(|e| e.id == editor);
                    if let Some(ed) = wizard.or(self.editor.as_mut().filter(|e| e.id == editor)) {
                        ed.oauth = match result {
                            Ok(t) => OAuthState::Done(t),
                            Err(e) => OAuthState::Failed(e),
                        };
                    }
                    continue;
                }
                _ => {}
            }
            let Some(i) = self.acct_index(&acct).filter(|&i| self.accts[i].gen == gen) else { continue };
            match kind {
                Ev::Folders(folders) => {
                    let a = &mut self.accts[i];
                    a.folders = folders;
                    let inbox = a.inbox();
                    a.ensure_index(&inbox);
                    self.rows_dirty = true;
                }
                Ev::Index { folder, index } => {
                    self.accts[i].idx.insert(folder.clone(), index);
                    if self.shows_folder(i, &folder) {
                        self.rows_dirty = true;
                    }
                }
                Ev::Counts(list) => {
                    let a = &mut self.accts[i];
                    for (f, u, t) in list {
                        a.counts.insert(f, (u, t));
                    }
                }
                Ev::Body { folder, uv, uid, raw } => {
                    let key = Key { acct: i, folder, uid };
                    let current_uv = self.accts[i].uv(&key.folder);
                    if current_uv != 0 && uv != current_uv {
                        if self.loading.as_ref() == Some(&key) {
                            self.loading = None;
                            self.view_error = Some("This folder changed on the server. Refreshing…".into());
                        }
                        continue;
                    }
                    self.cache_body(key.clone(), raw.clone());
                    if self.loading.as_ref() == Some(&key) {
                        self.loading = None;
                        if self.cur.as_ref() == Some(&key) {
                            self.set_view(Some(view::parse(raw)));
                        }
                    }
                }
                Ev::BodyFailed { folder, uid, error } => {
                    let key = Key { acct: i, folder, uid };
                    if self.loading.as_ref() == Some(&key) {
                        self.loading = None;
                        self.view_error = Some(error);
                    }
                }
                Ev::Search { req, folder, uv, msgs, total } => {
                    if let Some(s) = self.accts[i].search.as_mut().filter(|s| s.req == req) {
                        s.folder = folder;
                        s.uv = uv;
                        s.msgs = msgs;
                        s.total = total;
                        s.loading = false;
                        self.rows_dirty = true;
                    }
                }
                Ev::Busy(b) => self.accts[i].busy = b,
                Ev::State(s) => {
                    if let AcctState::AuthFailed(e) | AcctState::Offline(e) = &s {
                        self.status = format!("{}: {e}", self.accts[i].cfg.label());
                    }
                    self.accts[i].state = s;
                }
                Ev::Synced { folder, at } => {
                    let a = &mut self.accts[i];
                    if folder == a.inbox() {
                        a.last_sync = Some(at);
                    }
                }
                Ev::Error(e) => self.status = format!("{}: {e}", self.accts[i].cfg.label()),
                Ev::Push(on) => self.accts[i].push = on,
                Ev::NotifyClick { folder, uid } => {
                    self.select(Sel::Folder(i, folder.clone()));
                    self.rebuild_rows();
                    let key = Key { acct: i, folder, uid };
                    self.open(key);
                    self.scroll_to_cur = true;
                }
                Ev::Done { .. }
                | Ev::Sent { .. }
                | Ev::OAuth { .. }
                | Ev::Update(_)
                | Ev::Autoconfig { .. }
                | Ev::Verified { .. }
                | Ev::Link(_)
                | Ev::Preview { .. } => {}
            }
        }
    }

    fn op_done(&mut self, ai: usize, op: Op, result: Result<Option<String>, OpErr>) {
        let label = self.accts.get(ai).map(|a| a.cfg.label().to_string()).unwrap_or_default();
        match op {
            Op::Triage(keys) => {
                match &result {
                    Ok(note) => {
                        for k in &keys {
                            if let Some(a) = self.accts.get_mut(k.acct) {
                                if let Some(ix) = a.idx.get_mut(&k.folder) {
                                    if ix.msgs.iter().any(|m| m.uid == k.uid) {
                                        ix.msgs.retain(|m| m.uid != k.uid);
                                        ix.exists = ix.exists.saturating_sub(1);
                                    }
                                }
                                if let Some(s) = a.search.as_mut().filter(|s| s.folder == k.folder) {
                                    s.msgs.retain(|m| m.uid != k.uid);
                                }
                            }
                        }
                        if let Some(n) = note {
                            self.status = n.clone();
                        }
                    }
                    Err(e) => {
                        self.status = format!("{label}: {}", e.msg);
                        self.resync(&keys);
                    }
                }
                for k in &keys {
                    self.hidden.remove(k);
                }
                self.rows_dirty = true;
            }
            Op::Flag(keys, flag, on) => {
                if let Err(e) = result {
                    self.status = format!("{label}: {}", e.msg);
                    self.apply_flags_local(&keys, flag, !on);
                    self.resync(&keys);
                }
            }
            Op::SentCopy => match result {
                Ok(_) => {}
                Err(e) => self.status = format!("Message sent, but saving a copy to Sent failed: {}", e.msg),
            },
            Op::DraftSave { compose, version, hash } => {
                let Some(pos) = self.composes.iter().position(|c| c.id == compose) else { return };
                let c = &mut self.composes[pos];
                c.saving = None;
                match result {
                    Ok(note) => {
                        c.version = c.version.max(version);
                        c.replaces = None;
                        c.clean = hash;
                        c.error = None;
                        c.note = Some(note.unwrap_or_else(|| "Draft saved".into()));
                        if c.close_after_save {
                            let key = c.key.clone();
                            self.composes.remove(pos);
                            drafts::delete(&key);
                            self.status = "Draft saved".into();
                        } else {
                            Self::persist(c, &self.accts);
                        }
                    }
                    Err(e) => {
                        // A draft that might have been saved is harmless: the next save
                        // replaces every version with the same draft key.
                        if e.unknown {
                            c.version = c.version.max(version);
                        }
                        c.close_after_save = false;
                        c.error = Some(format!("Couldn't save the draft to the server: {}. It's kept on this computer.", e.msg));
                    }
                }
            }
            Op::DraftDelete => {
                if let Err(e) = result {
                    self.status = format!("Message sent, but removing the saved draft failed: {}", e.msg);
                }
            }
        }
    }

    /// After a failed or uncertain change, fetch the server's view again.
    fn resync(&mut self, keys: &[Key]) {
        let mut done = HashSet::new();
        for k in keys {
            if done.insert((k.acct, k.folder.clone())) {
                if let Some(a) = self.accts.get(k.acct) {
                    a.send(Cmd::Sync { folder: k.folder.clone(), more: false });
                }
            }
        }
        if let Sel::Search(q) = self.sel.clone() {
            if keys.iter().any(|k| self.accts.get(k.acct).and_then(|a| a.search.as_ref()).is_some_and(|s| s.folder == k.folder)) {
                self.filter = q;
                self.server_search();
            }
        }
    }

    fn sent(&mut self, compose: u64, result: Result<Vec<u8>, String>) {
        let Some(pos) = self.composes.iter().position(|c| c.id == compose) else { return };
        match result {
            Ok(data) => {
                let c = self.composes.remove(pos);
                drafts::delete(&c.key);
                self.edit_track.remove(&c.id);
                self.status = "Message sent".into();
                if let Some(a) = self.accts.get(c.acct).filter(|a| a.live()) {
                    if a.cfg.save_sent {
                        if let Some(folder) = a.special(Special::Sent) {
                            let op = self.next_id + 1_000_000;
                            self.next_id += 1;
                            if a.send(Cmd::Append { op, folder, data, flags: vec![MsgFlag::Seen] }) {
                                self.ops.insert(op, (c.acct, Op::SentCopy));
                            } else {
                                self.status = "Message sent. The account is paused, so no copy was saved to Sent.".into();
                            }
                        }
                    }
                    if c.version > 0 || c.replaces.is_some() {
                        if let Some(folder) = a.special(Special::Drafts) {
                            let op = self.next_id + 1_000_000;
                            self.next_id += 1;
                            if a.send(Cmd::DeleteDraft { op, folder, key: c.key.clone(), replaces: c.replaces.clone() }) {
                                self.ops.insert(op, (c.acct, Op::DraftDelete));
                            }
                        }
                    }
                }
                if let Some((a, folder, uid)) = c.reply_of {
                    if self.accts.get(a).is_some_and(|x| x.live()) {
                        self.set_flags(&[Key { acct: a, folder, uid }], MsgFlag::Answered, true);
                    }
                }
            }
            Err(e) => {
                let c = &mut self.composes[pos];
                c.sending = false;
                c.error = Some(smtp_hint(&e));
            }
        }
    }

    fn rebuild_rows(&mut self) {
        self.rows_dirty = false;
        let mut rows = Vec::new();
        match &self.sel {
            Sel::Unified => {
                for (i, a) in self.accts.iter().enumerate().filter(|(_, a)| a.live()) {
                    let inbox = a.inbox();
                    if let Some(ix) = a.idx.get(&inbox) {
                        rows.extend(ix.msgs.iter().map(|s| Row { key: Key { acct: i, folder: inbox.clone(), uid: s.uid }, s: s.clone() }));
                    }
                }
                if self.live_count() > 1 {
                    rows.sort_by_key(|r| std::cmp::Reverse(r.s.date));
                }
            }
            Sel::Folder(i, f) => {
                if let Some(ix) = self.accts.get(*i).filter(|a| a.live()).and_then(|a| a.idx.get(f)) {
                    rows.extend(ix.msgs.iter().map(|s| Row { key: Key { acct: *i, folder: f.clone(), uid: s.uid }, s: s.clone() }));
                }
            }
            Sel::Search(q) => {
                for (i, a) in self.accts.iter().enumerate().filter(|(_, a)| a.live()) {
                    if let Some(s) = a.search.as_ref().filter(|s| &s.query == q) {
                        rows.extend(
                            s.msgs.iter().map(|m| Row { key: Key { acct: i, folder: s.folder.clone(), uid: m.uid }, s: m.clone() }),
                        );
                    }
                }
                rows.sort_by_key(|r| std::cmp::Reverse(r.s.date));
            }
        }
        if !self.hidden.is_empty() {
            rows.retain(|r| !self.hidden.contains(&r.key));
        }
        let f = self.filter.trim().to_lowercase();
        if !f.is_empty() && !matches!(self.sel, Sel::Search(_)) {
            rows.retain(|r| {
                r.s.subject.to_lowercase().contains(&f)
                    || r.s.from_name.to_lowercase().contains(&f)
                    || r.s.from_addr.to_lowercase().contains(&f)
                    || r.s.to.to_lowercase().contains(&f)
            });
        }
        let sample: String = rows.iter().take(400).map(|r| format!("{} {} ", r.s.subject, r.s.sender())).collect();
        self.ensure_fonts(&sample);
        self.rows = rows;
    }

    fn update_title(&mut self) {
        let unread: usize = self.accts.iter().filter(|a| a.live()).map(|a| a.unread(&a.inbox())).sum();
        if unread != self.title_unread {
            self.title_unread = unread;
            let title = if unread > 0 { format!("Mail ({unread})") } else { "Mail".into() };
            self.ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
        }
    }

    /// Workers poll their own inboxes. The UI only refreshes the folder on
    /// screen, and catches up when the window regains focus.
    fn poll(&mut self, focused: bool) {
        let every = Duration::from_secs(self.cfg.settings.poll_secs.max(15));
        if let Sel::Folder(i, f) = &self.sel {
            if self.last_folder_poll.elapsed() >= every {
                self.last_folder_poll = Instant::now();
                if let Some(a) = self.accts.get(*i) {
                    if *f != a.inbox() {
                        a.send(Cmd::Sync { folder: f.clone(), more: false });
                    }
                }
            }
        }
        if focused && !self.was_focused {
            for a in self.accts.iter().filter(|a| a.live()) {
                let stale = a.last_sync.is_none_or(|t| SystemTime::now().duration_since(t).unwrap_or_default() > Duration::from_secs(15));
                if stale {
                    a.send(Cmd::Sync { folder: a.inbox(), more: false });
                }
            }
            if let Sel::Folder(i, f) = &self.sel {
                if let Some(a) = self.accts.get(*i) {
                    a.send(Cmd::Sync { folder: f.clone(), more: false });
                }
            }
        }
        self.was_focused = focused;
        // Updates: first check shortly after launch, then daily.
        // Development builds never update themselves automatically.
        if !cfg!(debug_assertions) && self.cfg.settings.auto_update && update::platform().is_some() && self.staged.is_none() {
            let due = match self.last_update_check {
                None => self.started.elapsed() >= Duration::from_secs(10),
                Some(t) => t.elapsed() >= Duration::from_secs(24 * 3600),
            };
            if due && !matches!(self.update, UpdateState::Checking | UpdateState::Downloading(_)) {
                self.check_updates(true);
            }
        }
        // Undo windows that have expired go to the server now.
        let now = Instant::now();
        let (due, keep): (Vec<Pending>, Vec<Pending>) = std::mem::take(&mut self.pending).into_iter().partition(|p| p.deadline <= now);
        self.pending = keep;
        for p in due {
            self.dispatch(p.moves);
        }
        let mut next = every.saturating_sub(self.last_folder_poll.elapsed());
        if let Some(p) = self.pending.iter().map(|p| p.deadline).min() {
            next = next.min(p.saturating_duration_since(now));
        }
        self.ctx.request_repaint_after(next + Duration::from_millis(30));
    }

    // --------------------------------------------------------- actions ---

    fn summary(&self, key: &Key) -> Option<&Summary> {
        self.rows.iter().find(|r| &r.key == key).map(|r| &r.s)
    }

    fn open(&mut self, key: Key) {
        if self.cur.as_ref() == Some(&key) && (self.view.is_some() || self.loading.is_some()) {
            return;
        }
        if !self.accts.get(key.acct).is_some_and(|a| a.live()) {
            return;
        }
        self.cur = Some(key.clone());
        self.set_view(None);
        self.view_error = None;
        self.loading = None;
        let raw = self.bodies.get(&key).cloned().or_else(|| {
            let a = &self.accts[key.acct];
            let uv = a.idx.get(&key.folder)?.uidvalidity;
            a.store.load_body(&key.folder, uv, key.uid).map(Arc::new)
        });
        match raw {
            Some(raw) => {
                self.cache_body(key.clone(), raw.clone());
                self.set_view(Some(view::parse(raw)));
            }
            None => {
                if self.accts[key.acct].send(Cmd::Body { folder: key.folder.clone(), uid: key.uid }) {
                    self.loading = Some(key.clone());
                } else {
                    self.view_error =
                        Some("This account is paused and the message isn't downloaded yet. Resume the account to read it.".into());
                }
            }
        }
        let connected = self.accts[key.acct].worker.is_some();
        if connected && self.cfg.settings.mark_read_on_open && self.summary(&key).is_some_and(|s| !s.seen) {
            self.set_flags(&[key], MsgFlag::Seen, true);
        }
    }

    fn apply_flags_local(&mut self, keys: &[Key], flag: MsgFlag, on: bool) {
        for k in keys {
            let Some(a) = self.accts.get_mut(k.acct) else { continue };
            if let Some(ix) = a.idx.get_mut(&k.folder) {
                for m in ix.msgs.iter_mut().filter(|m| m.uid == k.uid) {
                    if flag == MsgFlag::Seen && m.seen != on {
                        if let Some(u) = ix.unseen.as_mut() {
                            *u = if on { u.saturating_sub(1) } else { *u + 1 };
                        }
                    }
                    flag.apply(m, on);
                }
            }
            if let Some(s) = a.search.as_mut().filter(|s| s.folder == k.folder) {
                s.msgs.iter_mut().filter(|m| m.uid == k.uid).for_each(|m| flag.apply(m, on));
            }
            self.rows.iter_mut().filter(|r| &r.key == k).for_each(|r| flag.apply(&mut r.s, on));
        }
    }

    fn new_op(&mut self) -> OpId {
        self.id()
    }

    fn set_flags(&mut self, keys: &[Key], flag: MsgFlag, on: bool) {
        let keys: Vec<Key> = keys.iter().filter(|k| self.accts.get(k.acct).is_some_and(|a| a.live())).cloned().collect();
        self.apply_flags_local(&keys, flag, on);
        let mut groups: HashMap<(usize, String), Vec<Key>> = HashMap::new();
        for k in keys {
            groups.entry((k.acct, k.folder.clone())).or_default().push(k);
        }
        for ((a, folder), keys) in groups {
            let op = self.new_op();
            let uv = self.accts[a].uv(&folder);
            let uids = keys.iter().map(|k| k.uid).collect();
            if self.accts[a].send(Cmd::Flag { op, folder, uv, uids, flag, on }) {
                self.ops.insert(op, (a, Op::Flag(keys, flag, on)));
            } else {
                self.apply_flags_local(&keys, flag, !on);
                self.status = "That account is paused; resume it to make changes.".into();
            }
        }
    }

    /// Hide messages locally and move the selection to a neighbour.
    fn hide(&mut self, keys: &[Key]) {
        let pos = self.cur.as_ref().and_then(|c| self.rows.iter().position(|r| &r.key == c));
        for k in keys {
            self.hidden.insert(k.clone());
        }
        self.rows.retain(|r| !keys.contains(&r.key));
        if self.cur.as_ref().is_some_and(|c| keys.contains(c)) {
            self.cur = None;
            self.set_view(None);
            self.loading = None;
            if let Some(p) = pos {
                if let Some(next) = self.rows.get(p.min(self.rows.len().saturating_sub(1))) {
                    let k = next.key.clone();
                    self.open(k);
                    self.scroll_to_cur = true;
                }
            }
        }
    }

    /// Archive/delete/move: hidden now, sent to the server after the undo window.
    fn triage(&mut self, moves: Vec<(Key, Option<String>)>, label: String, undoable: bool) {
        let moves: Vec<_> =
            moves.into_iter().filter(|(k, _)| self.accts.get(k.acct).is_some_and(|a| a.live() && a.worker.is_some())).collect();
        if moves.is_empty() {
            self.status = "That account is paused; resume it to make changes.".into();
            return;
        }
        let keys: Vec<Key> = moves.iter().map(|(k, _)| k.clone()).collect();
        self.hide(&keys);
        if undoable {
            self.pending.push(Pending { label, moves, deadline: Instant::now() + UNDO_WINDOW });
        } else {
            self.dispatch(moves);
        }
    }

    fn dispatch(&mut self, moves: Vec<(Key, Option<String>)>) {
        let mut groups: HashMap<(usize, String, Option<String>), Vec<Key>> = HashMap::new();
        for (k, dest) in moves {
            groups.entry((k.acct, k.folder.clone(), dest)).or_default().push(k);
        }
        for ((a, folder, dest), keys) in groups {
            let op = self.new_op();
            let uv = self.accts[a].uv(&folder);
            let uids: Vec<u32> = keys.iter().map(|k| k.uid).collect();
            let cmd = match dest {
                Some(dest) => Cmd::Move { op, folder, uv, uids, dest },
                None => Cmd::Expunge { op, folder, uv, uids },
            };
            if self.accts[a].send(cmd) {
                self.ops.insert(op, (a, Op::Triage(keys)));
            } else {
                for k in &keys {
                    self.hidden.remove(k);
                }
                self.rows_dirty = true;
                self.status = "That account is paused; the change was not made.".into();
            }
        }
    }

    fn undo(&mut self) {
        if let Some(p) = self.pending.pop() {
            for (k, _) in &p.moves {
                self.hidden.remove(k);
            }
            self.rows_dirty = true;
            self.status = format!("Undone: {}", p.label.to_lowercase());
        }
    }

    fn count_label(verb: &str, n: usize) -> String {
        if n == 1 {
            format!("{verb} 1 message")
        } else {
            format!("{verb} {n} messages")
        }
    }

    fn trash(&mut self, keys: Vec<Key>) {
        let mut forever = Vec::new();
        let mut moves = Vec::new();
        for k in keys {
            let a = &self.accts[k.acct];
            match (a.folder_special(&k.folder), a.special(Special::Trash)) {
                (Some(Special::Trash), _) | (_, None) => forever.push(k),
                (_, Some(t)) => moves.push((k, Some(t))),
            }
        }
        if !moves.is_empty() {
            let label = Self::count_label("Deleted", moves.len());
            self.triage(moves, label, true);
        }
        if !forever.is_empty() {
            self.confirm = Some(Confirm::DeleteForever(forever));
        }
    }

    fn archive(&mut self, keys: Vec<Key>) {
        let mut moves = Vec::new();
        for k in keys {
            let a = &self.accts[k.acct];
            match a.special(Special::Archive).or_else(|| a.special(Special::All)) {
                Some(dest) if dest != k.folder => moves.push((k, Some(dest))),
                Some(_) => {}
                None => self.status = format!("{} has no Archive folder", a.cfg.label()),
            }
        }
        if !moves.is_empty() {
            let label = Self::count_label("Archived", moves.len());
            self.triage(moves, label, true);
        }
    }

    fn current_acct(&self) -> usize {
        if let Some(k) = &self.cur {
            if self.accts.get(k.acct).is_some_and(|a| a.live()) {
                return k.acct;
            }
        }
        if let Sel::Folder(i, _) = &self.sel {
            return *i;
        }
        self.acct_index(&self.cfg.settings.default_account).or_else(|| self.first_live()).unwrap_or(0)
    }

    fn new_compose(&mut self, acct: usize) -> Option<&mut Compose> {
        if self.live_count() == 0 {
            self.status = "Add an account first".into();
            return None;
        }
        let acct = if self.accts.get(acct).is_some_and(|a| a.live()) { acct } else { self.first_live()? };
        let id = self.id();
        let c = Compose::new(id, acct, &self.accts[acct].cfg.signature);
        self.composes.push(c);
        self.composes.last_mut()
    }

    fn own_addresses(&self) -> Vec<String> {
        self.accts.iter().filter(|a| a.live()).flat_map(|a| a.cfg.own_addresses()).collect()
    }

    fn reply(&mut self, all: bool) {
        let Some(key) = self.cur.clone() else { return };
        if self.view.is_none() {
            return;
        }
        let id = self.id();
        let own = self.own_addresses();
        let a = &self.accts[key.acct];
        let mut c = Compose::new(id, key.acct, &a.cfg.signature);
        compose::reply(&mut c, &a.cfg, &own, self.view.as_ref().unwrap(), all, (key.acct, key.folder.clone(), key.uid));
        self.composes.push(c);
    }

    fn forward(&mut self) {
        let Some(key) = self.cur.clone() else { return };
        let Some(v) = self.view.as_ref() else { return };
        let carried = view::all_attachments(v);
        let id = self.id();
        let mut c = Compose::new(id, key.acct, &self.accts[key.acct].cfg.signature);
        compose::forward(&mut c, self.view.as_ref().unwrap(), carried);
        self.composes.push(c);
    }

    /// Open a message for editing. For drafts, later saves replace the original.
    fn edit_as_new(&mut self, resume_draft: bool) {
        let Some(key) = self.cur.clone() else { return };
        let Some(v) = self.view.as_ref() else { return };
        let carried = view::all_attachments(v);
        let id = self.id();
        let uv = self.accts[key.acct].uv(&key.folder);
        let v = self.view.as_ref().unwrap();
        let mut c = Compose::new(id, key.acct, "");
        c.to = v.to_list.join(", ");
        c.cc = v.cc_list.join(", ");
        c.bcc = v.bcc_list.join(", ");
        c.show_cc = !c.cc.is_empty() || !c.bcc.is_empty();
        c.subject = v.subject.clone();
        c.body = v.text.clone();
        c.carried = carried;
        c.carried_dirty = true;
        if resume_draft {
            match compose::parse_draft_id(&v.message_id) {
                Some((k, version)) => {
                    c.key = k;
                    c.version = version;
                }
                None => c.replaces = Some((key.folder.clone(), uv, key.uid)),
            }
            // Pick the identity the draft was written from.
            let from = compose::bare(&v.from);
            if let Some(i) = self.accts[key.acct].cfg.identities().iter().position(|x| compose::bare(x) == from) {
                c.from = i;
            }
        }
        c.mark_clean();
        self.composes.push(c);
    }

    fn send_compose(&mut self, id: u64) {
        let Some(c) = self.composes.iter_mut().find(|c| c.id == id) else { return };
        if c.sending || c.saving.is_some() {
            return;
        }
        let Some(a) = self.accts.get(c.acct).filter(|a| a.live()) else {
            c.error = Some("Choose an account to send from".into());
            return;
        };
        let msg = match compose::build(&a.cfg, c, None) {
            Ok(m) => m,
            Err(e) => {
                c.error = Some(e);
                return;
            }
        };
        c.sending = true;
        c.error = None;
        c.note = None;
        let acct = a.cfg.clone();
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let result = crate::smtp::send(&acct, &msg).map(|_| msg.formatted());
            let _ = tx.send(Event { acct: String::new(), gen: 0, kind: Ev::Sent { compose: id, result } });
            ctx.request_repaint();
        });
    }

    fn save_draft(&mut self, id: u64, close_after: bool) {
        let op = self.new_op();
        let Some(c) = self.composes.iter_mut().find(|c| c.id == id) else { return };
        if c.sending || c.saving.is_some() {
            return;
        }
        Self::persist(c, &self.accts);
        let Some(a) = self.accts.get(c.acct).filter(|a| a.live()) else {
            c.error = Some("Choose an account first".into());
            return;
        };
        let Some(folder) = a.special(Special::Drafts) else {
            c.error = Some("This account has no Drafts folder. The draft is kept on this computer.".into());
            return;
        };
        let version = c.version + 1;
        let data = match compose::build(&a.cfg, c, Some(version)) {
            Ok(m) => m.formatted(),
            Err(e) => {
                c.error = Some(e);
                return;
            }
        };
        let cmd = Cmd::SaveDraft { op, folder, data, key: c.key.clone(), msg_id: c.draft_msg_id(version), replaces: c.replaces.clone() };
        if !a.send(cmd) {
            c.error = Some("The account is paused. The draft is kept on this computer.".into());
            return;
        }
        c.saving = Some(op);
        c.close_after_save = close_after;
        c.error = None;
        c.note = Some("Saving…".into());
        let hash = c.content_hash();
        let acct = c.acct;
        self.ops.insert(op, (acct, Op::DraftSave { compose: id, version, hash }));
    }

    fn to_saved(c: &Compose, accts: &[Acct]) -> drafts::Saved {
        let id = |i: usize| accts.get(i).map(|a| a.cfg.id.clone()).unwrap_or_default();
        drafts::Saved {
            key: c.key.clone(),
            version: c.version,
            acct_id: id(c.acct),
            from: c.from,
            to: c.to.clone(),
            cc: c.cc.clone(),
            bcc: c.bcc.clone(),
            subject: c.subject.clone(),
            body: c.body.clone(),
            files: c.files.clone(),
            carried: c.carried.iter().map(|(n, m, _)| (n.clone(), m.clone())).collect(),
            in_reply_to: c.in_reply_to.clone(),
            references: c.references.clone(),
            reply_of: c.reply_of.as_ref().map(|(a, f, u)| (id(*a), f.clone(), *u)),
            replaces: c.replaces.clone(),
        }
    }

    /// Write the local recovery copy now.
    fn persist(c: &mut Compose, accts: &[Acct]) {
        let carried = if c.carried_dirty { Some(c.carried.as_slice()) } else { None };
        if drafts::save(&Self::to_saved(c, accts), carried).is_ok() {
            c.persisted = c.content_hash();
            c.carried_dirty = false;
        }
    }

    /// Persist edited compose windows a moment after typing stops.
    fn autosave(&mut self) {
        let now = Instant::now();
        let mut soonest: Option<Duration> = None;
        for c in self.composes.iter_mut() {
            let h = c.content_hash();
            if h == c.persisted {
                continue;
            }
            let e = self.edit_track.entry(c.id).or_insert((h, now));
            if e.0 != h {
                *e = (h, now);
            }
            let waited = now.duration_since(e.1);
            if waited >= PERSIST_AFTER {
                Self::persist(c, &self.accts);
            } else {
                let left = PERSIST_AFTER - waited;
                soonest = Some(soonest.map_or(left, |s| s.min(left)));
            }
        }
        if let Some(d) = soonest {
            self.ctx.request_repaint_after(d + Duration::from_millis(20));
        }
    }

    fn recover_composes(&mut self) {
        for (s, carried) in drafts::load_all() {
            let acct = self.acct_index(&s.acct_id).or_else(|| self.first_live()).unwrap_or(0);
            let id = self.id();
            let mut c = Compose::new(id, acct, "");
            c.key = s.key;
            c.version = s.version;
            c.from = s.from;
            c.to = s.to;
            c.cc = s.cc;
            c.bcc = s.bcc;
            c.show_cc = !c.cc.is_empty() || !c.bcc.is_empty();
            c.subject = s.subject;
            c.body = s.body;
            c.files = s.files;
            c.carried = carried;
            c.in_reply_to = s.in_reply_to;
            c.references = s.references;
            c.reply_of = s.reply_of.and_then(|(aid, f, u)| self.acct_index(&aid).map(|a| (a, f, u)));
            c.replaces = s.replaces;
            c.recovered = true;
            c.focus = false;
            c.persisted = c.content_hash();
            // Not saved anywhere but this computer: closing should ask.
            c.clean = 0;
            self.composes.push(c);
        }
        if !self.composes.is_empty() {
            self.status = format!("Recovered {} unsent message(s) from last time", self.composes.len());
        }
    }

    fn request_close(&mut self, id: u64) {
        let Some(c) = self.composes.iter().find(|c| c.id == id) else { return };
        if c.sending {
            self.status = "Still sending; the window will close when it's done.".into();
            return;
        }
        if c.saving.is_some() {
            return;
        }
        if c.dirty() {
            self.confirm = Some(Confirm::CloseCompose(id));
        } else {
            self.close_compose(id);
        }
    }

    fn close_compose(&mut self, id: u64) {
        if let Some(pos) = self.composes.iter().position(|c| c.id == id) {
            let c = self.composes.remove(pos);
            drafts::delete(&c.key);
            self.edit_track.remove(&id);
        }
    }

    fn refresh(&mut self) {
        self.last_folder_poll = Instant::now();
        for a in self.accts.iter().filter(|a| a.live()) {
            a.send(Cmd::Folders);
            a.send(Cmd::Sync { folder: a.inbox(), more: false });
        }
        if let Sel::Folder(i, f) = &self.sel {
            self.accts[*i].send(Cmd::Sync { folder: f.clone(), more: false });
        }
    }

    fn retry(&mut self, i: usize) {
        let a = &mut self.accts[i];
        a.send(Cmd::Reconnect);
        if let Some(w) = &a.watcher {
            w.kick();
        }
        a.state = AcctState::Connecting;
    }

    fn set_paused(&mut self, i: usize, paused: bool) {
        if self.accts[i].cfg.paused == paused {
            return;
        }
        self.accts[i].cfg.paused = paused;
        if paused {
            let _ = self.stop_conn(i);
        } else {
            self.start_conn(i, Duration::ZERO, None);
        }
        self.save_config();
    }

    fn server_search(&mut self) {
        let q = self.filter.trim().to_string();
        if q.is_empty() {
            return;
        }
        // Keep the scope of an existing search; otherwise search the folder on screen.
        let targets: Vec<(usize, String)> = match &self.sel {
            Sel::Folder(i, f) => vec![(*i, f.clone())],
            Sel::Search(_) => self.accts.iter().enumerate().filter_map(|(i, a)| a.search.as_ref().map(|s| (i, s.folder.clone()))).collect(),
            Sel::Unified => {
                self.accts.iter().enumerate().filter(|(_, a)| a.live() && a.worker.is_some()).map(|(i, a)| (i, a.inbox())).collect()
            }
        };
        if targets.is_empty() {
            return;
        }
        for a in self.accts.iter_mut() {
            a.search = None;
        }
        self.search_req += 1;
        for (i, folder) in targets {
            let a = &mut self.accts[i];
            a.search = Some(SearchState {
                req: self.search_req,
                folder: folder.clone(),
                uv: 0,
                query: q.clone(),
                msgs: vec![],
                total: 0,
                loading: true,
            });
            a.send(Cmd::Search { req: self.search_req, folder, query: q.clone() });
        }
        self.clear_reader();
        self.sel = Sel::Search(q);
        self.rows_dirty = true;
    }

    fn clear_reader(&mut self) {
        self.cur = None;
        self.set_view(None);
        self.loading = None;
        self.view_error = None;
    }

    fn select(&mut self, sel: Sel) {
        if sel == self.sel {
            return;
        }
        if let Sel::Folder(i, f) = &sel {
            let a = &mut self.accts[*i];
            a.ensure_index(f);
            a.send(Cmd::Sync { folder: f.clone(), more: false });
            self.last_folder_poll = Instant::now();
        }
        if matches!(self.sel, Sel::Search(_)) {
            self.filter.clear();
            for a in self.accts.iter_mut() {
                a.search = None;
            }
        }
        self.clear_reader();
        self.sel = sel;
        self.rows_dirty = true;
    }

    fn move_selection(&mut self, delta: i32) {
        if self.rows.is_empty() {
            return;
        }
        let pos = self.cur.as_ref().and_then(|c| self.rows.iter().position(|r| &r.key == c));
        let next = match pos {
            Some(p) => (p as i64 + delta as i64).clamp(0, self.rows.len() as i64 - 1) as usize,
            None => 0,
        };
        let k = self.rows[next].key.clone();
        self.open(k);
        self.scroll_to_cur = true;
    }

    fn save_attachment(&mut self, i: usize) {
        let Some(v) = self.nested.last().or(self.view.as_ref()) else { return };
        let Some(a) = v.attachments.get(i) else { return };
        let Some(bytes) = view::part_bytes(&v.raw, a.part) else { return };
        if let Some(path) = rfd::FileDialog::new().set_file_name(view::sanitize_filename(&a.name)).save_file() {
            self.status = match std::fs::write(&path, bytes) {
                Ok(()) => format!("Saved {}", path.display()),
                Err(e) => format!("Could not save: {e}"),
            };
        }
    }

    fn open_attachment(&mut self, i: usize) {
        let n = self.id();
        let Some(v) = self.nested.last().or(self.view.as_ref()) else { return };
        let Some(a) = v.attachments.get(i) else { return };
        let name = view::sanitize_filename(&a.name);
        let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        if !name.contains('.') || RISKY_EXT.contains(&ext.as_str()) {
            self.status = format!("\"{name}\" could run code, so it can only be saved, not opened from here");
            return;
        }
        let Some(bytes) = view::part_bytes(&v.raw, a.part) else { return };
        let dir = std::env::temp_dir().join("mail-rs").join(format!("{n:x}"));
        let path = dir.join(&name);
        let r = std::fs::create_dir_all(&dir)
            .and_then(|_| std::fs::write(&path, bytes))
            .map_err(|e| e.to_string())
            .and_then(|_| open::that(&path).map_err(|e| e.to_string()));
        if let Err(e) = r {
            self.status = format!("Could not open attachment: {e}");
        }
    }

    fn save_eml(&mut self) {
        let Some(v) = self.nested.last().or(self.view.as_ref()) else { return };
        let name = format!("{}.eml", view::sanitize_filename(&v.subject));
        if let Some(path) = rfd::FileDialog::new().set_file_name(name).add_filter("Email", &["eml"]).save_file() {
            // The original bytes, exactly as received.
            self.status = match std::fs::write(&path, v.raw.as_slice()) {
                Ok(()) => format!("Saved {}", path.display()),
                Err(e) => format!("Could not save: {e}"),
            };
        }
    }

    fn open_html(&mut self) {
        let n = self.id();
        let Some(v) = self.nested.last().or(self.view.as_ref()) else { return };
        let Some(html) = &v.html else { return };
        let path = std::env::temp_dir().join("mail-rs").join(format!("message-{n:x}.html"));
        let mut data = b"\xEF\xBB\xBF".to_vec(); // BOM: the decoded HTML is UTF-8.
        data.extend_from_slice(html.as_bytes());
        let r = std::fs::create_dir_all(path.parent().unwrap())
            .and_then(|_| std::fs::write(&path, data))
            .map_err(|e| e.to_string())
            .and_then(|_| open::that(&path).map_err(|e| e.to_string()));
        if let Err(e) = r {
            self.status = format!("Could not open browser: {e}");
        }
    }

    fn attach_files(&mut self, id: u64) {
        if let Some(files) = rfd::FileDialog::new().pick_files() {
            if let Some(c) = self.composes.iter_mut().find(|c| c.id == id && !c.sending) {
                c.files.extend(files);
            }
        }
    }

    fn remove_account(&mut self, i: usize) {
        let join = self.stop_conn(i);
        let a = &mut self.accts[i];
        a.removed = true;
        a.idx.clear();
        a.search = None;
        let (id, store) = (a.cfg.id.clone(), a.store.clone());
        let removed_id = id.clone();
        // Purge only after the worker has really stopped writing to the cache.
        std::thread::spawn(move || {
            if let Some(j) = join {
                let _ = j.join();
            }
            secrets::delete(&id, secrets::PASSWORD);
            secrets::delete(&id, secrets::REFRESH);
            oauth::forget(&id);
            store.purge();
        });
        // Unsent messages move to another account rather than disappearing.
        let other = self.first_live();
        for c in self.composes.iter_mut().filter(|c| c.acct == i) {
            if let Some(o) = other {
                c.acct = o;
                c.from = 0;
            }
            if c.reply_of.as_ref().is_some_and(|r| r.0 == i) {
                c.reply_of = None;
            }
        }
        self.hidden.retain(|k| k.acct != i);
        self.pending.retain(|p| p.moves.iter().all(|(k, _)| k.acct != i));
        self.bodies.retain(|k, _| k.acct != i);
        self.body_order.retain(|k| k.acct != i);
        if self.cfg.settings.default_account == removed_id {
            self.cfg.settings.default_account.clear();
        }
        self.sel = Sel::Unified;
        self.clear_reader();
        self.rows_dirty = true;
        self.title_unread = usize::MAX;
        self.save_config();
    }

    /// Anything that must finish before the app can safely exit.
    fn outstanding(&self) -> usize {
        self.composes.iter().filter(|c| c.sending || c.saving.is_some()).count() + self.pending.len() + self.ops.len()
    }

    fn check_updates(&mut self, install: bool) {
        self.last_update_check = Some(Instant::now());
        self.update = UpdateState::Checking;
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        update::spawn(install, move |msg| {
            let _ = tx.send(Event { acct: String::new(), gen: 0, kind: Ev::Update(msg) });
            ctx.request_repaint();
        });
    }

    fn install_update(&mut self, r: update::Release) {
        self.update = UpdateState::Downloading(r.version.to_string());
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let msg = match update::download(&r) {
                Ok(s) => match s.mode {
                    update::Mode::Portable => match update::apply_portable(&s) {
                        Ok(()) => update::Msg::Ready(s),
                        Err(e) => update::Msg::Failed(e),
                    },
                    update::Mode::Installer => update::Msg::Ready(s),
                },
                Err(e) => update::Msg::Failed(e),
            };
            let _ = tx.send(Event { acct: String::new(), gen: 0, kind: Ev::Update(msg) });
            ctx.request_repaint();
        });
    }

    /// Called once the window is really closing: finish a staged update.
    fn on_closing(&mut self) {
        if let Some(s) = self.staged.take() {
            let r = match s.mode {
                update::Mode::Installer => update::run_installer(&s, self.restart_requested),
                update::Mode::Portable if self.restart_requested => update::relaunch(),
                update::Mode::Portable => Ok(()),
            };
            if let Err(e) = r {
                eprintln!("update: {e}");
            }
        } else if self.restart_requested {
            let _ = update::relaunch();
        }
    }

    fn ui_update_banner(&mut self, root: &mut egui::Ui) {
        let ctx = &root.ctx().clone();
        let p = theme::palette(ctx);
        let Some(s) = &self.staged else { return };
        if self.banner_dismissed {
            return;
        }
        let (version, url, mode) = (s.version.to_string(), s.url.clone(), s.mode);
        egui::Panel::top("update-banner")
            .exact_size(40.0)
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(p.accent_subtle).inner_margin(egui::Margin::symmetric(16, 4)))
            .show(root, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(kit::glyph(ic::ICON_SYSTEM_UPDATE, 18.0, p.accent));
                    let text = match mode {
                        update::Mode::Installer => format!("Mail {version} is ready. It installs when you close Mail."),
                        update::Mode::Portable => format!("Mail {version} is ready. It starts next time you open Mail."),
                    };
                    ui.label(RichText::new(text).color(p.text));
                    if ui
                        .add(egui::Button::new(RichText::new("Restart now").font(theme::semibold(ty::UI)).color(p.accent)).frame(false))
                        .clicked()
                    {
                        self.restart_requested = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    ui.hyperlink_to(RichText::new("What's new").color(p.accent), url);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if kit::icon_button(ui, ic::ICON_CLOSE, "Dismiss").clicked() {
                            self.banner_dismissed = true;
                        }
                    });
                });
            });
    }

    fn apply(&mut self, act: Action) {
        match act {
            Action::Select(s) => self.select(s),
            Action::Open(k) => self.open(k),
            Action::Flag(keys, f, on) => self.set_flags(&keys, f, on),
            Action::Trash(keys) => self.trash(keys),
            Action::Archive(keys) => self.archive(keys),
            Action::MoveTo(keys, dest) => {
                let moves: Vec<_> = keys.into_iter().filter(|k| k.folder != dest).map(|k| (k, Some(dest.clone()))).collect();
                let label = Self::count_label("Moved", moves.len());
                self.triage(moves, label, true);
            }
            Action::Undo => self.undo(),
            Action::Compose => {
                let a = self.current_acct();
                self.new_compose(a);
            }
            Action::ComposeTo(to) => {
                let a = self.current_acct();
                if let Some(c) = self.new_compose(a) {
                    let to = to.trim_start_matches("mailto:");
                    let (addr, query) = to.split_once('?').unwrap_or((to, ""));
                    c.to =
                        url::form_urlencoded::parse(format!("x={addr}").as_bytes()).next().map(|(_, v)| v.into_owned()).unwrap_or_default();
                    for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
                        match k.to_ascii_lowercase().as_str() {
                            "subject" => c.subject = v.into_owned(),
                            "cc" => {
                                c.cc = v.into_owned();
                                c.show_cc = true;
                            }
                            "body" => c.body = format!("{v}{}", c.body),
                            _ => {}
                        }
                    }
                }
            }
            Action::Reply(all) => self.reply(all),
            Action::Forward => self.forward(),
            Action::EditAsNew(draft) => self.edit_as_new(draft),
            Action::Refresh => self.refresh(),
            Action::Retry(i) => self.retry(i),
            Action::Pause(i, p) => self.set_paused(i, p),
            Action::LoadMore => {
                if let Sel::Folder(i, f) = &self.sel {
                    self.accts[*i].send(Cmd::Sync { folder: f.clone(), more: true });
                } else if self.sel == Sel::Unified {
                    for a in self.accts.iter().filter(|a| a.live()) {
                        a.send(Cmd::Sync { folder: a.inbox(), more: true });
                    }
                }
            }
            Action::ServerSearch => self.server_search(),
            Action::AddAccount => self.open_editor(None),
            Action::EditAccount(i) => self.open_editor(Some(i)),
            Action::RemoveAccount(i) => self.confirm = Some(Confirm::RemoveAccount(i)),
            Action::ViewSource => {
                if let Some(v) = self.nested.last().or(self.view.as_ref()) {
                    const MAX: usize = 4 * 1024 * 1024;
                    let raw = &v.raw[..v.raw.len().min(MAX)];
                    let mut s = String::from_utf8_lossy(raw).into_owned();
                    if v.raw.len() > MAX {
                        s.push_str("\n\n[… truncated for display; use Save .eml for the full message]");
                    }
                    self.source = Some((v.subject.clone(), s));
                }
            }
            Action::SaveEml => self.save_eml(),
            Action::OpenHtml => self.open_html(),
            Action::SaveAttachment(i) => self.save_attachment(i),
            Action::OpenAttachment(i) => self.open_attachment(i),
            Action::Send(id) => self.send_compose(id),
            Action::SaveDraft(id, close) => self.save_draft(id, close),
            Action::AttachFiles(id) => self.attach_files(id),
            Action::RequestClose(id) => self.request_close(id),
            Action::CloseNow(id) => self.close_compose(id),
            Action::Move(d) => self.move_selection(d),
            Action::Back => {
                self.nested.pop();
                self.bump_view();
            }
            Action::ToggleHtml(k) => {
                if !self.text_toggle.remove(&k) {
                    self.text_toggle.insert(k);
                }
            }
            Action::AllowRemote(k) => {
                self.remote_ok.insert(k);
            }
            Action::OpenNested(i) => self.open_nested(i),
            Action::AddToCalendar => self.invite_file(true),
            Action::SaveIcs => self.invite_file(false),
            Action::Unsubscribe => self.unsubscribe(),
            Action::CloseAccountDetail => self.request_leave(Leave::Back),
        }
    }

    // ---------------------------------------------------- account editor ---

    fn open_editor(&mut self, editing: Option<usize>) {
        if editing.is_some() {
            self.settings_open = true;
            self.settings_page = SettingsPage::Accounts;
        }
        self.open_editor_inner(editing);
        self.editor_baseline = self.editor.as_ref().map(Self::fingerprint);
        if editing.is_none() {
            if let Some(ed) = self.editor.take() {
                let id = self.id();
                self.setup = Some(Setup {
                    id,
                    step: SetupStep::Email,
                    ed: Editor { new_id: Some(crate::config::new_account_id()), ..ed },
                    detected: String::new(),
                    show_password: false,
                    incoming: Check::Waiting,
                    outgoing: Check::Waiting,
                    added: None,
                    error: None,
                    admin: None,
                });
            }
        }
    }

    /// Everything the user can change in the account editor.
    fn fingerprint(ed: &Editor) -> String {
        format!(
            "{:?}",
            (
                &ed.name,
                &ed.email,
                &ed.username,
                &ed.password,
                ed.auth,
                (&ed.imap.host, ed.imap.port, ed.imap.security),
                (&ed.smtp.host, ed.smtp.port, ed.smtp.security),
                (&ed.client_id, &ed.client_secret),
                (ed.save_sent, &ed.signature, &ed.aliases, ed.notify),
                // A fresh browser sign-in is only kept once saved.
                matches!(ed.oauth, OAuthState::Done(_)),
            )
        )
    }

    fn editor_dirty(&self) -> bool {
        match (&self.editor, &self.editor_baseline) {
            (Some(ed), Some(base)) => ed.editing.is_some() && Self::fingerprint(ed) != *base,
            _ => false,
        }
    }

    /// Leave the account editor (or Settings), asking first if there are unsaved edits.
    fn request_leave(&mut self, to: Leave) {
        if self.editor_dirty() {
            self.leave_pending = Some(to);
        } else {
            self.apply_leave(to);
        }
    }

    fn apply_leave(&mut self, to: Leave) {
        self.leave_pending = None;
        match to {
            Leave::Page(page) => {
                self.settings_page = page;
                if page != SettingsPage::Accounts {
                    self.editor = None;
                }
            }
            Leave::Back => self.editor = None,
            Leave::Close => {
                self.settings_open = false;
                self.editor = None;
            }
        }
    }

    fn open_editor_inner(&mut self, editing: Option<usize>) {
        let id = self.id();
        let s = self.cfg.settings.clone();
        let ed = match editing.and_then(|i| self.accts.get(i)).filter(|a| a.live()) {
            Some(a) => {
                let c = &a.cfg;
                Editor {
                    id,
                    editing,
                    provider: Provider::guess(&c.email).unwrap_or(Provider::Custom),
                    name: c.name.clone(),
                    email: c.email.clone(),
                    username: c.username.clone(),
                    username_touched: c.username != c.email,
                    password: String::new(),
                    auth: c.auth,
                    imap: c.imap.clone(),
                    smtp: c.smtp.clone(),
                    client_id: c.oauth_client_id.clone(),
                    client_secret: c.oauth_client_secret.clone(),
                    save_sent: c.save_sent,
                    signature: c.signature.clone(),
                    aliases: c.aliases.join("\n"),
                    notify: c.notify,
                    oauth: OAuthState::Idle,
                    error: None,
                    new_id: None,
                }
            }
            None => {
                let mut ed = Editor {
                    id,
                    editing: None,
                    provider: Provider::Gmail,
                    name: String::new(),
                    email: String::new(),
                    username: String::new(),
                    username_touched: false,
                    password: String::new(),
                    auth: AuthKind::Password,
                    imap: Provider::Gmail.preset().0,
                    smtp: Provider::Gmail.preset().1,
                    client_id: String::new(),
                    client_secret: String::new(),
                    save_sent: false,
                    signature: String::new(),
                    aliases: String::new(),
                    notify: true,
                    oauth: OAuthState::Idle,
                    error: None,
                    new_id: None,
                };
                Self::apply_preset(&mut ed, Provider::Gmail, &s);
                ed
            }
        };
        self.editor = Some(ed);
    }

    fn apply_preset(ed: &mut Editor, p: Provider, settings: &crate::config::Settings) {
        ed.provider = p;
        let (imap, smtp, mut auth, save_sent) = p.preset();
        // Without a Google OAuth client, an app password is the path that works.
        if p == Provider::Gmail && settings.google_client_id.trim().is_empty() {
            auth = AuthKind::Password;
        }
        ed.imap = imap;
        ed.smtp = smtp;
        ed.auth = auth;
        ed.save_sent = save_sent;
        ed.oauth = OAuthState::Idle;
        Self::default_client(ed, settings);
    }

    fn default_client(ed: &mut Editor, s: &crate::config::Settings) {
        match ed.auth {
            AuthKind::OAuthGoogle => {
                ed.client_id = s.google_client_id.clone();
                ed.client_secret = s.google_client_secret.clone();
            }
            AuthKind::OAuthMicrosoft => {
                ed.client_id = s.microsoft_client_id.clone();
                ed.client_secret.clear();
            }
            AuthKind::Password => {}
        }
    }

    fn save_editor(&mut self) -> Result<(), String> {
        let ed = self.editor.as_mut().unwrap();
        let email = ed.email.trim().to_string();
        if !email.contains('@') {
            return Err("Enter a valid email address".into());
        }
        if ed.imap.host.trim().is_empty() || ed.smtp.host.trim().is_empty() {
            return Err("IMAP and SMTP servers are required".into());
        }
        let username = if ed.username.trim().is_empty() { email.clone() } else { ed.username.trim().to_string() };
        // The same mailbox twice would double connections and confuse every view.
        let dup = self.accts.iter().enumerate().any(|(i, a)| {
            a.live()
                && Some(i) != ed.editing
                && a.cfg.imap.host.trim().eq_ignore_ascii_case(ed.imap.host.trim())
                && a.cfg.username.trim().eq_ignore_ascii_case(&username)
        });
        if dup {
            return Err("This mailbox is already added (same server and user name). Edit the existing account instead.".into());
        }
        let aliases: Vec<String> = ed.aliases.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
        for a in &aliases {
            if a.parse::<lettre::message::Mailbox>().is_err() {
                return Err(format!("Alias \"{a}\" is not a valid address"));
            }
        }
        let existing = ed.editing.and_then(|i| self.accts.get(i)).map(|a| a.cfg.clone());
        let id = existing.as_ref().map(|a| a.id.clone()).or_else(|| ed.new_id.clone()).unwrap_or_else(crate::config::new_account_id);
        match ed.auth {
            AuthKind::Password => {
                if !ed.password.is_empty() {
                    secrets::set(&id, secrets::PASSWORD, &ed.password)?;
                } else if existing.as_ref().is_none_or(|a| a.auth != AuthKind::Password) {
                    return Err("Enter your password (or app password)".into());
                }
            }
            _ => match &ed.oauth {
                OAuthState::Done(refresh) => secrets::set(&id, secrets::REFRESH, refresh)?,
                _ if existing.as_ref().is_some_and(|a| a.auth == ed.auth && a.oauth_client_id == ed.client_id.trim()) => {}
                _ => return Err("Press \"Sign in with browser\" first".into()),
            },
        }
        let acct = Account {
            id: id.clone(),
            name: ed.name.trim().to_string(),
            email,
            username,
            imap: Server { host: ed.imap.host.trim().into(), ..ed.imap.clone() },
            smtp: Server { host: ed.smtp.host.trim().into(), ..ed.smtp.clone() },
            auth: ed.auth,
            oauth_client_id: ed.client_id.trim().into(),
            oauth_client_secret: ed.client_secret.trim().into(),
            save_sent: ed.save_sent,
            signature: ed.signature.clone(),
            paused: existing.as_ref().is_some_and(|a| a.paused),
            notify: ed.notify,
            aliases,
        };
        oauth::forget(&id);
        match ed.editing {
            Some(i) if i < self.accts.len() => {
                // Restart the connection; the new worker waits for the old one to finish.
                let join = self.stop_conn(i);
                self.accts[i].cfg = acct;
                let n = self.cfg.settings.notifications;
                let a = &self.accts[i];
                a.shared.notify.store(n && a.cfg.notify, Ordering::Relaxed);
                if !self.accts[i].cfg.paused {
                    self.start_conn(i, Duration::ZERO, join);
                }
            }
            _ => self.add_account(acct, Duration::ZERO),
        }
        self.save_config();
        Ok(())
    }

    // -------------------------------------------------------------- UI ---

    fn ui_top(&mut self, root: &mut egui::Ui) {
        let ctx = &root.ctx().clone();
        let p = theme::palette(ctx);
        let narrow = ctx.content_rect().width() < 1100.0;
        egui::Panel::top("top")
            .exact_size(56.0)
            .frame(egui::Frame::new().fill(p.window).inner_margin(egui::Margin { left: 8, right: 12, top: 12, bottom: 12 }))
            .show(root, |ui| {
                ui.horizontal_centered(|ui| {
                    if kit::icon_button(ui, ic::ICON_MENU, "Show or hide folders").clicked() {
                        if narrow {
                            self.nav_drawer = !self.nav_drawer;
                        } else {
                            self.nav_open = !self.nav_open;
                        }
                    }
                    ui.add_space(4.0);
                    ui.label(RichText::new("Mail").font(theme::semibold(ty::HEADING)).color(p.text));
                    ui.add_space(24.0);

                    // Search: typing filters downloaded mail; Enter searches the server.
                    let search_w = (ui.available_width() - 150.0).clamp(240.0, 720.0);
                    let in_folder = matches!(self.sel, Sel::Folder(..));
                    egui::Frame::new()
                        .fill(p.input)
                        .stroke(Stroke::new(1.0, p.divider))
                        .corner_radius(egui::CornerRadius::same(8))
                        .inner_margin(egui::Margin { left: 10, right: 4, top: 2, bottom: 2 })
                        .show(ui, |ui| {
                            ui.set_width(search_w);
                            ui.horizontal(|ui| {
                                ui.label(kit::glyph(ic::ICON_SEARCH, 20.0, p.text_secondary));
                                let scope_w = if in_folder { 128.0 } else { 0.0 };
                                let clear_w = if self.filter.is_empty() { 0.0 } else { 32.0 };
                                let edit = egui::TextEdit::singleline(&mut self.filter)
                                    .hint_text("Search mail")
                                    .frame(egui::Frame::NONE)
                                    .margin(egui::Margin::symmetric(4, 7))
                                    .align(Align2::LEFT_CENTER)
                                    .desired_width((ui.available_width() - scope_w - clear_w - 8.0).max(80.0));
                                let r = ui.add(edit);
                                if self.focus_filter {
                                    r.request_focus();
                                    self.focus_filter = false;
                                }
                                if r.changed() {
                                    self.rows_dirty = true;
                                }
                                if r.lost_focus() && ui.input(|i| i.key_pressed(K::Enter)) {
                                    self.acts.push(Action::ServerSearch);
                                }
                                if r.has_focus() && ui.input(|i| i.key_pressed(K::Escape)) {
                                    if matches!(self.sel, Sel::Search(_)) {
                                        self.acts.push(Action::Select(Sel::Unified));
                                    }
                                    self.filter.clear();
                                    self.rows_dirty = true;
                                }
                                if !self.filter.is_empty() && kit::icon_button(ui, ic::ICON_CLOSE, "Clear search (Esc)").clicked() {
                                    if matches!(self.sel, Sel::Search(_)) {
                                        self.acts.push(Action::Select(Sel::Unified));
                                    }
                                    self.filter.clear();
                                    self.rows_dirty = true;
                                }
                                if in_folder {
                                    egui::ComboBox::from_id_salt("search-scope")
                                        .icon(kit::combo_chevron)
                                        .selected_text(
                                            RichText::new(if self.search_all { "All inboxes" } else { "This folder" }).size(ty::META + 1.0),
                                        )
                                        .width(112.0)
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(&mut self.search_all, false, "This folder");
                                            ui.selectable_value(&mut self.search_all, true, "All inboxes");
                                        })
                                        .response
                                        .on_hover_text("Where Enter searches on the server");
                                }
                            });
                        });

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if kit::icon_button(ui, ic::ICON_SETTINGS, "Settings").clicked() {
                            self.settings_open = true;
                        }
                        if kit::icon_button(ui, ic::ICON_HELP, "Keyboard shortcuts (?)").clicked() {
                            self.shortcuts_open = true;
                        }
                        if kit::icon_button(ui, ic::ICON_REFRESH, "Check for new mail (F5)").clicked() {
                            self.acts.push(Action::Refresh);
                        }
                        if self.accts.iter().any(|a| a.busy) {
                            ui.add(egui::Spinner::new().size(16.0).color(p.text_secondary));
                        }
                    });
                });
            });
    }

    /// Transient notices (what the old status bar showed) appear as a toast.
    fn track_status(&mut self) {
        if self.status != self.last_status {
            self.last_status = self.status.clone();
            if !self.status.is_empty() {
                self.toast_text = self.status.clone();
                self.toast_at = Some(Instant::now());
            }
        }
    }

    fn ui_toast(&mut self, ctx: &egui::Context) {
        let p = theme::palette(ctx);
        let now = Instant::now();
        let undo = self.pending.last().map(|x| x.label.clone());
        let info = match self.toast_at {
            Some(t) if now.duration_since(t) < TOAST_FOR && !self.toast_text.is_empty() => Some(self.toast_text.clone()),
            _ => None,
        };
        let Some(text) = undo.clone().or(info) else { return };
        let r = egui::Area::new(Id::unique("toast")).order(Order::Foreground).anchor(Align2::LEFT_BOTTOM, egui::vec2(16.0, -16.0)).show(
            ctx,
            |ui| {
                egui::Frame::new()
                    .fill(p.inverse)
                    .corner_radius(egui::CornerRadius::same(8))
                    .shadow(ctx.global_style().visuals.popup_shadow)
                    .inner_margin(egui::Margin { left: 16, right: 8, top: 6, bottom: 6 })
                    .show(ui, |ui| {
                        ui.set_max_width(480.0);
                        ui.horizontal(|ui| {
                            ui.add(egui::Label::new(RichText::new(&text).color(p.on_inverse)).wrap());
                            let accent = if p.dark { theme::LIGHT.accent } else { theme::DARK.accent };
                            if undo.is_some() {
                                if ui
                                    .add(egui::Button::new(RichText::new("Undo").font(theme::semibold(ty::UI)).color(accent)).frame(false))
                                    .on_hover_text("Ctrl+Z")
                                    .clicked()
                                {
                                    self.acts.push(Action::Undo);
                                }
                            } else if ui
                                .add(egui::Button::new(kit::glyph(ic::ICON_CLOSE, 18.0, p.on_inverse)).frame(false))
                                .on_hover_text("Dismiss")
                                .clicked()
                            {
                                self.toast_at = None;
                            }
                        });
                    });
            },
        );
        // Hovering keeps the toast (and the undo window) open.
        if r.response.contains_pointer() {
            for x in self.pending.iter_mut() {
                x.deadline = x.deadline.max(now + Duration::from_secs(3));
            }
            if undo.is_none() {
                self.toast_at = Some(now);
            }
        }
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    /// Account health under its heading: quiet when fine, actionable when not.
    fn ui_account_status(ui: &mut egui::Ui, i: usize, a: &Acct, acts: &mut Vec<Action>) {
        let p = kit::pal(ui);
        let line = |ui: &mut egui::Ui, icon, color, text: &str, tip: &str| {
            ui.label(kit::glyph(icon, 16.0, color));
            ui.label(RichText::new(text).size(ty::META).color(color)).on_hover_text(tip);
        };
        let link = |ui: &mut egui::Ui, text: &str| {
            ui.add(egui::Button::new(RichText::new(text).size(ty::META).color(p.accent)).frame(false)).clicked()
        };
        if a.cfg.paused {
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                line(ui, ic::ICON_PAUSE, p.text_secondary, "Paused", "This account holds no connections while paused");
                if link(ui, "Resume") {
                    acts.push(Action::Pause(i, false));
                }
            });
            return;
        }
        match &a.state {
            AcctState::Offline(e) => {
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    match crate::admin::classify(e, "", "") {
                        Some(issue) => line(ui, ic::ICON_ADMIN_PANEL_SETTINGS, p.warning, issue.short, e),
                        None => line(ui, ic::ICON_CLOUD_OFF, p.warning, "Offline", e),
                    }
                    if link(ui, "Retry") {
                        acts.push(Action::Retry(i));
                    }
                });
            }
            AcctState::AuthFailed(e) => {
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    match crate::admin::classify(e, "", "") {
                        Some(issue) => {
                            line(ui, ic::ICON_ADMIN_PANEL_SETTINGS, p.warning, issue.short, e);
                            if link(ui, "Details") {
                                acts.push(Action::EditAccount(i));
                            }
                        }
                        None => {
                            line(ui, ic::ICON_ERROR, p.error, "Sign-in failed", e);
                            if link(ui, "Sign in again") {
                                acts.push(Action::EditAccount(i));
                            }
                        }
                    }
                });
            }
            AcctState::Online | AcctState::Connecting => {}
        }
    }

    fn folder_icon(special: Option<Special>) -> egui_material_icons::MaterialIcon {
        match special {
            Some(Special::Inbox) => ic::ICON_INBOX,
            Some(Special::Drafts) => ic::ICON_DRAFT,
            Some(Special::Sent) => ic::ICON_SEND,
            Some(Special::Archive) => ic::ICON_ARCHIVE,
            Some(Special::All) => ic::ICON_ALL_INBOX,
            Some(Special::Junk) => ic::ICON_REPORT,
            Some(Special::Trash) => ic::ICON_DELETE,
            None => ic::ICON_FOLDER,
        }
    }

    fn ui_folders(&mut self, root: &mut egui::Ui) {
        let ctx = root.ctx().clone();
        let p = theme::palette(&ctx);
        let narrow = ctx.content_rect().width() < 1100.0;
        if (narrow && !self.nav_drawer) || (!narrow && !self.nav_open) {
            return;
        }
        egui::Panel::left("folders")
            .resizable(true)
            .default_size(224.0)
            .size_range(180.0..=360.0)
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(p.nav).inner_margin(egui::Margin { left: 8, right: 8, top: 4, bottom: 8 }))
            .show(root, |ui| {
                let mut acts = Vec::new();
                egui::Panel::bottom("nav-footer")
                    .show_separator_line(false)
                    .frame(egui::Frame::new().fill(p.nav).inner_margin(egui::Margin::symmetric(0, 6)))
                    .show(ui, |ui| {
                        if kit::quiet_button(ui, Some(ic::ICON_PERSON_ADD), "Add account").clicked() {
                            acts.push(Action::AddAccount);
                        }
                    });
                ui.add_space(4.0);
                if kit::primary_button(ui, Some(ic::ICON_EDIT), "New message").on_hover_text("New message (C)").clicked() {
                    acts.push(Action::Compose);
                }
                ui.add_space(12.0);
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let total: usize = self.accts.iter().filter(|a| a.live()).map(|a| a.unread(&a.inbox())).sum();
                    if kit::nav_row(ui, ic::ICON_ALL_INBOX, "All inboxes", total, self.sel == Sel::Unified, 0.0, true).clicked() {
                        acts.push(Action::Select(Sel::Unified));
                    }
                    if let Sel::Search(q) = &self.sel {
                        let _ = kit::nav_row(ui, ic::ICON_SEARCH, &format!("“{q}”"), 0, true, 0.0, true);
                    }
                    for (i, a) in self.accts.iter().enumerate().filter(|(_, a)| a.live()) {
                        ui.add_space(14.0);
                        let id = Id::unique(("acct-open", &a.cfg.id));
                        let open = ctx.data(|d| d.get_temp::<bool>(id)).unwrap_or(true);
                        let paused = a.cfg.paused;
                        // Account heading: name, collapse, and an explicit overflow menu.
                        ui.horizontal(|ui| {
                            ui.set_height(28.0);
                            let head = ui.add(
                                egui::Button::new(
                                    RichText::new(a.cfg.label()).font(theme::semibold(ty::META + 1.0)).color(p.text_secondary),
                                )
                                .frame(false),
                            );
                            if head.clicked() {
                                ctx.data_mut(|d| d.insert_temp(id, !open));
                            }
                            ui.label(kit::glyph(if open { ic::ICON_EXPAND_LESS } else { ic::ICON_EXPAND_MORE }, 16.0, p.text_disabled));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.menu_button(kit::glyph(ic::ICON_MORE_VERT, 18.0, p.text_secondary), |ui| {
                                    if ui.button("Account settings…").clicked() {
                                        acts.push(Action::EditAccount(i));
                                        ui.close();
                                    }
                                    if ui.button(if paused { "Resume syncing" } else { "Pause syncing" }).clicked() {
                                        acts.push(Action::Pause(i, !paused));
                                        ui.close();
                                    }
                                    if ui.button("Remove account…").clicked() {
                                        acts.push(Action::RemoveAccount(i));
                                        ui.close();
                                    }
                                })
                                .response
                                .on_hover_text("Account options");
                            });
                        });
                        Self::ui_account_status(ui, i, a, &mut acts);
                        if !open {
                            continue;
                        }
                        if a.folders.is_empty() {
                            ui.label(RichText::new("Loading folders…").size(ty::META).color(p.text_secondary));
                        }
                        for f in &a.folders {
                            let unread = a.unread(&f.name);
                            let show_unread =
                                !matches!(f.special, Some(Special::Trash | Special::Junk | Special::Sent | Special::All | Special::Drafts));
                            let selected = self.sel == Sel::Folder(i, f.name.clone());
                            let r = kit::nav_row(
                                ui,
                                Self::folder_icon(f.special),
                                &f.display,
                                if show_unread { unread } else { 0 },
                                selected,
                                f.depth as f32 * 16.0,
                                f.selectable,
                            );
                            if f.selectable && r.clicked() {
                                acts.push(Action::Select(Sel::Folder(i, f.name.clone())));
                            }
                        }
                    }
                });
                self.acts.extend(acts);
            });
    }

    fn ui_list(&mut self, root: &mut egui::Ui) {
        let ctx = root.ctx().clone();
        let p = theme::palette(&ctx);
        let compact = self.cfg.settings.compact;
        let row_h = if compact { ROW_H_COMPACT } else { ROW_H };
        egui::Panel::left("list")
            .resizable(true)
            .default_size(360.0)
            .size_range(320.0..=700.0)
            .show_separator_line(true)
            .frame(egui::Frame::new().fill(p.surface))
            .show(root, |ui| {
                // Header: folder title and count, or the bulk-action bar when several are selected.
                let header = Rect::from_min_size(ui.cursor().min, egui::vec2(ui.available_width(), 52.0));
                ui.scope_builder(egui::UiBuilder::new().max_rect(header.shrink2(egui::vec2(16.0, 0.0))), |ui| {
                    ui.horizontal_centered(|ui| {
                        if self.selection.len() > 1 {
                            let keys: Vec<Key> = self.selection.iter().cloned().collect();
                            if kit::icon_button(ui, ic::ICON_CLOSE, "Clear selection (Esc)").clicked() {
                                self.selection.clear();
                            }
                            ui.label(RichText::new(format!("{} selected", keys.len())).font(theme::semibold(ty::UI)).color(p.text));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if kit::icon_button(ui, ic::ICON_MARK_EMAIL_UNREAD, "Mark as unread").clicked() {
                                    self.acts.push(Action::Flag(keys.clone(), MsgFlag::Seen, false));
                                }
                                if kit::icon_button(ui, ic::ICON_DRAFTS, "Mark as read").clicked() {
                                    self.acts.push(Action::Flag(keys.clone(), MsgFlag::Seen, true));
                                }
                                if kit::icon_button(ui, ic::ICON_DELETE, "Delete").clicked() {
                                    self.acts.push(Action::Trash(keys.clone()));
                                    self.selection.clear();
                                }
                                if kit::icon_button(ui, ic::ICON_ARCHIVE, "Archive").clicked() {
                                    self.acts.push(Action::Archive(keys.clone()));
                                    self.selection.clear();
                                }
                            });
                            return;
                        }
                        let title = match &self.sel {
                            Sel::Unified => "All inboxes".to_string(),
                            Sel::Folder(i, f) => self.accts[*i].folder_display(f),
                            Sel::Search(q) => format!("Results for “{q}”"),
                        };
                        ui.label(RichText::new(title).font(theme::semibold(ty::HEADING)).color(p.text));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if let Sel::Search(_) = &self.sel {
                                if kit::icon_button(ui, ic::ICON_CLOSE, "Close search (Esc)").clicked() {
                                    self.acts.push(Action::Select(Sel::Unified));
                                }
                                if self.accts.iter().any(|a| a.search.as_ref().is_some_and(|s| s.loading)) {
                                    ui.add(egui::Spinner::new().size(16.0).color(p.text_secondary));
                                }
                            } else if !self.rows.is_empty() {
                                ui.label(RichText::new(format!("{}", self.rows.len())).size(ty::META).color(p.text_secondary));
                            }
                        });
                    });
                });
                ui.advance_cursor_after_rect(header);
                if let Sel::Search(_) = &self.sel {
                    let scopes: Vec<String> = self
                        .accts
                        .iter()
                        .filter_map(|a| a.search.as_ref().map(|s| (a, s)))
                        .map(|(a, s)| {
                            let trunc =
                                if s.total > s.msgs.len() { format!(" · newest {} of {}", s.msgs.len(), s.total) } else { String::new() };
                            format!("{} › {}{trunc}", a.cfg.label(), a.folder_display(&s.folder))
                        })
                        .collect();
                    ui.horizontal(|ui| {
                        ui.add_space(16.0);
                        ui.add(
                            egui::Label::new(
                                RichText::new(format!("Searched {}", scopes.join("; "))).size(ty::META).color(p.text_secondary),
                            )
                            .wrap(),
                        );
                    });
                    ui.add_space(6.0);
                } else if !self.filter.trim().is_empty() {
                    ui.horizontal(|ui| {
                        ui.add_space(16.0);
                        ui.label(
                            RichText::new("Filtering downloaded messages · press Enter to search the server")
                                .size(ty::META)
                                .color(p.text_secondary),
                        );
                    });
                    ui.add_space(6.0);
                }
                let r = ui.available_rect_before_wrap();
                ui.painter().hline(r.x_range(), r.top(), Stroke::new(1.0, p.divider));

                if self.rows.is_empty() {
                    let busy = self.accts.iter().any(|a| a.busy || a.search.as_ref().is_some_and(|s| s.loading));
                    let (icon, title, detail, action) = if self.live_count() == 0 {
                        (ic::ICON_PERSON_ADD, "No accounts yet", "Add an email account to get started.", Some("Add account"))
                    } else if busy {
                        (ic::ICON_SYNC, "Loading messages…", "", None)
                    } else if matches!(self.sel, Sel::Search(_)) {
                        (ic::ICON_SEARCH, "No results", "Try different words, or search all inboxes.", None)
                    } else if !self.filter.trim().is_empty() {
                        (ic::ICON_SEARCH, "No downloaded messages match", "Press Enter to search the server.", None)
                    } else {
                        (ic::ICON_INBOX, "Nothing here", "This folder is empty.", None)
                    };
                    if kit::empty_state(ui, icon, title, detail, action) {
                        self.acts.push(Action::AddAccount);
                    }
                    return;
                }

                ui.spacing_mut().item_spacing.y = 0.0;
                let mut sa = egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("list");
                if self.scroll_to_cur {
                    self.scroll_to_cur = false;
                    if let Some(pos) = self.cur.as_ref().and_then(|c| self.rows.iter().position(|r| &r.key == c)) {
                        let y = pos as f32 * row_h;
                        let (off, h) = self.list_scroll;
                        if y < off {
                            sa = sa.vertical_scroll_offset(y);
                        } else if y + row_h > off + h {
                            sa = sa.vertical_scroll_offset(y + row_h - h);
                        }
                    }
                }
                let multi = self.live_count() > 1 && self.sel != Sel::Folder(0, String::new());
                let mods = ctx.input(|i| i.modifiers);
                let mut acts = Vec::new();
                let mut clicked_row: Option<(usize, bool, bool)> = None;
                let more = match &self.sel {
                    Sel::Unified => self
                        .accts
                        .iter()
                        .filter(|a| a.live())
                        .any(|a| a.idx.get(&a.inbox()).is_some_and(|i| (i.msgs.len() as u32) < i.exists)),
                    Sel::Folder(i, f) => self.accts[*i].idx.get(f).is_some_and(|x| (x.msgs.len() as u32) < x.exists),
                    Sel::Search(_) => false,
                };
                let n = self.rows.len() + usize::from(more);
                let out = sa.show_rows(ui, row_h, n, |ui, range| {
                    for i in range {
                        if i == self.rows.len() {
                            // Last row: load older messages.
                            ui.allocate_ui(egui::vec2(ui.available_width(), row_h), |ui| {
                                ui.centered_and_justified(|ui| {
                                    if kit::quiet_button(ui, Some(ic::ICON_EXPAND_MORE), "Load older messages").clicked() {
                                        acts.push(Action::LoadMore);
                                    }
                                });
                            });
                            continue;
                        }
                        let r = &self.rows[i];
                        let a = &self.accts[r.key.acct];
                        let st = RowState {
                            selected: self.cur.as_ref() == Some(&r.key),
                            checked: self.selection.contains(&r.key),
                            compact,
                            acct_label: if multi && !matches!(self.sel, Sel::Folder(..)) {
                                a.cfg.label().to_string()
                            } else {
                                String::new()
                            },
                        };
                        let (resp, act) = paint_row(ui, r, &st);
                        let keys = vec![r.key.clone()];
                        match act {
                            Some(RowAct::Archive) => acts.push(Action::Archive(keys.clone())),
                            Some(RowAct::Delete) => acts.push(Action::Trash(keys.clone())),
                            Some(RowAct::ToggleRead) => acts.push(Action::Flag(keys.clone(), MsgFlag::Seen, !r.s.seen)),
                            Some(RowAct::ToggleStar) => acts.push(Action::Flag(keys.clone(), MsgFlag::Flagged, !r.s.flagged)),
                            None => {
                                if resp.clicked() {
                                    clicked_row = Some((i, mods.command, mods.shift));
                                }
                            }
                        }
                        let key = r.key.clone();
                        let (seen, flagged) = (r.s.seen, r.s.flagged);
                        resp.context_menu(|ui| {
                            let keys = vec![key.clone()];
                            if ui.button(if seen { "Mark as unread" } else { "Mark as read" }).clicked() {
                                acts.push(Action::Flag(keys.clone(), MsgFlag::Seen, !seen));
                                ui.close();
                            }
                            if ui.button(if flagged { "Unstar" } else { "Star" }).clicked() {
                                acts.push(Action::Flag(keys.clone(), MsgFlag::Flagged, !flagged));
                                ui.close();
                            }
                            if ui.button("Archive").clicked() {
                                acts.push(Action::Archive(keys.clone()));
                                ui.close();
                            }
                            if ui.button("Delete").clicked() {
                                acts.push(Action::Trash(keys.clone()));
                                ui.close();
                            }
                            ui.menu_button("Move to", |ui| {
                                egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                                    for f in a.folders.iter().filter(|f| f.selectable && f.name != key.folder) {
                                        if ui.button(&f.display).clicked() {
                                            acts.push(Action::MoveTo(keys.clone(), f.name.clone()));
                                            ui.close();
                                        }
                                    }
                                });
                            });
                        });
                    }
                });
                // Click: open. Ctrl+click: toggle in selection. Shift+click: select a range.
                if let Some((i, ctrl, shift)) = clicked_row {
                    let key = self.rows[i].key.clone();
                    if shift {
                        let a = self.sel_anchor.unwrap_or(i);
                        let (lo, hi) = (a.min(i), a.max(i));
                        self.selection = self.rows[lo..=hi].iter().map(|r| r.key.clone()).collect();
                    } else if ctrl {
                        if self.selection.is_empty() {
                            if let Some(c) = self.cur.clone() {
                                self.selection.insert(c);
                            }
                        }
                        if !self.selection.remove(&key) {
                            self.selection.insert(key);
                        }
                        self.sel_anchor = Some(i);
                    } else {
                        self.selection.clear();
                        self.sel_anchor = Some(i);
                        acts.push(Action::Open(key));
                    }
                }
                self.acts.extend(acts);
                self.list_scroll = (out.state.clamped_offset().y, out.inner_rect.height());
            });
    }

    fn html_supported(&self) -> bool {
        cfg!(any(windows, target_os = "macos")) && self.html_error.is_none()
    }

    /// The message on screen: an opened attached email, or the selected one.
    fn top_view(&self) -> Option<&View> {
        self.nested.last().or(self.view.as_ref())
    }

    fn ui_reader(&mut self, root: &mut egui::Ui) {
        let ctx = &root.ctx().clone();
        let p = theme::palette(ctx);
        let html_ok = self.html_supported();
        let mut html_target: Option<(Rect, u64)> = None;
        let mut want_previews: Vec<mail_parser::PartId> = Vec::new();
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(p.surface).inner_margin(egui::Margin { left: 24, right: 24, top: 8, bottom: 0 }))
            .show(root, |ui| {
                if self.live_count() == 0 {
                    if kit::empty_state(ui, ic::ICON_MAIL, "Welcome to Mail", "Add an email account to get started.", Some("Add account")) {
                        self.acts.push(Action::AddAccount);
                    }
                    return;
                }
                let Some(key) = self.cur.clone() else {
                    let detail = if self.selection.len() > 1 {
                        "Use the actions above the list for the selected messages."
                    } else {
                        "Choose a message to read it here."
                    };
                    let title = if self.selection.len() > 1 {
                        format!("{} messages selected", self.selection.len())
                    } else {
                        "No message selected".into()
                    };
                    kit::empty_state(ui, ic::ICON_MAIL, &title, detail, None);
                    return;
                };
                if self.loading.is_some() {
                    ui.centered_and_justified(|ui| ui.add(egui::Spinner::new().size(24.0).color(p.text_secondary)));
                    return;
                }
                if let Some(e) = self.view_error.clone() {
                    if kit::empty_state(ui, ic::ICON_ERROR, "Couldn't load this message", &e, Some("Try again")) {
                        self.view_error = None;
                        self.acts.push(Action::Open(key));
                    }
                    return;
                }
                let nested = self.nested.len();
                let Some(v) = self.nested.last().or(self.view.as_ref()) else { return };
                let s = self.rows.iter().find(|r| r.key == key).map(|r| r.s.clone()).unwrap_or_default();
                let acct = &self.accts[key.acct];
                let in_drafts = acct.folder_special(&key.folder) == Some(Special::Drafts);
                let keys = vec![key.clone()];
                let mut want_html = !self.cfg.settings.prefer_plain_text;
                if self.text_toggle.contains(&key) {
                    want_html = !want_html;
                }
                let html_mode = want_html && html_ok && v.html.is_some();
                let allow_remote = self.cfg.settings.load_remote_content || self.remote_ok.contains(&key);

                // ---- Toolbar
                ui.horizontal(|ui| {
                    ui.set_height(40.0);
                    if nested > 0 {
                        if kit::quiet_button(ui, Some(ic::ICON_ARROW_BACK), "Back")
                            .on_hover_text("Return to the message this was attached to")
                            .clicked()
                        {
                            self.acts.push(Action::Back);
                        }
                        ui.label(RichText::new("Attached message").size(ty::META).color(p.text_secondary));
                    } else {
                        if in_drafts && kit::quiet_button(ui, Some(ic::ICON_EDIT), "Edit draft").clicked() {
                            self.acts.push(Action::EditAsNew(true));
                        }
                        if kit::quiet_button(ui, Some(ic::ICON_REPLY), "Reply").on_hover_text("Reply (R)").clicked() {
                            self.acts.push(Action::Reply(false));
                        }
                        if kit::quiet_button(ui, Some(ic::ICON_REPLY_ALL), "Reply all").on_hover_text("Reply all (A)").clicked() {
                            self.acts.push(Action::Reply(true));
                        }
                        if kit::quiet_button(ui, Some(ic::ICON_FORWARD), "Forward").on_hover_text("Forward (F)").clicked() {
                            self.acts.push(Action::Forward);
                        }
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.menu_button(kit::glyph(ic::ICON_MORE_VERT, 20.0, p.text_secondary), |ui| {
                            if v.html.is_some() {
                                let label = if html_mode { "Show plain text" } else { "Show formatted (HTML)" };
                                if ui.add_enabled(html_ok || html_mode, egui::Button::new(label)).clicked() {
                                    self.acts.push(Action::ToggleHtml(key.clone()));
                                    ui.close();
                                }
                            }
                            if ui.button("View source").clicked() {
                                self.acts.push(Action::ViewSource);
                                ui.close();
                            }
                            if ui.button("Save as .eml").on_hover_text("The original message, byte for byte").clicked() {
                                self.acts.push(Action::SaveEml);
                                ui.close();
                            }
                            if v.html.is_some() && ui.button("Open in browser").clicked() {
                                self.acts.push(Action::OpenHtml);
                                ui.close();
                            }
                            if nested == 0 && ui.button("Edit as new message").clicked() {
                                self.acts.push(Action::EditAsNew(false));
                                ui.close();
                            }
                            ui.separator();
                            ui.checkbox(&mut self.mono, "Monospace plain text");
                        })
                        .response
                        .on_hover_text("More");
                        if nested == 0 {
                            if kit::icon_button_ex(
                                ui,
                                ic::ICON_STAR,
                                if s.flagged { "Unstar (S)" } else { "Star (S)" },
                                s.flagged,
                                s.flagged.then_some(p.warning),
                            )
                            .clicked()
                            {
                                self.acts.push(Action::Flag(keys.clone(), MsgFlag::Flagged, !s.flagged));
                            }
                            let (icon, tip) = if s.seen {
                                (ic::ICON_MARK_EMAIL_UNREAD, "Mark as unread (U)")
                            } else {
                                (ic::ICON_DRAFTS, "Mark as read (U)")
                            };
                            if kit::icon_button(ui, icon, tip).clicked() {
                                self.acts.push(Action::Flag(keys.clone(), MsgFlag::Seen, !s.seen));
                            }
                            ui.menu_button(kit::glyph(ic::ICON_DRIVE_FILE_MOVE, 20.0, p.text_secondary), |ui| {
                                egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                                    for f in acct.folders.iter().filter(|f| f.selectable && f.name != key.folder) {
                                        if ui.button(&f.display).clicked() {
                                            self.acts.push(Action::MoveTo(keys.clone(), f.name.clone()));
                                            ui.close();
                                        }
                                    }
                                });
                            })
                            .response
                            .on_hover_text("Move to");
                            if kit::icon_button(ui, ic::ICON_DELETE, "Delete (Del)").clicked() {
                                self.acts.push(Action::Trash(keys.clone()));
                            }
                            if kit::icon_button(ui, ic::ICON_ARCHIVE, "Archive (E)").clicked() {
                                self.acts.push(Action::Archive(keys.clone()));
                            }
                        }
                    });
                });
                ui.add_space(8.0);

                // ---- Header (bounded, so long recipient lists never push the body away)
                let max_head = (ui.available_height() * 0.7).max(200.0);
                egui::ScrollArea::vertical()
                    .id_salt(("head", key.acct, &key.folder, key.uid, nested))
                    .max_height(max_head)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(&v.subject).font(theme::semibold(ty::SUBJECT)).color(p.text)).wrap());
                        ui.add_space(12.0);
                        let (name, addr) = split_mailbox(&v.from);
                        let details_id = Id::unique(("details", key.acct, &key.folder, key.uid));
                        let details = ctx.data(|d| d.get_temp::<bool>(details_id)).unwrap_or(false);
                        ui.horizontal(|ui| {
                            kit::avatar(ui, if name.is_empty() { &addr } else { &name }, 36.0);
                            ui.add_space(4.0);
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(if name.is_empty() { &addr } else { &name })
                                            .font(theme::semibold(ty::UI))
                                            .color(p.text),
                                    );
                                    if !name.is_empty() {
                                        ui.label(RichText::new(format!("<{addr}>")).size(ty::META + 1.0).color(p.text_secondary));
                                    }
                                    if !v.unsubscribe.is_empty()
                                        && ui
                                            .add(
                                                egui::Button::new(RichText::new("Unsubscribe").size(ty::META + 1.0).color(p.accent))
                                                    .frame(false),
                                            )
                                            .on_hover_text(v.unsubscribe.join("\n"))
                                            .clicked()
                                    {
                                        self.acts.push(Action::Unsubscribe);
                                    }
                                });
                                let own = acct.cfg.own_addresses();
                                let names: Vec<String> = v
                                    .to_list
                                    .iter()
                                    .chain(&v.cc_list)
                                    .map(|a| {
                                        if own.contains(&compose::bare(a)) {
                                            "me".to_string()
                                        } else {
                                            split_mailbox(a)
                                                .0
                                                .split(' ')
                                                .next()
                                                .filter(|n| !n.is_empty())
                                                .map(String::from)
                                                .unwrap_or_else(|| split_mailbox(a).1)
                                        }
                                    })
                                    .collect();
                                let summary = match names.len() {
                                    0 => String::new(),
                                    1..=3 => format!("to {}", names.join(", ")),
                                    n => format!("to {}, {} +{}", names[0], names[1], n - 2),
                                };
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(summary).size(ty::META + 1.0).color(p.text_secondary));
                                    if ui
                                        .add(
                                            egui::Button::new(kit::glyph(
                                                if details { ic::ICON_EXPAND_LESS } else { ic::ICON_EXPAND_MORE },
                                                18.0,
                                                p.text_secondary,
                                            ))
                                            .frame(false),
                                        )
                                        .on_hover_text("Show details")
                                        .clicked()
                                    {
                                        ctx.data_mut(|d| d.insert_temp(details_id, !details));
                                    }
                                });
                            });
                            ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                                ui.label(RichText::new(&v.date).size(ty::META + 1.0).color(p.text_secondary));
                            });
                        });
                        if details {
                            ui.add_space(6.0);
                            egui::Frame::new()
                                .fill(p.window)
                                .corner_radius(egui::CornerRadius::same(6))
                                .inner_margin(egui::Margin::same(12))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    egui::Grid::new("hdrs").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
                                        let row = |ui: &mut egui::Ui, k: &str, val: &str| {
                                            if !val.is_empty() {
                                                ui.label(RichText::new(k).size(ty::META + 1.0).color(p.text_secondary));
                                                ui.add(egui::Label::new(RichText::new(val).size(ty::META + 1.0).color(p.text)).wrap());
                                                ui.end_row();
                                            }
                                        };
                                        row(ui, "From", &v.from);
                                        row(ui, "To", &v.to);
                                        row(ui, "Cc", &v.cc);
                                        if !v.bcc_list.is_empty() {
                                            row(ui, "Bcc", &v.bcc_list.join(", "));
                                        }
                                        row(ui, "Date", &v.date);
                                    });
                                });
                        }
                        ui.add_space(12.0);
                        for n in &v.notices {
                            let tone = if matches!(n, crate::formats::Notice::Encrypted(_)) { Tone::Warning } else { Tone::Info };
                            kit::notice(ui, tone, &n.text(), None);
                            ui.add_space(6.0);
                        }
                        if let Some((inv, _)) = &v.invite {
                            egui::Frame::new()
                                .stroke(Stroke::new(1.0, p.divider))
                                .corner_radius(egui::CornerRadius::same(8))
                                .inner_margin(egui::Margin::same(16))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.horizontal(|ui| {
                                        ui.label(kit::glyph(ic::ICON_CALENDAR_MONTH, 20.0, p.accent));
                                        ui.label(RichText::new(inv.headline()).size(ty::META + 1.0).color(p.text_secondary));
                                    });
                                    ui.label(RichText::new(&inv.summary).font(theme::semibold(16.0)).color(p.text));
                                    ui.add_space(4.0);
                                    for (icon, val) in [
                                        (ic::ICON_SCHEDULE, &inv.when),
                                        (ic::ICON_LOCATION_ON, &inv.location),
                                        (ic::ICON_PERSON, &inv.organizer),
                                    ] {
                                        if !val.is_empty() {
                                            ui.horizontal(|ui| {
                                                ui.label(kit::glyph(icon, 16.0, p.text_secondary));
                                                ui.add(egui::Label::new(RichText::new(val.as_str()).color(p.text)).wrap());
                                            });
                                        }
                                    }
                                    ui.add_space(8.0);
                                    ui.horizontal(|ui| {
                                        if kit::secondary_button(ui, "Add to calendar")
                                            .on_hover_text("Opens the invitation in your calendar app")
                                            .clicked()
                                        {
                                            self.acts.push(Action::AddToCalendar);
                                        }
                                        if kit::quiet_button(ui, Some(ic::ICON_DOWNLOAD), "Save .ics").clicked() {
                                            self.acts.push(Action::SaveIcs);
                                        }
                                    });
                                });
                            ui.add_space(8.0);
                        }
                        if html_mode && v.remote_content && !allow_remote {
                            if kit::notice(ui, Tone::Info, "Remote images are blocked to protect your privacy.", Some("Load images")) {
                                self.acts.push(Action::AllowRemote(key.clone()));
                            }
                            ui.add_space(8.0);
                        } else if !html_mode && v.html.is_some() {
                            let (text, action) = match &self.html_error {
                                Some(e) if want_html => (format!("Shown as text: {e}."), Some("Open in browser")),
                                _ if !html_ok => ("This message is shown as text.".to_string(), Some("Open in browser")),
                                _ if v.from_html => {
                                    ("This message only has an HTML version; it's shown as text.".to_string(), Some("Show formatted"))
                                }
                                _ => ("Shown as plain text.".to_string(), Some("Show formatted")),
                            };
                            if kit::notice(ui, Tone::Info, &text, action) {
                                self.acts.push(if html_ok { Action::ToggleHtml(key.clone()) } else { Action::OpenHtml });
                            }
                            ui.add_space(8.0);
                        }
                        if !v.attachments.is_empty() {
                            ui.horizontal_wrapped(|ui| {
                                ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                                for (i, a) in v.attachments.iter().enumerate() {
                                    let icon = if a.is_message {
                                        ic::ICON_MAIL
                                    } else if a.is_image {
                                        ic::ICON_IMAGE
                                    } else if a.mime == "text/calendar" {
                                        ic::ICON_CALENDAR_MONTH
                                    } else {
                                        ic::ICON_DESCRIPTION
                                    };
                                    egui::Frame::new()
                                        .stroke(Stroke::new(1.0, p.divider))
                                        .corner_radius(egui::CornerRadius::same(8))
                                        .inner_margin(egui::Margin { left: 10, right: 4, top: 6, bottom: 6 })
                                        .show(ui, |ui| {
                                            ui.set_width(232.0);
                                            ui.horizontal(|ui| {
                                                ui.label(kit::glyph(icon, 22.0, p.text_secondary));
                                                ui.vertical(|ui| {
                                                    ui.spacing_mut().item_spacing.y = 0.0;
                                                    ui.set_width(130.0);
                                                    let g =
                                                        kit::one_line(ui, a.name.clone(), theme::semibold(ty::META + 1.0), p.text, 130.0);
                                                    ui.label(g).on_hover_text(&a.name);
                                                    ui.label(
                                                        RichText::new(format!(
                                                            "{} · {}",
                                                            file_kind(&a.name, &a.mime),
                                                            model::fmt_size(a.size)
                                                        ))
                                                        .size(ty::META)
                                                        .color(p.text_secondary),
                                                    );
                                                });
                                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                                    ui.spacing_mut().item_spacing.x = 0.0;
                                                    if kit::icon_button(ui, ic::ICON_DOWNLOAD, "Save").clicked() {
                                                        self.acts.push(Action::SaveAttachment(i));
                                                    }
                                                    let (oi, ot) = if a.is_message {
                                                        (ic::ICON_MAIL, "Read in Mail")
                                                    } else {
                                                        (ic::ICON_OPEN_IN_NEW, "Open")
                                                    };
                                                    if kit::icon_button(ui, oi, ot).clicked() {
                                                        self.acts.push(if a.is_message {
                                                            Action::OpenNested(i)
                                                        } else {
                                                            Action::OpenAttachment(i)
                                                        });
                                                    }
                                                });
                                            });
                                        });
                                }
                            });
                            ui.add_space(8.0);
                        }
                    });
                let r = ui.available_rect_before_wrap();
                ui.painter().hline(r.x_range(), r.top(), Stroke::new(1.0, p.divider));
                ui.add_space(1.0);

                if html_mode {
                    // The native web view is placed over this area after the frame is laid out.
                    let rect = ui.available_rect_before_wrap();
                    ui.allocate_rect(rect, Sense::hover());
                    use std::hash::{Hash, Hasher};
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    (&key, nested, allow_remote, self.view_seq).hash(&mut h);
                    html_target = Some((rect, h.finish()));
                    return;
                }

                let mono = self.mono;
                egui::ScrollArea::vertical().id_salt(("body", key.acct, &key.folder, key.uid, nested)).auto_shrink([false, false]).show(
                    ui,
                    |ui| {
                        ui.add_space(16.0);
                        ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
                        let font = if mono { FontId::monospace(ty::MONO) } else { theme::regular(ty::BODY) };
                        for b in &v.blocks {
                            match b {
                                Block::Text(t) => {
                                    ui.add(egui::Label::new(RichText::new(t).font(font.clone()).color(p.text)).wrap().selectable(true));
                                }
                                Block::Line(segs) => {
                                    ui.horizontal_wrapped(|ui| {
                                        for (link, s) in segs {
                                            let rt = RichText::new(s).font(font.clone());
                                            if !*link {
                                                ui.add(egui::Label::new(rt.color(p.text)).wrap().selectable(true));
                                            } else if s.starts_with("mailto:") {
                                                if ui.link(rt).clicked() {
                                                    self.acts.push(Action::ComposeTo(s.clone()));
                                                }
                                            } else {
                                                ui.hyperlink_to(rt, s).on_hover_text(s);
                                            }
                                        }
                                    });
                                }
                            }
                        }
                        // Image attachments are previewed below the text, at their own size at most.
                        ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                        for (i, a) in v.attachments.iter().enumerate().filter(|(_, a)| a.is_image) {
                            ui.add_space(16.0);
                            match self.previews.get(&(self.view_seq, a.part)) {
                                Some(Preview::Ready(tex)) => {
                                    let natural = tex.size_vec2();
                                    let max_w = ui.available_width().min(640.0).min(natural.x);
                                    let size = natural * (max_w / natural.x).min(1.0);
                                    let r = ui
                                        .add(egui::Image::new(tex).fit_to_exact_size(size).corner_radius(6.0).sense(Sense::click()))
                                        .on_hover_text(format!("{} · click to open", a.name));
                                    if r.clicked() {
                                        self.acts.push(Action::OpenAttachment(i));
                                    }
                                }
                                Some(Preview::Loading) => {
                                    ui.add(egui::Spinner::new().color(p.text_secondary));
                                }
                                Some(Preview::Failed) => {}
                                None => want_previews.push(a.part),
                            }
                        }
                        ui.add_space(32.0);
                    },
                );
            });
        self.html_target = html_target;
        for p in want_previews {
            self.request_preview(p);
        }
    }

    fn request_preview(&mut self, part: mail_parser::PartId) {
        let Some(v) = self.top_view() else { return };
        let (raw, seq) = (v.raw.clone(), self.view_seq);
        self.previews.insert((seq, part), Preview::Loading);
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let image = view::part_bytes(&raw, part).and_then(|b| image::load_from_memory(&b).ok()).map(|img| {
                // Only ever shrink: `thumbnail` would scale small images up to the bound.
                let img = if img.width() > 1400 || img.height() > 1400 { img.thumbnail(1400, 1400) } else { img };
                let img = img.to_rgba8();
                egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw())
            });
            let _ = tx.send(Event { acct: String::new(), gen: 0, kind: Ev::Preview { seq, part, image } });
            ctx.request_repaint();
        });
    }

    /// Replace the message on screen (resets per-message state).
    fn set_view(&mut self, v: Option<View>) {
        if let Some(v) = &v {
            let sample = format!("{} {} {}", v.subject, v.from, v.text.chars().take(20_000).collect::<String>());
            self.ensure_fonts(&sample);
        }
        self.view = v;
        self.nested.clear();
        self.bump_view();
    }

    fn bump_view(&mut self) {
        self.view_seq += 1;
        self.previews.clear();
    }

    /// Load system fonts for scripts egui's defaults don't cover, the first
    /// time text in that script is shown (CJK, Hangul, Thai, Indic…).
    fn ensure_fonts(&mut self, text: &str) {
        let needs = |lo: u32, hi: u32| text.chars().any(|c| (lo..=hi).contains(&(c as u32)));
        let scripts: [(&str, bool, &[&str]); 4] = [
            (
                "cjk",
                needs(0x3040, 0x30FF) || needs(0x3400, 0x9FFF) || needs(0xF900, 0xFAFF) || needs(0xFF00, 0xFFEF),
                &[
                    "C:\\Windows\\Fonts\\msyh.ttc",
                    "C:\\Windows\\Fonts\\YuGothM.ttc",
                    "C:\\Windows\\Fonts\\msgothic.ttc",
                    "/System/Library/Fonts/Hiragino Sans GB.ttc",
                    "/System/Library/Fonts/PingFang.ttc",
                    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
                    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
                    "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
                ],
            ),
            (
                "hangul",
                needs(0xAC00, 0xD7AF) || needs(0x1100, 0x11FF) || needs(0x3130, 0x318F),
                &[
                    "C:\\Windows\\Fonts\\malgun.ttf",
                    "/System/Library/Fonts/AppleSDGothicNeo.ttc",
                    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
                    "/usr/share/fonts/truetype/nanum/NanumGothic.ttf",
                ],
            ),
            (
                "thai",
                needs(0x0E00, 0x0E7F),
                &[
                    "C:\\Windows\\Fonts\\LeelawUI.ttf",
                    "/System/Library/Fonts/Supplemental/Thonburi.ttc",
                    "/usr/share/fonts/truetype/noto/NotoSansThai-Regular.ttf",
                ],
            ),
            (
                "indic",
                needs(0x0900, 0x0DFF),
                &[
                    "C:\\Windows\\Fonts\\Nirmala.ttc",
                    "C:\\Windows\\Fonts\\Nirmala.ttf",
                    "/System/Library/Fonts/Kohinoor.ttc",
                    "/usr/share/fonts/truetype/noto/NotoSansDevanagari-Regular.ttf",
                ],
            ),
        ];
        for (name, needed, paths) in scripts {
            if !needed || self.fonts_loaded.contains(name) {
                continue;
            }
            self.fonts_loaded.insert(name);
            if let Some(bytes) = paths.iter().find_map(|p| std::fs::read(p).ok()) {
                use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};
                let fams = [FontFamily::Proportional, FontFamily::Monospace]
                    .into_iter()
                    .map(|family| InsertFontFamily { family, priority: FontPriority::Lowest })
                    .collect();
                self.ctx.add_font(FontInsert::new(name, egui::FontData::from_owned(bytes), fams));
            }
        }
    }

    /// Place, fill and show/hide the native HTML view to match the reader.
    #[cfg(any(windows, target_os = "macos"))]
    fn sync_webview(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        let Some((rect, key)) = self.html_target.take() else {
            if let Some(h) = &mut self.html_view {
                h.set_visible(false);
            }
            return;
        };
        if self.html_view.is_none() && self.html_error.is_none() {
            let (tx, c) = (self.tx.clone(), ctx.clone());
            let on_link = move |url: String| {
                let _ = tx.send(Event { acct: String::new(), gen: 0, kind: Ev::Link(url) });
                c.request_repaint();
            };
            match crate::html::HtmlView::new(frame, crate::config::cache_dir().join("webview"), on_link) {
                Ok(v) => self.html_view = Some(v),
                Err(e) => {
                    self.html_error = Some(e);
                    return;
                }
            }
        }
        let modal_open = self.any_modal_open();
        let Some(hv) = self.html_view.as_mut() else { return };
        let Some(v) = self.nested.last().or(self.view.as_ref()) else { return };
        let allow_remote = self.cfg.settings.load_remote_content || self.cur.as_ref().is_some_and(|k| self.remote_ok.contains(k));
        hv.show(key, || crate::html::Page {
            html: crate::html::prepare(v.html.as_deref().unwrap_or("")),
            raw: v.raw.clone(),
            cids: v.cids.clone(),
            allow_remote,
        });
        // Native child windows draw above egui. Never resize the view for
        // overlapping windows, menus or tooltips (that reflows the email);
        // cut them out of the view instead.
        let ppp = ctx.pixels_per_point();
        let holes: Vec<(i32, i32, i32, i32)> = ctx.memory(|m| {
            m.areas()
                .visible_layer_ids()
                .into_iter()
                .filter(|l| matches!(l.order, Order::Middle | Order::Foreground | Order::Tooltip))
                .filter_map(|l| m.area_rect(l.id))
                .filter_map(|r| {
                    let r = r.expand(1.0).intersect(rect);
                    r.is_positive().then(|| {
                        let px = |v: f32, o: f32| ((v - o) * ppp).round() as i32;
                        (px(r.min.x, rect.min.x), px(r.min.y, rect.min.y), px(r.max.x, rect.min.x), px(r.max.y, rect.min.y))
                    })
                })
                .collect()
        });
        hv.place(
            (rect.min.x * ppp).round() as i32,
            (rect.min.y * ppp).round() as i32,
            (rect.width() * ppp).round() as u32,
            (rect.height() * ppp).round() as u32,
        );
        // Without window regions (macOS), hide rather than resize while something overlaps.
        // Modal dialogs dim everything behind them, which a native view can't show: hide it.
        let clip_supported = cfg!(windows);
        hv.set_visible(!modal_open && rect.width() > 8.0 && rect.height() > 8.0 && (clip_supported || holes.is_empty()));
        hv.set_holes(holes);
    }

    /// Every dialog that dims the window (the native email view is hidden behind these).
    fn any_modal_open(&self) -> bool {
        self.settings_open
            || self.setup.is_some()
            || self.confirm.is_some()
            || self.quitting
            || self.source.is_some()
            || self.shortcuts_open
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    fn sync_webview(&mut self, _ctx: &egui::Context, _frame: &eframe::Frame) {
        self.html_target = None;
    }

    fn open_nested(&mut self, i: usize) {
        let Some(v) = self.top_view() else { return };
        let Some(a) = v.attachments.get(i) else { return };
        let Some(bytes) = view::part_bytes(&v.raw, a.part) else { return };
        let nv = view::parse(Arc::new(bytes));
        let sample = format!("{} {}", nv.subject, nv.text.chars().take(20_000).collect::<String>());
        self.ensure_fonts(&sample);
        self.nested.push(nv);
        self.bump_view();
    }

    fn invite_file(&mut self, open_it: bool) {
        let n = self.id();
        let Some((_, data)) = self.top_view().and_then(|v| v.invite.clone()) else { return };
        if open_it {
            let path = std::env::temp_dir().join("mail-rs").join(format!("invite-{n:x}.ics"));
            let r = std::fs::create_dir_all(path.parent().unwrap())
                .and_then(|_| std::fs::write(&path, &data))
                .map_err(|e| e.to_string())
                .and_then(|_| open::that(&path).map_err(|e| e.to_string()));
            if let Err(e) = r {
                self.status = format!("Couldn't open the invitation: {e}");
            }
        } else if let Some(path) = rfd::FileDialog::new().set_file_name("invite.ics").add_filter("Calendar", &["ics"]).save_file() {
            self.status = match std::fs::write(&path, &data) {
                Ok(()) => format!("Saved {}", path.display()),
                Err(e) => format!("Could not save: {e}"),
            };
        }
    }

    fn unsubscribe(&mut self) {
        let Some(targets) = self.top_view().map(|v| v.unsubscribe.clone()) else { return };
        if let Some(url) = targets.iter().find(|u| u.starts_with("https://") || u.starts_with("http://")) {
            if let Err(e) = open::that(url) {
                self.status = format!("Couldn't open the unsubscribe page: {e}");
            }
        } else if let Some(m) = targets.iter().find(|u| u.starts_with("mailto:")) {
            self.acts.push(Action::ComposeTo(m.clone()));
        }
    }

    /// Which compose window holds keyboard focus, if any.
    fn focused_compose(&self, ctx: &egui::Context) -> Option<u64> {
        let layer = ctx.memory(|m| m.focused()).and_then(|id| ctx.read_response(id)).map(|r| r.layer_id)?;
        self.composes.iter().find(|c| compose_layer(c.id) == layer).map(|c| c.id)
    }

    fn ui_composes(&mut self, ctx: &egui::Context) {
        let p = theme::palette(ctx);
        // Ctrl+Enter is taken before any text field sees it, so nothing is
        // inserted into (or trimmed from) the message.
        if let Some(id) = self.focused_compose(ctx) {
            if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, K::Enter)) {
                self.acts.push(Action::Send(id));
            }
        }
        let accts: Vec<(usize, String, Vec<String>)> = self
            .accts
            .iter()
            .enumerate()
            .filter(|(_, a)| a.live())
            .map(|(i, a)| (i, a.cfg.label().to_string(), a.cfg.identities()))
            .collect();
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        let dragging_files = ctx.input(|i| !i.raw.hovered_files.is_empty());
        let pointer = ctx.input(|i| i.pointer.hover_pos().or(i.pointer.latest_pos()));
        let screen = ctx.content_rect();
        // Docked composers sit bottom-right, side by side while they fit.
        let layout = compose_layout(ctx, screen, &self.composes);
        let mut acts = Vec::new();
        let mut self_status_copied = false;
        let mut drop_target: Option<u64> = None;
        // (composer, expanded) to bring forward after this frame.
        let mut raise: Option<(u64, bool)> = None;
        // Drafts that don't fit on screen: one "More drafts" menu.
        if !layout.overflow.is_empty() {
            let (pos, w) = layout.more_at;
            egui::Area::new(Id::unique("compose-more")).fixed_pos(pos).order(egui::Order::Foreground).show(ctx, |ui| {
                egui::Frame::new()
                    .fill(p.raised)
                    .stroke(Stroke::new(1.0, p.divider))
                    .corner_radius(egui::CornerRadius { nw: 8, ne: 8, sw: 0, se: 0 })
                    .shadow(ctx.global_style().visuals.window_shadow)
                    .show(ui, |ui| {
                        ui.set_width(w);
                        ui.set_height(44.0);
                        ui.centered_and_justified(|ui| {
                            ui.menu_button(
                                RichText::new(format!("More drafts ({})", layout.overflow.len()))
                                    .font(theme::semibold(ty::UI))
                                    .color(p.text),
                                |ui| {
                                    for c in self.composes.iter().filter(|c| layout.overflow.contains(&c.id)) {
                                        if ui.button(c.title()).clicked() {
                                            raise = Some((c.id, false));
                                        }
                                    }
                                },
                            );
                        });
                    });
            });
        }
        for c in self.composes.iter_mut().rev() {
            let id = c.id;
            let st_id = Id::unique(("compose-state", id));
            // (minimised, expanded)
            let (mut minimised, mut expanded) = ctx.data(|d| d.get_temp::<(bool, bool)>(st_id)).unwrap_or((false, false));
            let Some(&(pos, size, tab)) = layout.slots.get(&id) else { continue };
            let locked = c.sending || c.saving.is_some();
            let is_target = dragging_files && pointer.is_some_and(|ptr| Rect::from_min_size(pos, size).contains(ptr)) && !locked;
            let frame = egui::Frame::new()
                .fill(p.raised)
                .stroke(Stroke::new(if is_target { 2.0 } else { 1.0 }, if is_target { p.accent } else { p.divider }))
                .corner_radius(egui::CornerRadius::same(8))
                .shadow(ctx.global_style().visuals.window_shadow);
            let resp = egui::Window::new(c.title())
                .id(Id::unique(("compose", id)))
                .title_bar(false)
                .frame(frame)
                .current_pos(pos)
                .fixed_size(size)
                .resizable(false)
                .movable(false)
                .show(ctx, |ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(8.0, 0.0);
                    // Header: title + minimise / expand / close.
                    let (head_rect, head) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 44.0), Sense::click());
                    let title_w = (head_rect.width() - 3.0 * 32.0 - 40.0).max(40.0);
                    let g = kit::one_line(ui, c.title(), theme::semibold(ty::UI), p.text, title_w);
                    ui.painter().galley(egui::pos2(head_rect.left() + 16.0, head_rect.center().y - g.size().y / 2.0), g, p.text);
                    let btns = Rect::from_min_max(
                        egui::pos2(head_rect.right() - 3.0 * 32.0 - 8.0, head_rect.top() + 6.0),
                        egui::pos2(head_rect.right() - 8.0, head_rect.bottom() - 6.0),
                    );
                    let toggled = ui
                        .scope_builder(egui::UiBuilder::new().max_rect(btns).layout(Layout::left_to_right(Align::Center)), |ui| {
                            ui.spacing_mut().item_spacing.x = 0.0;
                            let mut toggled = None;
                            if kit::icon_button(ui, ic::ICON_MINIMIZE, if tab { "Restore" } else { "Minimise" }).clicked() {
                                toggled = Some((!tab, false));
                            }
                            if kit::icon_button(
                                ui,
                                if expanded { ic::ICON_CLOSE_FULLSCREEN } else { ic::ICON_OPEN_IN_FULL },
                                if expanded { "Dock" } else { "Expand" },
                            )
                            .clicked()
                            {
                                toggled = Some((false, !expanded));
                            }
                            if kit::icon_button(ui, ic::ICON_CLOSE, "Close").clicked() {
                                acts.push(Action::RequestClose(id));
                            }
                            toggled
                        })
                        .inner
                        .or_else(|| (head.clicked() && tab).then_some((false, false)));
                    if let Some((m, e)) = toggled {
                        if tab && !m {
                            raise = Some((id, e));
                        }
                        minimised = m;
                        expanded = e;
                    }
                    if tab {
                        return;
                    }
                    let r = ui.available_rect_before_wrap();
                    ui.painter().hline(r.x_range(), r.top(), Stroke::new(1.0, p.divider));

                    let row = |ui: &mut egui::Ui, label: &str, add: &mut dyn FnMut(&mut egui::Ui)| {
                        ui.horizontal(|ui| {
                            ui.set_height(40.0);
                            ui.add_space(16.0);
                            ui.allocate_ui_with_layout(egui::vec2(56.0, 40.0), Layout::left_to_right(Align::Center), |ui| {
                                ui.label(RichText::new(label).color(p.text_secondary));
                            });
                            add(ui);
                        });
                        let r = ui.available_rect_before_wrap();
                        ui.painter().hline((r.left() + 16.0)..=(r.right() - 16.0), r.top(), Stroke::new(1.0, p.divider));
                    };
                    // The error block's height from last frame (it sits just above the footer).
                    let err_key = Id::unique(("compose-err-h", id));
                    let err_h = if c.error.is_some() { ctx.data(|d| d.get_temp::<f32>(err_key)).unwrap_or(48.0) } else { 0.0 };
                    let scroll_h = (ui.available_height() - 60.0 - err_h).max(40.0);
                    egui::ScrollArea::vertical().id_salt(("cscroll", id)).max_height(scroll_h).auto_shrink([false, false]).show(ui, |ui| {
                        let top = ui.cursor().top();
                        ui.add_enabled_ui(!locked, |ui| {
                            row(ui, "From", &mut |ui| {
                                let current = accts
                                    .iter()
                                    .find(|(i, _, _)| *i == c.acct)
                                    .and_then(|(_, _, ids)| ids.get(c.from).or(ids.first()).cloned())
                                    .unwrap_or_else(|| "Choose an account".into());
                                egui::ComboBox::from_id_salt(("from", id))
                                    .icon(kit::combo_chevron)
                                    .selected_text(current)
                                    .width(ui.available_width() - 16.0)
                                    .show_ui(ui, |ui| {
                                        for (i, _, ids) in &accts {
                                            for (k, ident) in ids.iter().enumerate() {
                                                if ui.selectable_label(c.acct == *i && c.from == k, ident).clicked() {
                                                    c.acct = *i;
                                                    c.from = k;
                                                }
                                            }
                                        }
                                    });
                            });
                            let focus_to = c.focus && c.to.is_empty();
                            let show_cc = c.show_cc;
                            row(ui, "To", &mut |ui| {
                                let w = ui.available_width() - if show_cc { 16.0 } else { 84.0 };
                                let r = ui.add(flat_edit(&mut c.to, "Recipients", w));
                                if focus_to {
                                    r.request_focus();
                                }
                                if !show_cc && ui.add(egui::Button::new(RichText::new("Cc/Bcc").color(p.accent)).frame(false)).clicked() {
                                    c.show_cc = true;
                                }
                            });
                            if c.show_cc {
                                row(ui, "Cc", &mut |ui| {
                                    let w = ui.available_width() - 16.0;
                                    ui.add(flat_edit(&mut c.cc, "", w));
                                });
                                row(ui, "Bcc", &mut |ui| {
                                    let w = ui.available_width() - 16.0;
                                    ui.add(flat_edit(&mut c.bcc, "", w));
                                });
                            }
                            row(ui, "Subject", &mut |ui| {
                                let w = ui.available_width() - 16.0;
                                ui.add(flat_edit(&mut c.subject, "", w));
                            });
                            let focus_body = c.focus && !c.to.is_empty();
                            c.focus = false;

                            let has_files = !c.files.is_empty() || !c.carried.is_empty();
                            if has_files {
                                ui.horizontal_wrapped(|ui| {
                                    ui.add_space(12.0);
                                    ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
                                    let mut remove_file = None;
                                    let mut remove_carried = None;
                                    let chip = |ui: &mut egui::Ui, name: &str, size: usize, tip: &str| -> bool {
                                        let mut remove = false;
                                        egui::Frame::new()
                                            .fill(p.window)
                                            .stroke(Stroke::new(1.0, p.divider))
                                            .corner_radius(egui::CornerRadius::same(6))
                                            .inner_margin(egui::Margin { left: 8, right: 0, top: 0, bottom: 0 })
                                            .show(ui, |ui| {
                                                ui.horizontal(|ui| {
                                                    ui.spacing_mut().item_spacing.x = 4.0;
                                                    ui.label(kit::glyph(ic::ICON_ATTACH_FILE, 16.0, p.text_secondary));
                                                    let g = kit::one_line(
                                                        ui,
                                                        format!("{name} · {}", model::fmt_size(size)),
                                                        theme::regular(ty::META + 1.0),
                                                        p.text,
                                                        180.0,
                                                    );
                                                    ui.label(g).on_hover_text(tip);
                                                    remove = kit::icon_button(ui, ic::ICON_CLOSE, "Remove attachment").clicked();
                                                });
                                            });
                                        remove
                                    };
                                    for (i, f) in c.files.iter().enumerate() {
                                        let name = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                                        let size = std::fs::metadata(f).map(|m| m.len() as usize).unwrap_or(0);
                                        if chip(ui, &name, size, &f.display().to_string()) {
                                            remove_file = Some(i);
                                        }
                                    }
                                    for (i, (name, _, data)) in c.carried.iter().enumerate() {
                                        if chip(ui, name, data.len(), name) {
                                            remove_carried = Some(i);
                                        }
                                    }
                                    if let Some(i) = remove_file {
                                        c.files.remove(i);
                                    }
                                    if let Some(i) = remove_carried {
                                        c.carried.remove(i);
                                        c.carried_dirty = true;
                                    }
                                });
                            }
                            // The body fills the rest of the view and grows with its text.
                            let used = ui.cursor().top() - top;
                            let body_h = (scroll_h - used - 8.0).max(96.0);
                            egui::Frame::new().inner_margin(egui::Margin { left: 16, right: 8, top: 8, bottom: 0 }).show(ui, |ui| {
                                let r = ui.add(
                                    egui::TextEdit::multiline(&mut c.body)
                                        .frame(egui::Frame::NONE)
                                        .desired_width(f32::INFINITY)
                                        .min_size(egui::vec2(0.0, body_h))
                                        .font(theme::regular(ty::BODY)),
                                );
                                if focus_body {
                                    r.request_focus();
                                    if let Some(mut st) = egui::TextEdit::load_state(ui.ctx(), r.id) {
                                        st.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(0))));
                                        st.store(ui.ctx(), r.id);
                                    }
                                }
                            });
                        });
                    });

                    if let Some(e) = &c.error {
                        let before = ui.cursor().top();
                        let from_addr = accts
                            .iter()
                            .find(|(i, _, _)| *i == c.acct)
                            .and_then(|(_, _, ids)| ids.get(c.from).or(ids.first()))
                            .map(|a| compose::bare(a))
                            .unwrap_or_default();
                        // Compact here: the composer must keep its Send bar in view.
                        egui::Frame::new().inner_margin(egui::Margin { left: 12, right: 12, top: 4, bottom: 0 }).show(ui, |ui| {
                            match crate::admin::classify(e, "", &from_addr) {
                                Some(issue) => {
                                    let text = format!("{}. Copy the fix to send to your IT admin.", issue.short);
                                    if kit::notice(ui, Tone::Warning, &text, Some(issue.copy_label)) {
                                        ui.ctx().copy_text(issue.copy_text.clone());
                                        self_status_copied = true;
                                    }
                                }
                                None => {
                                    kit::notice(ui, Tone::Error, e, None);
                                }
                            }
                        });
                        let h = ui.cursor().top() - before;
                        ctx.data_mut(|d| d.insert_temp(err_key, h));
                    }
                    // Footer: Send, Attach, Save draft · status ……… Discard
                    ui.horizontal(|ui| {
                        ui.set_height(60.0);
                        ui.add_space(16.0);
                        let send = ui
                            .add_enabled_ui(!locked, |ui| kit::primary_button(ui, Some(ic::ICON_SEND), "Send"))
                            .inner
                            .on_hover_text("Send (Ctrl+Enter)");
                        if send.clicked() {
                            acts.push(Action::Send(id));
                        }
                        ui.add_space(4.0);
                        ui.add_enabled_ui(!locked, |ui| {
                            if kit::icon_button(ui, ic::ICON_ATTACH_FILE, "Attach files").clicked() {
                                acts.push(Action::AttachFiles(id));
                            }
                            if kit::icon_button(ui, ic::ICON_SAVE, "Save draft to the server").clicked() {
                                acts.push(Action::SaveDraft(id, false));
                            }
                        });
                        let status = if c.sending {
                            "Sending…".to_string()
                        } else if c.saving.is_some() {
                            "Saving to Drafts…".to_string()
                        } else if let Some(n) = &c.note {
                            if n == "Draft saved" {
                                "Saved to Drafts".to_string()
                            } else {
                                n.clone()
                            }
                        } else if c.dirty() && c.content_hash() == c.persisted {
                            "Saved on this device".to_string()
                        } else {
                            String::new()
                        };
                        if c.sending || c.saving.is_some() {
                            ui.add(egui::Spinner::new().size(14.0).color(p.text_secondary));
                        }
                        ui.label(RichText::new(status).size(ty::META).color(p.text_secondary));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.add_space(8.0);
                            if ui.add_enabled_ui(!locked, |ui| kit::icon_button(ui, ic::ICON_DELETE, "Discard")).inner.clicked() {
                                acts.push(Action::RequestClose(id));
                            }
                        });
                    });
                });
            ctx.data_mut(|d| d.insert_temp(st_id, (minimised, expanded)));
            if let (Some(r), Some(ptr)) = (&resp, pointer) {
                if r.response.rect.contains(ptr) && !locked {
                    drop_target = Some(id);
                }
            }
        }
        if self_status_copied {
            self.status = "Copied. Send it to your IT admin.".into();
        }
        if let Some((id, expanded)) = raise {
            // Restored: un-minimise and bring to the front of the docked row (or expand it).
            ctx.data_mut(|d| {
                let st_id = Id::unique(("compose-state", id));
                d.insert_temp(st_id, (false, expanded));
                d.insert_temp(Id::unique(("compose-raised", id)), ctx.cumulative_pass_nr());
            });
        }
        // Files attach only to the composer they were dropped on; with none
        // open, a new message is started.
        if !dropped.is_empty() {
            match drop_target {
                Some(t) => {
                    if let Some(c) = self.composes.iter_mut().find(|c| c.id == t) {
                        c.files.extend(dropped);
                    }
                }
                None if self.composes.is_empty() => {
                    let a = self.current_acct();
                    if let Some(c) = self.new_compose(a) {
                        c.files.extend(dropped);
                    }
                }
                None => self.status = "Drop files onto a message to attach them".into(),
            }
        }
        self.acts.extend(acts);
    }

    /// Begin the browser sign-in for an OAuth account.
    fn start_oauth(ed: &mut Editor, tx: &Sender<Event>, ctx: &egui::Context) {
        ed.oauth = OAuthState::Waiting;
        let (kind, cid, secret, hint) =
            (ed.auth, ed.client_id.trim().to_string(), ed.client_secret.trim().to_string(), ed.email.trim().to_string());
        let (tx, ctx2, eid) = (tx.clone(), ctx.clone(), ed.id);
        std::thread::spawn(move || {
            let result = oauth::authorize(kind, &cid, &secret, &hint);
            let _ = tx.send(Event { acct: String::new(), gen: 0, kind: Ev::OAuth { editor: eid, result } });
            ctx2.request_repaint();
        });
    }

    /// The provider-specific sign-in controls (password or OAuth).
    fn signin_controls(
        ui: &mut egui::Ui,
        ed: &mut Editor,
        show_password: &mut bool,
        settings: &crate::config::Settings,
        tx: &Sender<Event>,
    ) {
        let p = kit::pal(ui);
        let ctx = ui.ctx().clone();
        let (text, links) = ed.provider.guide(ed.auth);
        egui::Frame::new().fill(p.window).corner_radius(egui::CornerRadius::same(6)).inner_margin(egui::Margin::same(12)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(egui::Label::new(RichText::new(text).size(ty::META + 1.0).color(p.text)).wrap());
            for (label, url) in links.iter() {
                ui.hyperlink_to(RichText::new(format!("{label} ↗")).size(ty::META + 1.0).color(p.accent), *url);
            }
        });
        ui.add_space(12.0);
        match ed.auth {
            AuthKind::Password => {
                ui.label(RichText::new(if ed.provider == Provider::Custom { "Password" } else { "App password" }).color(p.text_secondary));
                ui.horizontal(|ui| {
                    let r = ui.add(
                        kit::text_field(&mut ed.password, if ed.editing.is_some() { "Unchanged" } else { "" })
                            .password(!*show_password)
                            .desired_width(ui.available_width() - 40.0),
                    );
                    if r.lost_focus() && ui.input(|i| i.key_pressed(K::Enter)) {
                        ui.memory_mut(|m| m.data.insert_temp(Id::unique("setup-submit"), true));
                    }
                    if kit::icon_button(
                        ui,
                        if *show_password { ic::ICON_VISIBILITY_OFF } else { ic::ICON_VISIBILITY },
                        if *show_password { "Hide password" } else { "Show password" },
                    )
                    .clicked()
                    {
                        *show_password = !*show_password;
                    }
                });
                if ed.provider == Provider::Gmail
                    && ui
                        .add(
                            egui::Button::new(RichText::new("Use Google sign-in instead").size(ty::META + 1.0).color(p.accent))
                                .frame(false),
                        )
                        .clicked()
                {
                    ed.auth = AuthKind::OAuthGoogle;
                    Self::default_client(ed, settings);
                }
            }
            _ => {
                let has_default = match ed.auth {
                    AuthKind::OAuthMicrosoft => !settings.microsoft_client_id.trim().is_empty(),
                    _ => !settings.google_client_id.trim().is_empty(),
                };
                if !has_default || ed.client_id.trim().is_empty() {
                    ui.label(RichText::new("Client ID").color(p.text_secondary));
                    ui.add(kit::text_field(&mut ed.client_id, "Paste the Application (client) ID").desired_width(f32::INFINITY));
                    if ed.auth == AuthKind::OAuthGoogle {
                        ui.label(RichText::new("Client secret").color(p.text_secondary));
                        ui.add(kit::text_field(&mut ed.client_secret, "").password(true).desired_width(f32::INFINITY));
                    }
                    ui.add_space(8.0);
                }
                let valid = oauth::validate_client_id(ed.auth, &ed.client_id);
                ui.horizontal(|ui| {
                    let waiting = matches!(ed.oauth, OAuthState::Waiting);
                    let label = if ed.auth == AuthKind::OAuthMicrosoft { "Sign in with Microsoft" } else { "Sign in with Google" };
                    let b = ui.add_enabled_ui(!waiting && valid.is_ok(), |ui| kit::secondary_button(ui, label)).inner;
                    if b.clicked() {
                        Self::start_oauth(ed, tx, &ctx);
                    }
                    match &ed.oauth {
                        OAuthState::Waiting => {
                            ui.add(egui::Spinner::new().size(14.0).color(p.text_secondary));
                            ui.label(RichText::new("Finish signing in in your browser…").size(ty::META + 1.0).color(p.text_secondary));
                        }
                        OAuthState::Done(_) => {
                            ui.label(kit::glyph(ic::ICON_CHECK_CIRCLE, 18.0, p.success));
                            ui.label(RichText::new("Signed in").color(p.success));
                        }
                        _ => {}
                    }
                });
                if let Err(e) = &valid {
                    if !ed.client_id.trim().is_empty() {
                        kit::notice(ui, Tone::Warning, e, None);
                    }
                }
                if let OAuthState::Failed(e) = &ed.oauth {
                    match crate::admin::classify(e, &ed.client_id, ed.email.trim()) {
                        Some(issue) => admin_card(ui, &issue),
                        None => {
                            kit::notice(ui, Tone::Error, e, None);
                        }
                    }
                }
            }
        }
    }

    /// Incoming and outgoing server settings, side by side.
    fn server_form(ui: &mut egui::Ui, ed: &mut Editor) {
        let p = kit::pal(ui);
        ui.columns(2, |cols| {
            for (ui, (title, srv, imap)) in
                cols.iter_mut().zip([("Incoming (IMAP)", &mut ed.imap, true), ("Outgoing (SMTP)", &mut ed.smtp, false)])
            {
                ui.label(RichText::new(title).font(theme::semibold(ty::UI)).color(p.text));
                ui.label(RichText::new("Server").size(ty::META).color(p.text_secondary));
                ui.add(
                    kit::text_field(&mut srv.host, if imap { "imap.example.com" } else { "smtp.example.com" }).desired_width(f32::INFINITY),
                );
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new("Security").size(ty::META).color(p.text_secondary));
                        let before = srv.security;
                        egui::ComboBox::from_id_salt(("sec", title))
                            .icon(kit::combo_chevron)
                            .selected_text(match srv.security {
                                Security::Tls => "SSL/TLS",
                                Security::StartTls => "STARTTLS",
                            })
                            .width(110.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut srv.security, Security::Tls, "SSL/TLS");
                                ui.selectable_value(&mut srv.security, Security::StartTls, "STARTTLS");
                            });
                        // Standard ports follow the security choice unless customised.
                        if before != srv.security {
                            let standard = [993u16, 143, 465, 587];
                            if standard.contains(&srv.port) {
                                srv.port = match (imap, srv.security) {
                                    (true, Security::Tls) => 993,
                                    (true, Security::StartTls) => 143,
                                    (false, Security::Tls) => 465,
                                    (false, Security::StartTls) => 587,
                                };
                            }
                        }
                    });
                    ui.vertical(|ui| {
                        ui.label(RichText::new("Port").size(ty::META).color(p.text_secondary));
                        ui.add(egui::DragValue::new(&mut srv.port).range(1..=65535));
                    });
                });
            }
        });
        ui.add_space(8.0);
        ui.label(RichText::new("User name").size(ty::META).color(p.text_secondary));
        if ui.add(kit::text_field(&mut ed.username, "Usually your email address").desired_width(f32::INFINITY)).changed() {
            ed.username_touched = true;
        }
    }

    /// Run the incoming/outgoing sign-in checks for the wizard on a background thread.
    fn start_verify(&mut self) {
        let Some(st) = self.setup.as_mut() else { return };
        st.error = None;
        let ed = &mut st.ed;
        let id = ed.new_id.clone().unwrap_or_else(crate::config::new_account_id);
        ed.new_id = Some(id.clone());
        if ed.username.trim().is_empty() {
            ed.username = ed.email.trim().to_string();
        }
        let stored = match (&ed.auth, &ed.oauth) {
            (AuthKind::Password, _) if ed.password.is_empty() => Err("Enter your password".to_string()),
            (AuthKind::Password, _) => secrets::set(&id, secrets::PASSWORD, &ed.password),
            (_, OAuthState::Done(t)) => secrets::set(&id, secrets::REFRESH, t),
            _ => Err("Sign in with your browser first".to_string()),
        };
        if let Err(e) = stored {
            st.error = Some(e);
            return;
        }
        let acct = Account {
            id,
            name: ed.name.trim().to_string(),
            email: ed.email.trim().to_string(),
            username: ed.username.trim().to_string(),
            imap: Server { host: ed.imap.host.trim().into(), ..ed.imap.clone() },
            smtp: Server { host: ed.smtp.host.trim().into(), ..ed.smtp.clone() },
            auth: ed.auth,
            oauth_client_id: ed.client_id.trim().into(),
            oauth_client_secret: ed.client_secret.trim().into(),
            save_sent: ed.save_sent,
            signature: String::new(),
            paused: false,
            notify: true,
            aliases: vec![],
        };
        st.step = SetupStep::Verify;
        st.incoming = Check::Running;
        st.outgoing = Check::Running;
        let (tx, ctx, setup) = (self.tx.clone(), self.ctx.clone(), st.id);
        std::thread::spawn(move || {
            oauth::forget(&acct.id);
            let imap = crate::net::connect(&acct).map(|mut c| {
                let _ = c.sess.logout();
            });
            let imap = imap.map_err(|e| e.message().to_string());
            let smtp = crate::smtp::verify(&acct);
            let _ = tx.send(Event { acct: String::new(), gen: 0, kind: Ev::Verified { setup, imap, smtp } });
            ctx.request_repaint();
        });
    }

    /// Both checks passed: save the account and start syncing.
    fn finish_setup(&mut self) {
        let Some(mut st) = self.setup.take() else { return };
        self.editor = Some(std::mem::replace(&mut st.ed, Self::blank_editor(0)));
        let result = self.save_editor();
        st.ed = self.editor.take().unwrap_or_else(|| Self::blank_editor(0));
        match result {
            Ok(()) => {
                st.added = self.accts.iter().rposition(|a| a.live());
                st.step = SetupStep::Done;
            }
            Err(e) => {
                st.error = Some(e);
                st.step = SetupStep::SignIn;
            }
        }
        self.setup = Some(st);
    }

    fn blank_editor(id: u64) -> Editor {
        let (imap, smtp, auth, save_sent) = Provider::Custom.preset();
        Editor {
            id,
            editing: None,
            provider: Provider::Custom,
            name: String::new(),
            email: String::new(),
            username: String::new(),
            username_touched: false,
            password: String::new(),
            auth,
            imap,
            smtp,
            client_id: String::new(),
            client_secret: String::new(),
            save_sent,
            signature: String::new(),
            aliases: String::new(),
            notify: true,
            oauth: OAuthState::Idle,
            error: None,
            new_id: None,
        }
    }

    fn cancel_setup(&mut self) {
        if let Some(st) = self.setup.take() {
            // Nothing was saved: drop any credential stored for verification.
            if st.step != SetupStep::Done {
                if let Some(id) = &st.ed.new_id {
                    secrets::delete(id, secrets::PASSWORD);
                    secrets::delete(id, secrets::REFRESH);
                }
            }
        }
    }

    /// The add-account wizard.
    fn ui_editor(&mut self, ctx: &egui::Context) {
        let p = theme::palette(ctx);
        let settings = self.cfg.settings.clone();
        let Some(st) = self.setup.as_mut() else { return };
        let mut close = false;
        let mut continue_email = false;
        let mut verify = false;
        let tx = self.tx.clone();
        let existing: Vec<(String, String)> =
            self.accts.iter().filter(|a| a.live()).map(|a| (a.cfg.email.to_ascii_lowercase(), a.cfg.imap.host.clone())).collect();
        let modal = egui::Modal::new(Id::unique("setup-modal"))
            .frame(
                egui::Frame::new()
                    .fill(p.raised)
                    .corner_radius(egui::CornerRadius::same(8))
                    .inner_margin(egui::Margin::same(28))
                    .shadow(ctx.global_style().visuals.window_shadow),
            )
            .show(ctx, |ui| {
                ui.set_width(460.0f32.min(ctx.content_rect().width() - 32.0 - 56.0));
                // Progress: Email · Sign in · Verify · Done
                let stage = match st.step {
                    SetupStep::Email | SetupStep::Detecting => 0,
                    SetupStep::SignIn | SetupStep::Manual => 1,
                    SetupStep::Verify => 2,
                    SetupStep::Done => 3,
                };
                ui.horizontal(|ui| {
                    for (i, name) in ["Email", "Sign in", "Verify", "Done"].iter().enumerate() {
                        let color = if i <= stage { p.accent } else { p.text_disabled };
                        ui.label(RichText::new(*name).size(ty::META).color(color).strong());
                        if i < 3 {
                            ui.label(RichText::new("—").size(ty::META).color(p.divider));
                        }
                    }
                });
                ui.add_space(16.0);
                let submit = ui.memory_mut(|m| m.data.remove_temp::<bool>(Id::unique("setup-submit"))).unwrap_or(false);
                // Short windows: the step scrolls in a box that fits the window.
                let max_h = (ctx.content_rect().height() - 32.0 - 56.0 - 40.0).max(120.0);
                let short = max_h < 520.0;
                let mut body = |ui: &mut egui::Ui| match st.step {
                    SetupStep::Email | SetupStep::Detecting => {
                        kit::section_title(ui, "Add an email account");
                        ui.label(RichText::new("Email address").color(p.text_secondary));
                        let r = ui.add_enabled(
                            st.step == SetupStep::Email,
                            kit::text_field(&mut st.ed.email, "you@example.com").desired_width(f32::INFINITY).id(Id::unique("setup-email")),
                        );
                        if st.step == SetupStep::Email && !r.has_focus() && st.ed.email.is_empty() {
                            r.request_focus();
                        }
                        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(K::Enter));
                        if let Some(e) = &st.error {
                            ui.add_space(8.0);
                            kit::notice(ui, Tone::Error, e, None);
                        }
                        ui.add_space(20.0);
                        ui.horizontal(|ui| {
                            if st.step == SetupStep::Detecting {
                                ui.add(egui::Spinner::new().size(16.0).color(p.text_secondary));
                                ui.label(RichText::new("Finding your settings…").color(p.text_secondary));
                            } else if ui.add(egui::Button::new(RichText::new("Set up manually").color(p.accent)).frame(false)).clicked() {
                                let email = st.ed.email.trim().to_string();
                                if !crate::autoconfig::looks_like_email(&email) {
                                    st.error = Some("Enter your email address first.".into());
                                    ui.memory_mut(|m| m.request_focus(Id::unique("setup-email")));
                                } else {
                                    st.error = None;
                                    Self::apply_preset(&mut st.ed, Provider::Custom, &settings);
                                    st.ed.username = email;
                                    st.step = SetupStep::Manual;
                                }
                            }
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let ok = crate::autoconfig::looks_like_email(st.ed.email.trim()) && st.step == SetupStep::Email;
                                if ui.add_enabled_ui(ok, |ui| kit::primary_button(ui, None, "Continue")).inner.clicked() || (enter && ok) {
                                    continue_email = true;
                                }
                                if kit::secondary_button(ui, "Cancel").clicked() {
                                    close = true;
                                }
                            });
                        });
                        if continue_email {
                            let email = st.ed.email.trim().to_ascii_lowercase();
                            if existing.iter().any(|(e, _)| *e == email) {
                                st.error = Some(format!("{email} is already added. Edit it in Settings › Accounts."));
                                continue_email = false;
                            } else {
                                st.error = None;
                                if !st.ed.username_touched {
                                    st.ed.username = st.ed.email.trim().to_string();
                                }
                            }
                        }
                    }
                    SetupStep::SignIn => {
                        kit::section_title(ui, "Sign in");
                        ui.label(RichText::new(&st.ed.email).color(p.text));
                        ui.label(RichText::new(&st.detected).size(ty::META).color(p.text_secondary));
                        ui.add_space(12.0);
                        Self::signin_controls(ui, &mut st.ed, &mut st.show_password, &settings, &tx);
                        ui.add_space(12.0);
                        ui.label(RichText::new("Your name (shown to people you email)").color(p.text_secondary));
                        ui.add(kit::text_field(&mut st.ed.name, "Optional").desired_width(f32::INFINITY));
                        if let Some(e) = &st.error {
                            ui.add_space(8.0);
                            kit::notice(ui, Tone::Error, e, None);
                        }
                        ui.add_space(20.0);
                        ui.horizontal(|ui| {
                            if ui.add(egui::Button::new(RichText::new("Server settings").color(p.accent)).frame(false)).clicked() {
                                st.step = SetupStep::Manual;
                            }
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let ready = match st.ed.auth {
                                    AuthKind::Password => !st.ed.password.is_empty(),
                                    _ => matches!(st.ed.oauth, OAuthState::Done(_)),
                                };
                                if ui.add_enabled_ui(ready, |ui| kit::primary_button(ui, None, "Continue")).inner.clicked()
                                    || (submit && ready)
                                {
                                    verify = true;
                                }
                                if kit::secondary_button(ui, "Back").clicked() {
                                    st.step = SetupStep::Email;
                                }
                            });
                        });
                    }
                    SetupStep::Manual => {
                        ui.horizontal(|ui| {
                            ui.label(kit::glyph(ic::ICON_MAIL, 18.0, p.text_secondary));
                            ui.label(RichText::new(st.ed.email.trim()).color(p.text));
                            if ui.add(egui::Button::new(RichText::new("Change").color(p.accent)).frame(false)).clicked() {
                                st.step = SetupStep::Email;
                                st.error = None;
                            }
                        });
                        ui.add_space(8.0);
                        kit::section_title(ui, "Server settings");
                        if !st.detected.is_empty() {
                            ui.label(RichText::new(&st.detected).size(ty::META + 1.0).color(p.text_secondary));
                            ui.add_space(8.0);
                        }
                        Self::server_form(ui, &mut st.ed);
                        ui.add_space(12.0);
                        Self::signin_controls(ui, &mut st.ed, &mut st.show_password, &settings, &tx);
                        if let Some(e) = &st.error {
                            ui.add_space(8.0);
                            kit::notice(ui, Tone::Error, e, None);
                        }
                        ui.add_space(20.0);
                        ui.horizontal(|ui| {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let ok = !st.ed.imap.host.trim().is_empty() && !st.ed.smtp.host.trim().is_empty();
                                if ui.add_enabled_ui(ok, |ui| kit::primary_button(ui, None, "Continue")).inner.clicked() || (submit && ok) {
                                    verify = true;
                                }
                                if kit::secondary_button(ui, "Back").clicked() {
                                    st.step = SetupStep::Email;
                                }
                            });
                        });
                    }
                    SetupStep::Verify => {
                        kit::section_title(ui, "Checking your account");
                        let row = |ui: &mut egui::Ui, label: &str, check: &Check| {
                            ui.horizontal(|ui| {
                                match check {
                                    Check::Running | Check::Waiting => {
                                        ui.add(egui::Spinner::new().size(18.0).color(p.text_secondary));
                                    }
                                    Check::Ok => {
                                        ui.label(kit::glyph(ic::ICON_CHECK_CIRCLE, 20.0, p.success));
                                    }
                                    Check::Failed(_) => {
                                        ui.label(kit::glyph(ic::ICON_ERROR, 20.0, p.error));
                                    }
                                }
                                ui.label(RichText::new(label).color(p.text));
                            });
                            if let Check::Failed(e) = check {
                                ui.horizontal(|ui| {
                                    ui.add_space(28.0);
                                    ui.add(egui::Label::new(RichText::new(e).size(ty::META + 1.0).color(p.text_secondary)).wrap());
                                });
                            }
                            ui.add_space(6.0);
                        };
                        ui.horizontal(|ui| {
                            ui.label(kit::glyph(ic::ICON_CHECK_CIRCLE, 20.0, p.success));
                            ui.label(RichText::new("Found settings").color(p.text));
                        });
                        ui.add_space(6.0);
                        row(ui, "Signed in to incoming mail", &st.incoming);
                        row(ui, "Signed in to outgoing mail", &st.outgoing);
                        let failed = matches!(st.incoming, Check::Failed(_)) || matches!(st.outgoing, Check::Failed(_));
                        if let Some(issue) = st.admin.as_ref().filter(|_| failed) {
                            ui.add_space(8.0);
                            admin_card(ui, issue);
                        }
                        if let Some(e) = &st.error {
                            kit::notice(ui, Tone::Error, e, None);
                        }
                        ui.add_space(20.0);
                        ui.horizontal(|ui| {
                            if failed
                                && ui.add(egui::Button::new(RichText::new("Edit server settings").color(p.accent)).frame(false)).clicked()
                            {
                                st.step = SetupStep::Manual;
                            }
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if failed {
                                    if kit::primary_button(ui, None, "Try again").clicked() {
                                        verify = true;
                                    }
                                    if kit::secondary_button(ui, "Back").clicked() {
                                        st.step = SetupStep::SignIn;
                                    }
                                } else if kit::secondary_button(ui, "Cancel").clicked() {
                                    close = true;
                                }
                            });
                        });
                    }
                    SetupStep::Done => {
                        ui.horizontal(|ui| {
                            ui.label(kit::glyph(ic::ICON_CHECK_CIRCLE, 28.0, p.success));
                            ui.label(
                                RichText::new(format!("{} is ready", st.ed.email.trim())).font(theme::semibold(ty::HEADING)).color(p.text),
                            );
                        });
                        ui.add_space(4.0);
                        ui.label(RichText::new("Your mail is syncing now.").color(p.text_secondary));
                        ui.add_space(16.0);
                        if let Some(i) = st.added {
                            if let Some(a) = self.accts.get_mut(i) {
                                ui.label(RichText::new("Signature (optional)").color(p.text_secondary));
                                if ui
                                    .add(
                                        egui::TextEdit::multiline(&mut a.cfg.signature)
                                            .desired_rows(3)
                                            .desired_width(f32::INFINITY)
                                            .margin(egui::Margin::same(8)),
                                    )
                                    .changed()
                                {
                                    self.cfg
                                        .accounts
                                        .iter_mut()
                                        .filter(|x| x.id == a.cfg.id)
                                        .for_each(|x| x.signature = a.cfg.signature.clone());
                                }
                                ui.add_space(8.0);
                                let mut n = a.cfg.notify;
                                if kit::switch_labeled(ui, &mut n, "Notify me about new mail") {
                                    a.cfg.notify = n;
                                    a.shared.notify.store(settings.notifications && n, Ordering::Relaxed);
                                }
                            }
                        }
                        ui.add_space(20.0);
                        ui.horizontal(|ui| {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if kit::primary_button(ui, None, "Open inbox").clicked() {
                                    close = true;
                                }
                                if kit::secondary_button(ui, "Add another").clicked() {
                                    self.acts.push(Action::AddAccount);
                                    close = true;
                                }
                            });
                        });
                    }
                };
                if short {
                    egui::ScrollArea::vertical()
                        .id_salt("setup-scroll")
                        .max_height(max_h)
                        .auto_shrink([false, false])
                        .show(ui, |ui| body(ui));
                } else {
                    body(ui);
                }
            });
        if modal.should_close() && !matches!(self.setup.as_ref().map(|s| s.step), Some(SetupStep::Done)) {
            close = true;
        }
        if let Some(st) = self.setup.as_mut() {
            if continue_email {
                st.step = SetupStep::Detecting;
                let (tx, ctx2, sid, email) = (self.tx.clone(), ctx.clone(), st.id, st.ed.email.trim().to_string());
                std::thread::spawn(move || {
                    let found = crate::autoconfig::lookup(&email);
                    let _ = tx.send(Event { acct: String::new(), gen: 0, kind: Ev::Autoconfig { setup: sid, found } });
                    ctx2.request_repaint();
                });
            }
        }
        if verify {
            self.start_verify();
        }
        if close {
            if matches!(self.setup.as_ref().map(|s| s.step), Some(SetupStep::Done)) {
                self.save_config();
                self.setup = None;
            } else {
                self.cancel_setup();
            }
        }
    }

    fn ui_settings(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let p = theme::palette(ctx);
        let mut changed = false;
        let mut update_now = false;
        let mut install: Option<update::Release> = None;
        let mut close = false;
        let mut save_account = false;
        let mut leave: Option<Leave> = None;
        let tx = self.tx.clone();
        let accts: Vec<(String, String)> =
            self.accts.iter().filter(|a| a.live()).map(|a| (a.cfg.id.clone(), a.cfg.label().to_string())).collect();
        let screen = ctx.content_rect();
        let size = egui::vec2(840.0f32.min(screen.width() - 32.0), 560.0f32.min(screen.height() - 32.0));
        let modal = egui::Modal::new(Id::unique("settings-modal"))
            .frame(
                egui::Frame::new()
                    .fill(p.raised)
                    .corner_radius(egui::CornerRadius::same(8))
                    .shadow(ctx.global_style().visuals.window_shadow),
            )
            .show(ctx, |ui| {
                ui.set_min_size(size);
                ui.set_max_size(size);
                ui.horizontal_top(|ui| {
                    // Sidebar
                    egui::Frame::new()
                        .fill(p.nav)
                        .corner_radius(egui::CornerRadius { nw: 8, sw: 8, ne: 0, se: 0 })
                        .inner_margin(egui::Margin::same(12))
                        .show(ui, |ui| {
                            ui.vertical(|ui| {
                                ui.set_width(176.0);
                                ui.set_height(size.y - 24.0);
                                ui.spacing_mut().item_spacing.y = 0.0;
                                ui.add_space(4.0);
                                ui.label(RichText::new("Settings").font(theme::semibold(ty::HEADING)).color(p.text));
                                ui.add_space(16.0);
                                for (page, icon, label) in [
                                    (SettingsPage::Appearance, ic::ICON_PALETTE, "Appearance"),
                                    (SettingsPage::Accounts, ic::ICON_PERSON, "Accounts"),
                                    (SettingsPage::Reading, ic::ICON_ARTICLE, "Reading"),
                                    (SettingsPage::Privacy, ic::ICON_SHIELD, "Privacy"),
                                    (SettingsPage::Notifications, ic::ICON_NOTIFICATIONS, "Notifications"),
                                    (SettingsPage::Sync, ic::ICON_SYNC, "Sync"),
                                    (SettingsPage::Advanced, ic::ICON_TUNE, "Advanced"),
                                    (SettingsPage::About, ic::ICON_INFO, "About & updates"),
                                ] {
                                    if kit::nav_row(ui, icon, label, 0, self.settings_page == page, 0.0, true).clicked() {
                                        leave = Some(Leave::Page(page));
                                    }
                                }
                            })
                        });
                    // Content
                    ui.vertical(|ui| {
                        ui.set_width(size.x - 200.0 - 8.0);
                        let top_right = ui.max_rect().right_top();
                        ui.add_space(24.0);
                        egui::ScrollArea::vertical()
                            .id_salt(("settings-page", self.settings_page as u8))
                            .auto_shrink([false, false])
                            .max_height(size.y - 24.0)
                            .show(ui, |ui| {
                                ui.horizontal_top(|ui| {
                                ui.add_space(24.0);
                                ui.vertical(|ui| {
                                ui.set_width(size.x - 200.0 - 8.0 - 24.0 - 72.0);
                                let s = &mut self.cfg.settings;
                                match self.settings_page {
                                    SettingsPage::Appearance => {
                                        kit::section_title(ui, "Appearance");
                                        kit::setting_row(ui, "Theme", "Follow your system, or always use light or dark.", |ui| {
                                            for (v, l) in [("dark", "Dark"), ("light", "Light"), ("system", "System")] {
                                                if ui.selectable_label(s.theme == v, l).clicked() && s.theme != v {
                                                    s.theme = v.into();
                                                    theme::set_preference(ui.ctx(), v);
                                                    changed = true;
                                                }
                                            }
                                        });
                                        changed |= kit::setting_switch(ui, "Compact message list", "Shorter rows, so more messages fit.", &mut s.compact);
                                    }
                                    SettingsPage::Reading => {
                                        kit::section_title(ui, "Reading");
                                        changed |= kit::setting_switch(ui, "Mark as read when opened", "Otherwise messages stay unread until you mark them.", &mut s.mark_read_on_open);
                                        changed |= kit::setting_switch(ui, "Prefer plain text", "Show the text version of messages that have both. Switch per message from the More menu.", &mut s.prefer_plain_text);
                                    }
                                    SettingsPage::Privacy => {
                                        kit::section_title(ui, "Privacy");
                                        changed |= kit::setting_switch(ui, "Load remote images, styles and fonts", "Senders can learn when you open a message. When off, you can load them per message.", &mut s.load_remote_content);
                                        ui.label(
                                            RichText::new("Scripts, forms and embedded frames in emails are always blocked.")
                                                .size(ty::META)
                                                .color(p.text_secondary),
                                        );
                                    }
                                    SettingsPage::Notifications => {
                                        kit::section_title(ui, "Notifications");
                                        changed |= kit::setting_switch(ui, "Desktop notifications", "For new mail in your inboxes, while Mail isn't focused.", &mut s.notifications);
                                        let enabled = s.notifications;
                                        changed |= ui
                                            .add_enabled_ui(enabled, |ui| {
                                                kit::setting_switch(ui, "Show sender and subject", "Turn off to show only that new mail arrived.", &mut s.notify_preview)
                                            })
                                            .inner;
                                        ui.label(
                                            RichText::new("You can turn notifications off for a single account in Accounts.")
                                                .size(ty::META)
                                                .color(p.text_secondary),
                                        );
                                    }
                                    SettingsPage::Sync => {
                                        kit::section_title(ui, "Sync");
                                        kit::setting_row(
                                            ui,
                                            "Check for new mail every",
                                            "Used when a server doesn't support push. With push, new mail arrives instantly.",
                                            |ui| {
                                                changed |= ui
                                                    .add(egui::DragValue::new(&mut s.poll_secs).range(15..=3600).suffix(" seconds"))
                                                    .changed();
                                            },
                                        );
                                        kit::setting_row(
                                            ui,
                                            "Messages to keep per folder",
                                            "How many recent messages each folder downloads (and per “Load older”). Applies after restart.",
                                            |ui| {
                                                changed |= ui.add(egui::DragValue::new(&mut s.initial_fetch).range(50..=5000)).changed();
                                            },
                                        );
                                    }
                                    SettingsPage::Advanced => {
                                        kit::section_title(ui, "Advanced");
                                        kit::setting_row(ui, "Send new messages from", "Used in All inboxes and search results.", |ui| {
                                            let current = accts
                                                .iter()
                                                .find(|(id, _)| *id == s.default_account)
                                                .map(|(_, l)| l.clone())
                                                .unwrap_or_else(|| "First account".into());
                                            egui::ComboBox::from_id_salt("default-acct").icon(kit::combo_chevron).selected_text(current).width(200.0).show_ui(
                                                ui,
                                                |ui| {
                                                    for (id, label) in &accts {
                                                        changed |= ui.selectable_value(&mut s.default_account, id.clone(), label).changed();
                                                    }
                                                },
                                            );
                                        });
                                        ui.label(RichText::new("OAuth app registrations").font(theme::semibold(ty::UI)).color(p.text));
                                        ui.label(
                                            RichText::new("Used when adding Microsoft or Google accounts with browser sign-in.")
                                                .size(ty::META)
                                                .color(p.text_secondary),
                                        );
                                        ui.add_space(8.0);
                                        for (label, val, secret) in [
                                            ("Microsoft client ID", &mut s.microsoft_client_id, false),
                                            ("Google client ID", &mut s.google_client_id, false),
                                            ("Google client secret", &mut s.google_client_secret, true),
                                        ] {
                                            ui.label(RichText::new(label).size(ty::META).color(p.text_secondary));
                                            changed |=
                                                ui.add(kit::text_field(val, "").password(secret).desired_width(f32::INFINITY)).changed();
                                            ui.add_space(6.0);
                                        }
                                        ui.add_space(8.0);
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                RichText::new(format!("Mail data: {}", crate::config::cache_dir().display()))
                                                    .size(ty::META)
                                                    .color(p.text_secondary),
                                            );
                                            if ui
                                                .add(egui::Button::new(RichText::new("Open").size(ty::META).color(p.accent)).frame(false))
                                                .clicked()
                                            {
                                                let _ = open::that(crate::config::cache_dir());
                                            }
                                        });
                                    }
                                    SettingsPage::About => {
                                        kit::section_title(ui, "About & updates");
                                        ui.label(
                                            RichText::new(format!("Mail {}", update::current()))
                                                .font(theme::semibold(ty::UI))
                                                .color(p.text),
                                        );
                                        ui.add_space(12.0);
                                        changed |= kit::setting_switch(ui, "Install updates automatically", "Mail checks GitHub daily and installs updates when you close it.", &mut s.auto_update);
                                        ui.horizontal(|ui| {
                                            let busy = matches!(self.update, UpdateState::Checking | UpdateState::Downloading(_));
                                            if ui
                                                .add_enabled_ui(!busy && self.staged.is_none() && update::platform().is_some(), |ui| {
                                                    kit::secondary_button(ui, "Check for updates")
                                                })
                                                .inner
                                                .clicked()
                                            {
                                                update_now = true;
                                            }
                                            match &self.update {
                                                UpdateState::Idle => {}
                                                UpdateState::Checking => {
                                                    ui.add(egui::Spinner::new().size(14.0).color(p.text_secondary));
                                                    ui.label(RichText::new("Checking…").color(p.text_secondary));
                                                }
                                                UpdateState::UpToDate => {
                                                    ui.label(kit::glyph(ic::ICON_CHECK_CIRCLE, 18.0, p.success));
                                                    ui.label(RichText::new("You're up to date").color(p.text));
                                                }
                                                UpdateState::Available(r) => {
                                                    if kit::primary_button(
                                                        ui,
                                                        Some(ic::ICON_SYSTEM_UPDATE),
                                                        &format!("Install {}", r.version),
                                                    )
                                                    .clicked()
                                                    {
                                                        install = Some(r.clone());
                                                    }
                                                }
                                                UpdateState::Downloading(v) => {
                                                    ui.add(egui::Spinner::new().size(14.0).color(p.text_secondary));
                                                    ui.label(RichText::new(format!("Downloading {v}…")).color(p.text_secondary));
                                                }
                                                UpdateState::Ready => {
                                                    ui.label(kit::glyph(ic::ICON_CHECK_CIRCLE, 18.0, p.success));
                                                    ui.label(RichText::new("Ready — restart Mail to finish").color(p.text));
                                                }
                                                UpdateState::Failed(e) => {
                                                    ui.label(kit::glyph(ic::ICON_ERROR, 18.0, p.error));
                                                    ui.add(
                                                        egui::Label::new(
                                                            RichText::new(format!("Update failed: {e}")).size(ty::META + 1.0).color(p.text),
                                                        )
                                                        .wrap(),
                                                    );
                                                }
                                            }
                                        });
                                        ui.add_space(12.0);
                                        ui.hyperlink_to(
                                            RichText::new("Release notes ↗").color(p.accent),
                                            "https://github.com/MattGodwin/mail-rs/releases",
                                        );
                                    }
                                    SettingsPage::Accounts => {
                                        if let Some(ed) = self.editor.as_mut() {
                                            let issue = ed.editing.and_then(|i| self.accts.get(i)).and_then(|a| match &a.state {
                                                AcctState::AuthFailed(e) | AcctState::Offline(e) => {
                                                    crate::admin::classify(e, &a.cfg.oauth_client_id, &a.cfg.email)
                                                }
                                                _ => None,
                                            });
                                            save_account =
                                                Self::account_detail(ui, ed, &self.cfg.settings, &tx, &mut self.acts, issue.as_ref());
                                        } else {
                                            kit::section_title(ui, "Accounts");
                                            for (i, a) in self.accts.iter().enumerate().filter(|(_, a)| a.live()) {
                                                ui.horizontal(|ui| {
                                                    ui.set_height(52.0);
                                                    kit::avatar(ui, if a.cfg.name.is_empty() { &a.cfg.email } else { &a.cfg.name }, 32.0);
                                                    ui.vertical(|ui| {
                                                        ui.spacing_mut().item_spacing.y = 0.0;
                                                        ui.label(RichText::new(&a.cfg.email).font(theme::semibold(ty::UI)).color(p.text));
                                                        let (txt, col) = if a.cfg.paused {
                                                            ("Paused".to_string(), p.text_secondary)
                                                        } else {
                                                            match &a.state {
                                                                AcctState::Online => (
                                                                    if a.push { "Connected · push".into() } else { "Connected".into() },
                                                                    p.success,
                                                                ),
                                                                AcctState::Connecting => ("Connecting…".into(), p.text_secondary),
                                                                AcctState::Offline(_) => ("Offline".into(), p.warning),
                                                                AcctState::AuthFailed(_) => ("Sign-in failed".into(), p.error),
                                                            }
                                                        };
                                                        ui.label(RichText::new(txt).size(ty::META).color(col));
                                                    });
                                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                                        if kit::secondary_button(ui, "Edit").clicked() {
                                                            self.acts.push(Action::EditAccount(i));
                                                        }
                                                    });
                                                });
                                                let r = ui.available_rect_before_wrap();
                                                ui.painter().hline(r.x_range(), r.top(), Stroke::new(1.0, p.divider));
                                            }
                                            ui.add_space(12.0);
                                            if kit::quiet_button(ui, Some(ic::ICON_PERSON_ADD), "Add account").clicked() {
                                                self.acts.push(Action::AddAccount);
                                            }
                                        }
                                    }
                                }
                                });
                                });
                            });
                        // Close button, top-right, aligned with the page heading (drawn last, on top).
                        let close_rect = egui::Rect::from_min_size(egui::pos2(top_right.x - 48.0, top_right.y + 20.0), egui::vec2(32.0, 32.0));
                        ui.scope_builder(egui::UiBuilder::new().max_rect(close_rect), |ui| {
                            if kit::icon_button(ui, ic::ICON_CLOSE, "Close (Esc)").clicked() {
                                close = true;
                            }
                        });
                    });
                });
            });
        if modal.should_close() && self.confirm.is_none() && self.leave_pending.is_none() {
            close = true;
        }
        if save_account {
            self.save_account_edits();
        }
        // Unsaved account edits: Save changes / Discard / Keep editing.
        if let Some(to) = self.leave_pending {
            let mut choice = None;
            let r = egui::Modal::new(Id::unique("leave-account")).frame(kit::dialog_frame(ctx)).show(ctx, |ui| {
                ui.set_width(420.0);
                ui.label(RichText::new("Save changes to this account?").font(theme::semibold(ty::HEADING)).color(p.text));
                ui.add_space(8.0);
                ui.add(
                    egui::Label::new(
                        RichText::new("You changed this account's settings. Save them before leaving?").color(p.text_secondary),
                    )
                    .wrap(),
                );
                ui.add_space(20.0);
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new(RichText::new("Discard").color(p.error)).frame(false)).clicked() {
                        choice = Some(2);
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if kit::primary_button(ui, None, "Save changes").clicked() {
                            choice = Some(1);
                        }
                        if kit::secondary_button(ui, "Keep editing").clicked() {
                            choice = Some(0);
                        }
                    });
                });
            });
            if r.should_close() && choice.is_none() {
                choice = Some(0);
            }
            match choice {
                Some(1) => {
                    self.leave_pending = None;
                    if self.save_account_edits() {
                        self.apply_leave(to);
                    }
                }
                Some(2) => self.apply_leave(to),
                Some(_) => self.leave_pending = None,
                None => {}
            }
        }
        if update_now {
            let auto = self.cfg.settings.auto_update;
            self.check_updates(auto);
        }
        if let Some(r) = install {
            self.install_update(r);
        }
        if changed {
            self.sync_shared();
            if let Err(e) = self.cfg.save() {
                self.status = e;
            }
        }
        if let Some(to) = leave {
            self.request_leave(to);
        }
        if close {
            self.request_leave(Leave::Close);
        }
    }

    /// Returns true when saved; otherwise the error shows in the editor.
    fn save_account_edits(&mut self) -> bool {
        match self.save_editor() {
            Ok(()) => {
                self.editor = None;
                self.editor_baseline = None;
                self.status = "Account saved".into();
                true
            }
            Err(e) => {
                if let Some(ed) = self.editor.as_mut() {
                    ed.error = Some(e);
                }
                false
            }
        }
    }

    /// Settings › Accounts › one account. Returns true when "Save changes" is pressed.
    fn account_detail(
        ui: &mut egui::Ui,
        ed: &mut Editor,
        settings: &crate::config::Settings,
        tx: &Sender<Event>,
        acts: &mut Vec<Action>,
        issue: Option<&crate::admin::Issue>,
    ) -> bool {
        let p = kit::pal(ui);
        let mut save = false;
        ui.horizontal(|ui| {
            if kit::icon_button(ui, ic::ICON_ARROW_BACK, "All accounts").clicked() {
                ui.memory_mut(|m| m.data.insert_temp(Id::unique("acct-back"), true));
            }
            ui.label(RichText::new(&ed.email).font(theme::semibold(ty::HEADING)).color(p.text));
        });
        ui.add_space(12.0);
        if let Some(issue) = issue {
            admin_card(ui, issue);
            ui.add_space(8.0);
        }
        let group = |ui: &mut egui::Ui, title: &str| {
            ui.add_space(8.0);
            ui.label(RichText::new(title).font(theme::semibold(ty::UI)).color(p.text));
            ui.add_space(6.0);
        };
        group(ui, "Identity");
        ui.label(RichText::new("Your name").size(ty::META).color(p.text_secondary));
        ui.add(kit::text_field(&mut ed.name, "").desired_width(f32::INFINITY));
        ui.add_space(6.0);
        ui.label(RichText::new("Sending aliases — one per line, e.g. Jane Doe <jane@work.example>").size(ty::META).color(p.text_secondary));
        ui.add(egui::TextEdit::multiline(&mut ed.aliases).desired_rows(2).desired_width(f32::INFINITY).margin(egui::Margin::same(8)));
        group(ui, "Signature");
        ui.add(egui::TextEdit::multiline(&mut ed.signature).desired_rows(3).desired_width(f32::INFINITY).margin(egui::Margin::same(8)));
        group(ui, "Notifications");
        kit::switch_labeled(ui, &mut ed.notify, "Notify me about new mail in this account");
        group(ui, "Sign-in");
        // Reveal state lives across frames, per editor.
        let show_id = Id::unique(("acct-show-password", ed.new_id.clone(), ed.email.clone()));
        let mut show = ui.data(|d| d.get_temp::<bool>(show_id).unwrap_or(false));
        Self::signin_controls(ui, ed, &mut show, settings, tx);
        ui.data_mut(|d| d.insert_temp(show_id, show));
        group(ui, "Servers");
        Self::server_form(ui, ed);
        ui.add_space(6.0);
        kit::switch_labeled(ui, &mut ed.save_sent, "Save a copy of sent mail to Sent");
        ui.label(
            RichText::new("Gmail and Outlook save sent mail automatically; leave this off for them.")
                .size(ty::META)
                .color(p.text_secondary),
        );
        if let Some(e) = &ed.error {
            ui.add_space(8.0);
            kit::notice(ui, Tone::Error, e, None);
        }
        ui.add_space(16.0);
        ui.horizontal(|ui| {
            if kit::primary_button(ui, None, "Save changes").clicked() {
                save = true;
            }
            if let Some(i) = ed.editing {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(egui::Button::new(RichText::new("Remove account…").color(p.error)).frame(false)).clicked() {
                        acts.push(Action::RemoveAccount(i));
                    }
                });
            }
        });
        if ui.memory_mut(|m| m.data.remove_temp::<bool>(Id::unique("acct-back"))).unwrap_or(false) {
            acts.push(Action::CloseAccountDetail);
        }
        save
    }

    fn ui_dialogs(&mut self, ctx: &egui::Context) {
        let p = theme::palette(ctx);
        if let Some((title, src)) = &self.source {
            let mut close = false;
            let screen = ctx.content_rect();
            let r = egui::Modal::new(Id::unique("source-modal")).frame(kit::dialog_frame(ctx)).show(ctx, |ui| {
                let size = egui::vec2((screen.width() - 96.0).min(900.0), (screen.height() - 120.0).min(640.0));
                ui.set_width(size.x);
                ui.horizontal(|ui| {
                    ui.label(kit::glyph(ic::ICON_CODE, 20.0, p.text_secondary));
                    let g = kit::one_line(ui, format!("Source · {title}"), theme::semibold(ty::HEADING), p.text, size.x - 80.0);
                    ui.label(g);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if kit::icon_button(ui, ic::ICON_CLOSE, "Close (Esc)").clicked() {
                            close = true;
                        }
                    });
                });
                ui.add_space(12.0);
                egui::Frame::new()
                    .fill(p.input)
                    .stroke(Stroke::new(1.0, p.divider))
                    .corner_radius(egui::CornerRadius::same(6))
                    .inner_margin(egui::Margin::same(12))
                    .show(ui, |ui| {
                        egui::ScrollArea::both().max_height(size.y - 80.0).auto_shrink([false, false]).show(ui, |ui| {
                            ui.add(
                                egui::Label::new(RichText::new(src).font(FontId::monospace(ty::MONO)).color(p.text))
                                    .extend()
                                    .selectable(true),
                            );
                        });
                    });
            });
            if close || r.should_close() {
                self.source = None;
            }
        }
        if self.shortcuts_open {
            let r = egui::Modal::new(Id::unique("shortcuts-modal")).frame(kit::dialog_frame(ctx)).show(ctx, |ui| {
                ui.set_width(460.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Keyboard shortcuts").font(theme::semibold(ty::HEADING)).color(p.text));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if kit::icon_button(ui, ic::ICON_CLOSE, "Close (Esc)").clicked() {
                            self.shortcuts_open = false;
                        }
                    });
                });
                ui.add_space(12.0);
                egui::Grid::new("sc").num_columns(2).spacing([24.0, 10.0]).show(ui, |ui| {
                    for (k, d) in SHORTCUTS {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            for part in k.split(", ") {
                                egui::Frame::new()
                                    .fill(p.input)
                                    .stroke(Stroke::new(1.0, p.divider))
                                    .corner_radius(egui::CornerRadius::same(4))
                                    .inner_margin(egui::Margin::symmetric(6, 2))
                                    .show(ui, |ui| {
                                        ui.label(RichText::new(part).font(FontId::monospace(ty::META)).color(p.text));
                                    });
                            }
                        });
                        ui.label(RichText::new(*d).color(p.text));
                        ui.end_row();
                    }
                });
            });
            if r.should_close() {
                self.shortcuts_open = false;
            }
        }

        // Closing a compose window with unsaved changes: Save draft / Discard draft / Cancel.
        if let Some(Confirm::CloseCompose(id)) = &self.confirm {
            let id = *id;
            let mut choice = None;
            let r = egui::Modal::new(Id::unique("close-compose")).frame(kit::dialog_frame(ctx)).show(ctx, |ui| {
                ui.set_width(400.0);
                ui.label(RichText::new("Save this message?").font(theme::semibold(ty::HEADING)).color(p.text));
                ui.add_space(8.0);
                ui.add(egui::Label::new(RichText::new("Save it to Drafts to finish later, or discard it.").color(p.text_secondary)).wrap());
                ui.add_space(20.0);
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new(RichText::new("Discard draft").color(p.error)).frame(false)).clicked() {
                        choice = Some(2);
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if kit::primary_button(ui, None, "Save draft").clicked() {
                            choice = Some(1);
                        }
                        if kit::secondary_button(ui, "Cancel").clicked() {
                            choice = Some(0);
                        }
                    });
                });
            });
            if r.should_close() && choice.is_none() {
                choice = Some(0);
            }
            match choice {
                Some(1) => {
                    self.confirm = None;
                    self.acts.push(Action::SaveDraft(id, true));
                }
                Some(2) => {
                    self.confirm = None;
                    self.acts.push(Action::CloseNow(id));
                }
                Some(_) => self.confirm = None,
                None => {}
            }
            return;
        }

        let mut decision = None;
        if let Some(c) = &self.confirm {
            let (title, text, action) = match c {
                Confirm::RemoveAccount(i) => (
                    format!("Remove {}?", self.accts.get(*i).map(|a| a.cfg.label()).unwrap_or("this account")),
                    "Its downloaded mail and saved sign-in are removed from this computer. Nothing is deleted on the server, and you can add it again later.".to_string(),
                    "Remove account",
                ),
                Confirm::DeleteForever(keys) => (
                    if keys.len() == 1 { "Delete permanently?".to_string() } else { format!("Delete {} messages permanently?", keys.len()) },
                    "This can't be undone.".to_string(),
                    "Delete permanently",
                ),
                Confirm::CloseCompose(_) => unreachable!(),
            };
            let r = egui::Modal::new(Id::unique("confirm")).frame(kit::dialog_frame(ctx)).show(ctx, |ui| {
                ui.set_width(400.0);
                ui.label(RichText::new(&title).font(theme::semibold(ty::HEADING)).color(p.text));
                ui.add_space(8.0);
                ui.add(egui::Label::new(RichText::new(&text).color(p.text_secondary)).wrap());
                ui.add_space(20.0);
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if kit::danger_button(ui, action).clicked() {
                            decision = Some(true);
                        }
                        if kit::secondary_button(ui, "Cancel").clicked() {
                            decision = Some(false);
                        }
                    });
                });
            });
            if r.should_close() && decision.is_none() {
                decision = Some(false);
            }
        }
        if let Some(yes) = decision {
            let c = self.confirm.take().unwrap();
            if yes {
                match c {
                    Confirm::RemoveAccount(i) if i < self.accts.len() => self.remove_account(i),
                    Confirm::DeleteForever(keys) => {
                        let moves = keys.into_iter().map(|k| (k, None)).collect();
                        self.triage(moves, String::new(), false);
                    }
                    _ => {}
                }
            }
        }
    }

    fn ui_quitting(&mut self, ctx: &egui::Context) {
        let p = theme::palette(ctx);
        let n = self.outstanding();
        egui::Modal::new(Id::unique("quitting")).frame(kit::dialog_frame(ctx)).show(ctx, |ui| {
            ui.set_width(400.0);
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(18.0).color(p.text_secondary));
                ui.label(RichText::new("Finishing up").font(theme::semibold(ty::HEADING)).color(p.text));
            });
            ui.add_space(8.0);
            ui.add(
                egui::Label::new(
                    RichText::new(format!("Waiting for {n} change(s) to reach the server before Mail closes.")).color(p.text_secondary),
                )
                .wrap(),
            );
            ui.add_space(20.0);
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if kit::danger_button(ui, "Quit anyway").on_hover_text("Unfinished changes may not reach the server").clicked() {
                        self.force_quit = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    if kit::secondary_button(ui, "Keep open").clicked() {
                        self.quitting = false;
                    }
                });
            });
        });
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.text_edit_focused() || self.confirm.is_some() || self.editor.is_some() || self.quitting {
            return;
        }
        // Keys belong to whatever window has focus, never to the inbox behind it.
        let focus_in_window =
            ctx.memory(|m| m.focused()).and_then(|id| ctx.read_response(id)).is_some_and(|r| r.layer_id.order != Order::Background);
        if focus_in_window {
            return;
        }
        let keys = self.cur.clone().map(|k| vec![k]).unwrap_or_default();
        let s = self.cur.as_ref().and_then(|k| self.summary(k)).cloned();
        let mut acts = Vec::new();
        ctx.input(|i| {
            let p = |k| i.key_pressed(k) && !i.modifiers.command && !i.modifiers.alt;
            if i.modifiers.command && i.key_pressed(K::Z) {
                acts.push(Action::Undo);
            }
            if p(K::J) || p(K::ArrowDown) {
                acts.push(Action::Move(1));
            }
            if p(K::K) || p(K::ArrowUp) {
                acts.push(Action::Move(-1));
            }
            if p(K::PageDown) {
                acts.push(Action::Move(10));
            }
            if p(K::PageUp) {
                acts.push(Action::Move(-10));
            }
            if p(K::C) || p(K::N) {
                acts.push(Action::Compose);
            }
            if p(K::F5) {
                acts.push(Action::Refresh);
            }
            if p(K::Questionmark) {
                self.shortcuts_open = true;
            }
            if p(K::Slash) {
                self.focus_filter = true;
            }
            if !keys.is_empty() {
                if p(K::R) {
                    acts.push(Action::Reply(false));
                }
                if p(K::A) {
                    acts.push(Action::Reply(true));
                }
                if p(K::F) {
                    acts.push(Action::Forward);
                }
                if p(K::E) {
                    acts.push(Action::Archive(keys.clone()));
                }
                if p(K::Delete) || p(K::Backspace) || (i.modifiers.shift && i.key_pressed(K::Num3)) {
                    acts.push(Action::Trash(keys.clone()));
                }
                if let Some(s) = &s {
                    if p(K::U) {
                        acts.push(Action::Flag(keys.clone(), MsgFlag::Seen, !s.seen));
                    }
                    if p(K::S) {
                        acts.push(Action::Flag(keys.clone(), MsgFlag::Flagged, !s.flagged));
                    }
                }
            }
        });
        self.acts.extend(acts);
    }

    /// Never lose work on exit: persist compose windows, push undo-pending
    /// changes to the server, and wait for in-flight work to finish.
    fn guard_exit(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested()) {
            for c in self.composes.iter_mut() {
                if c.content_hash() != c.persisted {
                    Self::persist(c, &self.accts);
                }
            }
            for p in std::mem::take(&mut self.pending) {
                self.dispatch(p.moves);
            }
            if self.outstanding() > 0 && !self.force_quit {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.quitting = true;
            } else {
                self.on_closing();
            }
        }
        if self.quitting {
            if self.outstanding() == 0 {
                self.force_quit = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                self.ui_quitting(ctx);
            }
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        self.focused.store(focused, Ordering::Relaxed);
        self.handle_events();
        self.poll(focused);
        if self.rows_dirty {
            self.rebuild_rows();
        }
        self.update_title();
        // TEMP demo hook
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        if let Ok(i) = std::env::var("MAIL_DEMO_OPEN") {
            if n == 2 && std::env::var("MAIL_DEMO_PANEL").is_ok_and(|v| v.starts_with("small")) {
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(760.0, 440.0)));
            }
            if n == 8 {
                if let Some(r) = self.rows.get(i.parse::<usize>().unwrap_or(0)) {
                    let k = r.key.clone();
                    self.open(k);
                }
            }
            if n == 20 {
                match std::env::var("MAIL_DEMO_PANEL").as_deref() {
                    Ok("settings") => self.settings_open = true,
                    Ok("editor") => self.open_editor(None),
                    Ok("compose") => self.reply(false),
                    Ok("accounts") => {
                        self.settings_open = true;
                        self.settings_page = SettingsPage::Accounts;
                    }
                    Ok("privacy") => {
                        self.settings_open = true;
                        self.settings_page = SettingsPage::Privacy;
                    }
                    Ok("admin") => self.open_editor(Some(0)),
                    Ok("small-compose") => {
                        self.reply(false);
                        if let Some(c) = self.composes.last_mut() {
                            c.error = Some("permanent error (535): 5.7.139 Authentication unsuccessful, SmtpClientAuthentication is disabled for the Mailbox.".into());
                        }
                    }
                    Ok("small-setup") => self.open_editor(None),
                    Ok("composes") => {
                        for _ in 0..4 {
                            let a = self.current_acct();
                            self.new_compose(a);
                        }
                    }
                    _ => {}
                }
            }
            if n >= 20 && std::env::var("MAIL_DEMO_PANEL").as_deref() == Ok("admin") {
                if let Some(a) = self.accts.get_mut(0) {
                    a.cfg.oauth_client_id = "8a3f1c2e-0b7d-4e59-9c61-2f4d8e7a1b30".into();
                    a.state = AcctState::AuthFailed(
                        "invalid_grant: AADSTS65001: The user or administrator has not consented to use the application.".into(),
                    );
                }
            }
            if n < 80 {
                ctx.request_repaint();
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.track_status();
        self.ui_top(ui);
        self.ui_update_banner(ui);
        self.ui_folders(ui);
        self.ui_list(ui);
        self.ui_reader(ui);
        self.ui_composes(&ctx);
        self.ui_editor(&ctx);
        self.ui_settings(&ctx);
        self.ui_dialogs(&ctx);
        self.ui_toast(&ctx);
        self.shortcuts(&ctx);
        self.autosave();
        self.guard_exit(&ctx);
        self.sync_webview(&ctx, frame);

        let acts = std::mem::take(&mut self.acts);
        if !acts.is_empty() {
            for a in acts {
                self.apply(a);
            }
            if self.rows_dirty {
                self.rebuild_rows();
            }
            ctx.request_repaint();
        }
    }
}

/// Where each composer goes this frame.
struct ComposeLayout {
    /// id -> (position, size, shown as a tab)
    slots: HashMap<u64, (egui::Pos2, egui::Vec2, bool)>,
    /// Drafts with no room on screen, listed under "More drafts".
    overflow: Vec<u64>,
    more_at: (egui::Pos2, f32),
}

/// Docked composers sit bottom-right, most recently opened or restored first;
/// minimised ones and those that don't fit become tabs to their left, and
/// tabs that don't fit go into one "More drafts" menu. Everything stays at
/// least 16 points inside the window.
fn compose_layout(ctx: &egui::Context, screen: Rect, composes: &[Compose]) -> ComposeLayout {
    const W: f32 = 560.0;
    const H: f32 = 520.0;
    const GAP: f32 = 12.0;
    const TAB_W: f32 = 240.0;
    const TAB_H: f32 = 44.0;
    const MORE_W: f32 = 160.0;
    let mut out = ComposeLayout { slots: HashMap::new(), overflow: vec![], more_at: (egui::Pos2::ZERO, MORE_W) };
    let pass = ctx.cumulative_pass_nr();
    // (raised, id, minimised, expanded)
    let mut all: Vec<(u64, u64, bool, bool)> = composes
        .iter()
        .map(|c| {
            let (m, e) = ctx.data(|d| d.get_temp::<(bool, bool)>(Id::unique(("compose-state", c.id)))).unwrap_or((false, false));
            let key = Id::unique(("compose-raised", c.id));
            let raised = ctx.data_mut(|d| *d.get_temp_mut_or_insert_with(key, || pass));
            (raised, c.id, m, e)
        })
        .collect();
    all.sort_by_key(|x| std::cmp::Reverse((x.0, x.1)));
    let avail = (screen.width() - 32.0).max(0.0);
    let h = H.min(screen.height() - 32.0).max(160.0);
    let open: Vec<u64> = all.iter().filter(|x| !x.2 && !x.3).map(|x| x.1).collect();
    let mut docked = ((avail + GAP) / (W + GAP)).floor().max(1.0) as usize;
    docked = docked.min(open.len());
    let tabs_wanted = all.iter().filter(|x| !x.3).count() - docked;
    // Leave room for at least one tab (or the More menu) when anything is tabbed.
    while docked > 1 && tabs_wanted > 0 && avail - docked as f32 * (W + GAP) < MORE_W {
        docked -= 1;
    }
    let docked_ids: Vec<u64> = open.iter().take(docked).copied().collect();
    // Docked: keep creation order left-to-right so windows don't shuffle.
    let mut right = screen.right() - 16.0;
    let mut by_age = docked_ids.clone();
    by_age.sort_unstable_by(|a, b| b.cmp(a));
    for id in by_age {
        let w = W.min(avail);
        out.slots.insert(id, (egui::pos2(right - w, screen.bottom() - h - 16.0), egui::vec2(w, h), false));
        right -= w + GAP;
    }
    // Expanded: centred over everything.
    for x in all.iter().filter(|x| x.3) {
        let sz = egui::vec2((screen.width() - 96.0).min(960.0), (screen.height() - 64.0).min(760.0));
        out.slots.insert(x.1, (screen.center() - sz / 2.0, sz, false));
    }
    // Tabs, most recent first, then overflow.
    let tabs: Vec<u64> = all.iter().filter(|x| !x.3 && !docked_ids.contains(&x.1)).map(|x| x.1).collect();
    let room = right - (screen.left() + 16.0);
    let mut slots = ((room + GAP) / (TAB_W + GAP)).floor().max(0.0) as usize;
    if tabs.len() > slots {
        // Make space for the More menu.
        let with_more = ((room - MORE_W) / (TAB_W + GAP)).floor().max(0.0) as usize;
        slots = with_more;
    }
    for (i, id) in tabs.iter().enumerate() {
        if i < slots {
            out.slots.insert(*id, (egui::pos2(right - TAB_W, screen.bottom() - TAB_H), egui::vec2(TAB_W, TAB_H), true));
            right -= TAB_W + GAP;
        } else {
            out.overflow.push(*id);
        }
    }
    let more_w = MORE_W.min((right - screen.left() - 16.0).max(120.0));
    out.more_at = (egui::pos2(right - more_w, screen.bottom() - TAB_H), more_w);
    out
}
