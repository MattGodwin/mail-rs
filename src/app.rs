//! The user interface. Everything here runs on the UI thread and only ever
//! talks to the network through the per-account workers, so the window stays
//! responsive no matter how slow a server is.

use crate::compose::{self, Compose};
use crate::config::{Account, AuthKind, Config, Provider, Security, Server};
use crate::drafts;
use crate::model::{self, FolderIndex, FolderInfo, MsgFlag, Special, Summary};
use crate::store::Store;
use crate::update::{self, Staged};
use crate::view::{self, Block, View};
use crate::watcher::{self, Watcher};
use crate::worker::{self, AcctState, Cmd, Ev, Event, OpErr, OpId, Shared};
use crate::{oauth, secrets};
use eframe::egui::{
    self, text::LayoutJob, text::TextWrapping, Align, Align2, Color32, FontFamily, FontId, Galley, Id, Key as K, Layout, Modifiers, Order,
    Rect, RichText, Sense, Stroke, TextFormat, Vec2,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

const ROW_H: f32 = 52.0;
const BODY_CACHE: usize = 48;
const UNDO_WINDOW: Duration = Duration::from_secs(5);
const PERSIST_AFTER: Duration = Duration::from_millis(1500);
const ACCENT: Color32 = Color32::from_rgb(0xc0, 0x26, 0xd3);
const GOOD: Color32 = Color32::from_rgb(0x10, 0xb9, 0x81);
const ACCOUNT_COLORS: [Color32; 6] = [
    Color32::from_rgb(0x3b, 0x82, 0xf6),
    Color32::from_rgb(0x10, 0xb9, 0x81),
    Color32::from_rgb(0xf5, 0x9e, 0x0b),
    Color32::from_rgb(0xef, 0x44, 0x44),
    Color32::from_rgb(0x8b, 0x5c, 0xf6),
    Color32::from_rgb(0x06, 0xb6, 0xd4),
];
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
    bold: bool,
    quitting: bool,
    force_quit: bool,
    update: UpdateState,
    staged: Option<Staged>,
    started: Instant,
    last_update_check: Option<Instant>,
    restart_requested: bool,
    acts: Vec<Action>,
}

// ---------------------------------------------------------------- setup ---

fn setup_style(ctx: &egui::Context) -> bool {
    // Only Windows adds system fonts; elsewhere the defaults are used as-is.
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut fonts = egui::FontDefinitions::default();
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut bold = false;
    #[cfg(windows)]
    {
        let dir = PathBuf::from(std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into())).join("Fonts");
        let load = |f: &str| std::fs::read(dir.join(f)).ok();
        if let Some(b) = load("segoeui.ttf") {
            fonts.font_data.insert("ui".into(), Arc::new(egui::FontData::from_owned(b)));
            fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "ui".into());
        }
        if let Some(b) = load("segoeuib.ttf") {
            fonts.font_data.insert("ui-bold".into(), Arc::new(egui::FontData::from_owned(b)));
            let mut fam = vec!["ui-bold".to_string()];
            fam.extend(fonts.families[&FontFamily::Proportional].iter().cloned());
            fonts.families.insert(FontFamily::Name("bold".into()), fam);
            bold = true;
        }
        if let Some(b) = load("seguisym.ttf") {
            fonts.font_data.insert("sym".into(), Arc::new(egui::FontData::from_owned(b)));
            for fam in fonts.families.values_mut() {
                fam.push("sym".into());
            }
        }
        if let Some(b) = load("consola.ttf") {
            fonts.font_data.insert("mono".into(), Arc::new(egui::FontData::from_owned(b)));
            fonts.families.entry(FontFamily::Monospace).or_default().insert(0, "mono".into());
        }
    }
    ctx.set_fonts(fonts);
    ctx.all_styles_mut(|s| {
        use egui::TextStyle::*;
        s.text_styles.insert(Body, FontId::proportional(14.0));
        s.text_styles.insert(Button, FontId::proportional(14.0));
        s.text_styles.insert(Heading, FontId::proportional(20.0));
        s.text_styles.insert(Small, FontId::proportional(11.5));
        s.text_styles.insert(Monospace, FontId::monospace(13.0));
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(8.0, 4.0);
        s.spacing.interact_size.y = 24.0;
    });
    bold
}

/// Window icon: an envelope on the app colour, drawn procedurally.
pub fn icon() -> egui::IconData {
    let n = 64usize;
    let mut rgba = vec![0u8; n * n * 4];
    let inside_round = |x: f32, y: f32, r: f32, max: f32| {
        let cx = x.clamp(r, max - r);
        let cy = y.clamp(r, max - r);
        (x - cx).powi(2) + (y - cy).powi(2) <= r * r
    };
    for y in 0..n {
        for x in 0..n {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let i = (y * n + x) * 4;
            if !inside_round(fx, fy, 14.0, 64.0) {
                continue;
            }
            let mut c = [0xc0, 0x26, 0xd3, 0xff];
            let (l, r, t, b) = (14.0, 50.0, 20.0, 44.0);
            if fx >= l && fx <= r && fy >= t && fy <= b {
                let edge = fx - l < 2.5 || r - fx < 2.5 || fy - t < 2.5 || b - fy < 2.5;
                let mid = (l + r) / 2.0;
                let flap_y = t + (mid - (fx - mid).abs() - l) * (12.0 / (mid - l));
                let flap = (fy - flap_y).abs() < 1.8 && fy <= t + 13.0;
                if edge || flap {
                    c = [0xff, 0xff, 0xff, 0xff];
                }
            }
            rgba[i..i + 4].copy_from_slice(&c);
        }
    }
    egui::IconData { rgba, width: n as u32, height: n as u32 }
}

fn one_line_galley(ui: &egui::Ui, text: String, font: FontId, color: Color32, max_width: f32) -> Arc<Galley> {
    let mut job = LayoutJob::single_section(text, TextFormat::simple(font, color));
    job.wrap = TextWrapping { max_width, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    ui.fonts(|f| f.layout_job(job))
}

fn ago(t: SystemTime) -> String {
    let s = SystemTime::now().duration_since(t).unwrap_or_default().as_secs();
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", s / 60),
        3600..=86399 => format!("{} h ago", s / 3600),
        _ => format!("{} d ago", s / 86400),
    }
}

fn paint_row(ui: &mut egui::Ui, r: &Row, selected: bool, stripe: Option<Color32>, bold: bool, acct_label: &str) -> egui::Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, ROW_H), Sense::click());
    // Screen readers: one selectable item per message with everything it shows.
    let label = format!(
        "{}{}{}{}, {}, {}{}",
        if r.s.seen { "" } else { "Unread, " },
        if r.s.flagged { "starred, " } else { "" },
        if r.s.attach { "has attachment, " } else { "" },
        r.s.sender(),
        if r.s.subject.is_empty() { "no subject" } else { &r.s.subject },
        model::fmt_date_long(r.s.date),
        if acct_label.is_empty() { String::new() } else { format!(", {acct_label}") }
    );
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, &label));
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let vis = ui.visuals().clone();
    let painter = ui.painter().clone();
    let card = rect.shrink2(egui::vec2(4.0, 1.5));
    if selected {
        painter.rect_filled(card, 6.0, vis.selection.bg_fill);
    } else if resp.hovered() {
        painter.rect_filled(card, 6.0, vis.widgets.hovered.weak_bg_fill);
    }
    if resp.has_focus() {
        painter.rect_stroke(card, 6.0, vis.selection.stroke, egui::StrokeKind::Inside);
    }
    let text = if selected { vis.selection.stroke.color } else { vis.text_color() };
    let strong = if selected { text } else { vis.strong_text_color() };
    let weak = if selected { text } else { vis.weak_text_color() };
    if let Some(c) = stripe {
        painter.rect_filled(Rect::from_min_size(rect.min + egui::vec2(5.0, 9.0), egui::vec2(3.0, ROW_H - 18.0)), 1.5, c);
    }
    let unread = !r.s.seen;
    if unread {
        painter.circle_filled(egui::pos2(rect.left() + 15.0, rect.top() + 17.0), 3.5, ACCENT);
    }
    let left = rect.left() + 24.0;
    let right = rect.right() - 12.0;
    let date = one_line_galley(ui, model::fmt_date_short(r.s.date), FontId::proportional(12.0), weak, 200.0);
    let date_x = right - date.size().x;
    painter.galley(egui::pos2(date_x, rect.top() + 9.0), date, weak);
    let mut icons = String::new();
    if r.s.answered {
        icons.push('↩');
    }
    if r.s.attach {
        icons.push('📎');
    }
    if r.s.flagged {
        icons.push('★');
    }
    let icon_g =
        one_line_galley(ui, icons, FontId::proportional(12.0), if r.s.flagged { Color32::from_rgb(0xf5, 0x9e, 0x0b) } else { weak }, 200.0);
    let icon_x = date_x - icon_g.size().x - 6.0;
    painter.galley(egui::pos2(icon_x, rect.top() + 9.0), icon_g, weak);
    let from_font = if unread && bold { FontId::new(14.0, FontFamily::Name("bold".into())) } else { FontId::proportional(14.0) };
    let sender = if r.s.sender().is_empty() { "(unknown sender)".to_string() } else { r.s.sender().to_string() };
    let from = one_line_galley(ui, sender, from_font, if unread { strong } else { text }, (icon_x - left - 8.0).max(10.0));
    painter.galley(egui::pos2(left, rect.top() + 7.0), from, text);
    let subject = if r.s.subject.is_empty() { "(no subject)".to_string() } else { r.s.subject.clone() };
    let subj = one_line_galley(ui, subject, FontId::proportional(13.0), if unread { text } else { weak }, (right - left).max(10.0));
    painter.galley(egui::pos2(left, rect.top() + 28.0), subj, text);
    painter.hline(
        (rect.left() + 12.0)..=(rect.right() - 12.0),
        rect.bottom() - 0.5,
        Stroke::new(0.5_f32, vis.widgets.noninteractive.bg_stroke.color),
    );
    resp
}

fn compose_layer(id: u64) -> egui::LayerId {
    egui::LayerId::new(Order::Middle, Id::new(("compose", id)))
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
        let bold = setup_style(&cc.egui_ctx);
        let cfg = Config::load();
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
            bold,
            quitting: false,
            force_quit: false,
            update: UpdateState::Idle,
            staged: None,
            started: Instant::now(),
            last_update_check: None,
            restart_requested: false,
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
                Ev::OAuth { editor, result } => {
                    if let Some(ed) = self.editor.as_mut().filter(|e| e.id == editor) {
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
                            self.view = Some(view::parse(raw));
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
                Ev::Done { .. } | Ev::Sent { .. } | Ev::OAuth { .. } | Ev::Update(_) => {}
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
        self.view = None;
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
                self.view = Some(view::parse(raw));
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
            self.view = None;
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
        self.view = None;
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
        let Some(v) = &self.view else { return };
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
        let Some(v) = &self.view else { return };
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
        let Some(v) = &self.view else { return };
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
        let Some(v) = &self.view else { return };
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

    fn ui_update_banner(&mut self, ctx: &egui::Context) {
        let Some(s) = &self.staged else { return };
        let (version, url, mode) = (s.version.to_string(), s.url.clone(), s.mode);
        egui::TopBottomPanel::top("update-banner")
            .frame(egui::Frame::side_top_panel(&ctx.style()).inner_margin(egui::Margin::symmetric(12, 5)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("✨").color(ACCENT));
                    let text = match mode {
                        update::Mode::Installer => {
                            format!("Mail {version} is ready to install. It installs automatically when you close Mail.")
                        }
                        update::Mode::Portable => format!("Mail {version} has been downloaded. It starts the next time you open Mail."),
                    };
                    ui.label(text);
                    if ui.add(egui::Button::new(RichText::new("Restart now").color(Color32::WHITE)).fill(ACCENT)).clicked() {
                        self.restart_requested = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    ui.hyperlink_to("What's new", url);
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
                if let Some(v) = &self.view {
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
        }
    }

    // ---------------------------------------------------- account editor ---

    fn open_editor(&mut self, editing: Option<usize>) {
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
        let id = existing.as_ref().map(|a| a.id.clone()).unwrap_or_else(crate::config::new_account_id);
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

    fn ui_top(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top")
            .frame(egui::Frame::side_top_panel(&ctx.style()).inner_margin(egui::Margin::symmetric(10, 7)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .add(egui::Button::new(RichText::new("✏  Compose").color(Color32::WHITE)).fill(ACCENT))
                        .on_hover_text("New message (C)")
                        .clicked()
                    {
                        self.acts.push(Action::Compose);
                    }
                    if ui.button("⟳").on_hover_text("Check for new mail (F5)").clicked() {
                        self.acts.push(Action::Refresh);
                    }
                    ui.add_space(8.0);
                    let scope = match &self.sel {
                        Sel::Folder(i, f) => self.accts[*i].folder_display(f),
                        _ => "all inboxes".to_string(),
                    };
                    let edit = egui::TextEdit::singleline(&mut self.filter)
                        .hint_text(format!("🔍  Filter · Enter searches {scope} on the server  ( / )"))
                        .desired_width(360.0);
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
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("⚙").on_hover_text("Settings").clicked() {
                            self.settings_open = true;
                        }
                        if ui.button("?").on_hover_text("Keyboard shortcuts (?)").clicked() {
                            self.shortcuts_open = true;
                        }
                        if ui.button("+ Account").clicked() {
                            self.acts.push(Action::AddAccount);
                        }
                        if self.accts.iter().any(|a| a.busy) {
                            ui.spinner();
                        }
                    });
                });
            });
    }

    fn ui_status(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("status")
            .frame(egui::Frame::side_top_panel(&ctx.style()).inner_margin(egui::Margin::symmetric(10, 3)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let text = if self.status.is_empty() { "Ready".to_string() } else { self.status.clone() };
                    ui.add(egui::Label::new(RichText::new(text).small().weak()).truncate());
                });
            });
    }

    fn ui_toast(&mut self, ctx: &egui::Context) {
        let Some(p) = self.pending.last() else { return };
        let label = p.label.clone();
        let left = p.deadline.saturating_duration_since(Instant::now()).as_secs() + 1;
        egui::Area::new(Id::new("undo-toast")).order(Order::Foreground).anchor(Align2::CENTER_BOTTOM, egui::vec2(0.0, -34.0)).show(
            ctx,
            |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(&label);
                        if ui.button(format!("Undo ({left})")).on_hover_text("Ctrl+Z").clicked() {
                            self.acts.push(Action::Undo);
                        }
                    });
                });
            },
        );
    }

    fn ui_account_status(ui: &mut egui::Ui, i: usize, a: &Acct, acts: &mut Vec<Action>) {
        let small = |t: String| RichText::new(t).small();
        ui.horizontal_wrapped(|ui| {
            if a.cfg.paused {
                ui.label(small("⏸ Paused".into()).weak());
                if ui.small_button("Resume").clicked() {
                    acts.push(Action::Pause(i, false));
                }
                return;
            }
            match &a.state {
                AcctState::Online => {
                    let when = a.last_sync.map(ago).unwrap_or_else(|| "syncing…".into());
                    let push = if a.push { " · push" } else { "" };
                    ui.label(small(format!("● {when}{push}")).color(GOOD)).on_hover_text(if a.push {
                        "Connected. New mail arrives instantly (IMAP IDLE)."
                    } else {
                        "Connected. Checking for new mail periodically."
                    });
                }
                AcctState::Connecting => {
                    ui.label(small("Connecting…".into()).weak());
                }
                AcctState::Offline(e) => {
                    ui.label(small("⚠ Offline".into()).color(ui.visuals().warn_fg_color)).on_hover_text(e);
                    if ui.small_button("Retry").clicked() {
                        acts.push(Action::Retry(i));
                    }
                }
                AcctState::AuthFailed(e) => {
                    ui.label(small("⚠ Sign-in failed".into()).color(ui.visuals().error_fg_color)).on_hover_text(e);
                    if ui.small_button("Sign in again").clicked() {
                        acts.push(Action::EditAccount(i));
                    }
                    if ui.small_button("Retry").clicked() {
                        acts.push(Action::Retry(i));
                    }
                }
            }
        });
    }

    fn ui_folders(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("folders").resizable(true).default_width(220.0).width_range(160.0..=360.0).show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.add_space(6.0);
                ui.with_layout(Layout::top_down_justified(Align::LEFT), |ui| {
                    let total: usize = self.accts.iter().filter(|a| a.live()).map(|a| a.unread(&a.inbox())).sum();
                    let label = if total > 0 {
                        RichText::new(format!("📥  All inboxes   {total}")).strong()
                    } else {
                        RichText::new("📥  All inboxes")
                    };
                    if ui.selectable_label(self.sel == Sel::Unified, label).clicked() {
                        self.acts.push(Action::Select(Sel::Unified));
                    }
                    if let Sel::Search(q) = &self.sel {
                        let _ = ui.selectable_label(true, format!("🔍  \"{q}\""));
                    }
                    ui.add_space(6.0);
                    let mut acts = Vec::new();
                    for (i, a) in self.accts.iter().enumerate().filter(|(_, a)| a.live()) {
                        let color = ACCOUNT_COLORS[i % ACCOUNT_COLORS.len()];
                        let head = egui::CollapsingHeader::new(RichText::new(format!("●  {}", a.cfg.label())).color(color).strong())
                            .id_salt(("acct", &a.cfg.id))
                            .default_open(true)
                            .show(ui, |ui| {
                                Self::ui_account_status(ui, i, a, &mut acts);
                                if a.folders.is_empty() {
                                    ui.label(RichText::new("No folders yet").weak().small());
                                }
                                for f in &a.folders {
                                    let indent = "      ".repeat(f.depth);
                                    let icon = f.special.map(|s| s.icon()).unwrap_or("📁");
                                    let unread = a.unread(&f.name);
                                    let show_unread = unread > 0
                                        && !matches!(
                                            f.special,
                                            Some(Special::Trash | Special::Junk | Special::Sent | Special::All | Special::Drafts)
                                        );
                                    let text = if show_unread {
                                        RichText::new(format!("{indent}{icon}  {}   {unread}", f.display)).strong()
                                    } else {
                                        RichText::new(format!("{indent}{icon}  {}", f.display))
                                    };
                                    if !f.selectable {
                                        ui.label(text.weak());
                                        continue;
                                    }
                                    let selected = self.sel == Sel::Folder(i, f.name.clone());
                                    if ui.selectable_label(selected, text).clicked() {
                                        acts.push(Action::Select(Sel::Folder(i, f.name.clone())));
                                    }
                                }
                            });
                        let paused = a.cfg.paused;
                        head.header_response.context_menu(|ui| {
                            if ui.button("Edit account…").clicked() {
                                acts.push(Action::EditAccount(i));
                                ui.close_menu();
                            }
                            if ui.button(if paused { "Resume" } else { "Pause" }).clicked() {
                                acts.push(Action::Pause(i, !paused));
                                ui.close_menu();
                            }
                            if ui.button("Remove account…").clicked() {
                                acts.push(Action::RemoveAccount(i));
                                ui.close_menu();
                            }
                        });
                    }
                    self.acts.extend(acts);
                });
            });
        });
    }

    fn ui_list(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("list")
            .resizable(true)
            .default_width(380.0)
            .width_range(260.0..=700.0)
            .frame(egui::Frame::side_top_panel(&ctx.style()).inner_margin(egui::Margin::symmetric(0, 6)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    let (title, more) = match &self.sel {
                        Sel::Unified => (
                            "All inboxes".to_string(),
                            self.accts
                                .iter()
                                .filter(|a| a.live())
                                .any(|a| a.idx.get(&a.inbox()).is_some_and(|i| (i.msgs.len() as u32) < i.exists)),
                        ),
                        Sel::Folder(i, f) => {
                            let a = &self.accts[*i];
                            (a.folder_display(f), a.idx.get(f).is_some_and(|x| (x.msgs.len() as u32) < x.exists))
                        }
                        Sel::Search(q) => (format!("Results for \"{q}\""), false),
                    };
                    ui.label(RichText::new(title).strong().size(16.0));
                    ui.label(RichText::new(format!("{}", self.rows.len())).weak());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add_space(10.0);
                        if let Sel::Search(_) = &self.sel {
                            if ui.small_button("✖ Close").on_hover_text("Esc in the search box").clicked() {
                                self.acts.push(Action::Select(Sel::Unified));
                            }
                            if self.accts.iter().any(|a| a.search.as_ref().is_some_and(|s| s.loading)) {
                                ui.spinner();
                            }
                        } else if more && ui.small_button("Load older").clicked() {
                            self.acts.push(Action::LoadMore);
                        }
                    });
                });
                if let Sel::Search(_) = &self.sel {
                    let scopes: Vec<String> = self
                        .accts
                        .iter()
                        .filter_map(|a| a.search.as_ref().map(|s| (a, s)))
                        .map(|(a, s)| {
                            let trunc = if s.total > s.msgs.len() {
                                format!(" · newest {} of {} matches", s.msgs.len(), s.total)
                            } else {
                                String::new()
                            };
                            format!("{} / {}{trunc}", a.cfg.label(), a.folder_display(&s.folder))
                        })
                        .collect();
                    ui.horizontal(|ui| {
                        ui.add_space(12.0);
                        ui.label(RichText::new(format!("In {}", scopes.join("; "))).small().weak());
                    });
                }
                ui.add_space(4.0);
                ui.separator();

                if self.rows.is_empty() {
                    ui.add_space(30.0);
                    ui.vertical_centered(|ui| {
                        let busy = self.accts.iter().any(|a| a.busy);
                        ui.label(
                            RichText::new(if self.live_count() == 0 {
                                "No accounts yet"
                            } else if busy {
                                "Loading…"
                            } else if !self.filter.is_empty() && !matches!(self.sel, Sel::Search(_)) {
                                "Nothing matches the filter. Press Enter to search the server."
                            } else {
                                "No messages"
                            })
                            .weak(),
                        );
                    });
                    return;
                }

                ui.spacing_mut().item_spacing.y = 0.0;
                let mut sa = egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("list");
                if self.scroll_to_cur {
                    self.scroll_to_cur = false;
                    if let Some(p) = self.cur.as_ref().and_then(|c| self.rows.iter().position(|r| &r.key == c)) {
                        let y = p as f32 * ROW_H;
                        let (off, h) = self.list_scroll;
                        if y < off {
                            sa = sa.vertical_scroll_offset(y);
                        } else if y + ROW_H > off + h {
                            sa = sa.vertical_scroll_offset(y + ROW_H - h);
                        }
                    }
                }
                let multi = self.live_count() > 1;
                let mut acts = Vec::new();
                let out = sa.show_rows(ui, ROW_H, self.rows.len(), |ui, range| {
                    for i in range {
                        let r = &self.rows[i];
                        let selected = self.cur.as_ref() == Some(&r.key);
                        let stripe = multi.then(|| ACCOUNT_COLORS[r.key.acct % ACCOUNT_COLORS.len()]);
                        let a = &self.accts[r.key.acct];
                        let resp = paint_row(ui, r, selected, stripe, self.bold, if multi { a.cfg.label() } else { "" });
                        if resp.clicked() {
                            acts.push(Action::Open(r.key.clone()));
                        }
                        let key = r.key.clone();
                        let (seen, flagged) = (r.s.seen, r.s.flagged);
                        resp.context_menu(|ui| {
                            let keys = vec![key.clone()];
                            if ui.button(if seen { "Mark as unread" } else { "Mark as read" }).clicked() {
                                acts.push(Action::Flag(keys.clone(), MsgFlag::Seen, !seen));
                                ui.close_menu();
                            }
                            if ui.button(if flagged { "Unstar" } else { "Star" }).clicked() {
                                acts.push(Action::Flag(keys.clone(), MsgFlag::Flagged, !flagged));
                                ui.close_menu();
                            }
                            if ui.button("Archive").clicked() {
                                acts.push(Action::Archive(keys.clone()));
                                ui.close_menu();
                            }
                            if ui.button("Delete").clicked() {
                                acts.push(Action::Trash(keys.clone()));
                                ui.close_menu();
                            }
                            ui.menu_button("Move to", |ui| {
                                egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                                    for f in a.folders.iter().filter(|f| f.selectable && f.name != key.folder) {
                                        if ui.button(&f.display).clicked() {
                                            acts.push(Action::MoveTo(keys.clone(), f.name.clone()));
                                            ui.close_menu();
                                        }
                                    }
                                });
                            });
                        });
                    }
                });
                self.acts.extend(acts);
                self.list_scroll = (out.state.offset.y, out.inner_rect.height());
            });
    }

    fn ui_reader(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().frame(egui::Frame::central_panel(&ctx.style()).inner_margin(egui::Margin::symmetric(20, 12))).show(
            ctx,
            |ui| {
                if self.live_count() == 0 {
                    ui.vertical_centered(|ui| {
                        ui.add_space(ui.available_height() * 0.3);
                        ui.heading("Welcome to Mail");
                        ui.label(RichText::new("Fast, minimal, and it never touches your messages.").weak());
                        ui.add_space(12.0);
                        if ui.button("Add an account").clicked() {
                            self.acts.push(Action::AddAccount);
                        }
                    });
                    return;
                }
                let Some(key) = self.cur.clone() else {
                    ui.centered_and_justified(|ui| ui.label(RichText::new("No message selected").weak()));
                    return;
                };
                if self.loading.is_some() {
                    ui.centered_and_justified(|ui| ui.spinner());
                    return;
                }
                if let Some(e) = &self.view_error {
                    ui.centered_and_justified(|ui| {
                        ui.label(RichText::new(format!("Could not load message: {e}")).color(ui.visuals().error_fg_color))
                    });
                    return;
                }
                let Some(v) = &self.view else { return };
                let s = self.rows.iter().find(|r| r.key == key).map(|r| r.s.clone()).unwrap_or_default();
                let in_drafts = self.accts[key.acct].folder_special(&key.folder) == Some(Special::Drafts);
                let keys = vec![key.clone()];

                ui.horizontal_wrapped(|ui| {
                    if in_drafts {
                        if ui.button("✏ Edit draft").clicked() {
                            self.acts.push(Action::EditAsNew(true));
                        }
                        ui.separator();
                    }
                    if ui.button("↩ Reply").on_hover_text("R").clicked() {
                        self.acts.push(Action::Reply(false));
                    }
                    if ui.button("↩ Reply all").on_hover_text("A").clicked() {
                        self.acts.push(Action::Reply(true));
                    }
                    if ui.button("↪ Forward").on_hover_text("F").clicked() {
                        self.acts.push(Action::Forward);
                    }
                    ui.separator();
                    if ui.button("🗄").on_hover_text("Archive (E)").clicked() {
                        self.acts.push(Action::Archive(keys.clone()));
                    }
                    if ui.button("🗑").on_hover_text("Delete (Del)").clicked() {
                        self.acts.push(Action::Trash(keys.clone()));
                    }
                    if ui.button(if s.flagged { "★" } else { "☆" }).on_hover_text("Star (S)").clicked() {
                        self.acts.push(Action::Flag(keys.clone(), MsgFlag::Flagged, !s.flagged));
                    }
                    if ui
                        .button(if s.seen { "✉" } else { "✉•" })
                        .on_hover_text(if s.seen { "Mark unread (U)" } else { "Mark read (U)" })
                        .clicked()
                    {
                        self.acts.push(Action::Flag(keys.clone(), MsgFlag::Seen, !s.seen));
                    }
                    ui.menu_button("⋯", |ui| {
                        if ui.button("View source").clicked() {
                            self.acts.push(Action::ViewSource);
                            ui.close_menu();
                        }
                        if ui.button("Save as .eml (original bytes)").clicked() {
                            self.acts.push(Action::SaveEml);
                            ui.close_menu();
                        }
                        if v.html.is_some()
                            && ui
                                .button("Open HTML in browser")
                                .on_hover_text("Remote images and trackers may load in the browser")
                                .clicked()
                        {
                            self.acts.push(Action::OpenHtml);
                            ui.close_menu();
                        }
                        if ui.button("Edit as new message").clicked() {
                            self.acts.push(Action::EditAsNew(false));
                            ui.close_menu();
                        }
                        ui.checkbox(&mut self.mono, "Monospace body");
                    });
                });
                ui.add_space(8.0);

                // Headers and attachments get a bounded area so long recipient
                // lists or many attachments never push the body off screen.
                let max_head = (ui.available_height() * 0.4).max(120.0);
                egui::ScrollArea::vertical()
                    .id_salt(("head", key.acct, &key.folder, key.uid))
                    .max_height(max_head)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(&v.subject).size(20.0).strong()).wrap());
                        ui.add_space(4.0);
                        egui::Grid::new("hdrs").num_columns(2).spacing([10.0, 3.0]).show(ui, |ui| {
                            let row = |ui: &mut egui::Ui, k: &str, val: &str| {
                                if !val.is_empty() {
                                    ui.label(RichText::new(k).weak());
                                    ui.add(egui::Label::new(val).wrap());
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
                        if !v.attachments.is_empty() {
                            ui.add_space(6.0);
                            ui.horizontal_wrapped(|ui| {
                                for (i, a) in v.attachments.iter().enumerate() {
                                    let r = ui
                                        .button(format!("📎 {}  ·  {}", a.name, model::fmt_size(a.size)))
                                        .on_hover_text(format!("{}\nClick to save · right-click for more", a.mime));
                                    if r.clicked() {
                                        self.acts.push(Action::SaveAttachment(i));
                                    }
                                    r.context_menu(|ui| {
                                        if ui.button("Open").clicked() {
                                            self.acts.push(Action::OpenAttachment(i));
                                            ui.close_menu();
                                        }
                                        if ui.button("Save as…").clicked() {
                                            self.acts.push(Action::SaveAttachment(i));
                                            ui.close_menu();
                                        }
                                    });
                                }
                            });
                        }
                        if v.from_html {
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("HTML message shown as text. Remote content is never loaded.").small().weak());
                                if ui.small_button("Open original in browser").clicked() {
                                    self.acts.push(Action::OpenHtml);
                                }
                            });
                        }
                    });
                ui.add_space(6.0);
                ui.separator();

                let mono = self.mono;
                egui::ScrollArea::vertical().id_salt(("body", key.acct, &key.folder, key.uid)).auto_shrink([false, false]).show(ui, |ui| {
                    ui.add_space(6.0);
                    ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
                    let font = if mono { FontId::monospace(13.0) } else { FontId::proportional(14.5) };
                    for b in &v.blocks {
                        match b {
                            Block::Text(t) => {
                                ui.add(egui::Label::new(RichText::new(t).font(font.clone())).wrap().selectable(true));
                            }
                            Block::Line(segs) => {
                                ui.horizontal_wrapped(|ui| {
                                    for (link, s) in segs {
                                        let rt = RichText::new(s).font(font.clone());
                                        if !*link {
                                            ui.add(egui::Label::new(rt).wrap().selectable(true));
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
                    ui.add_space(24.0);
                });
            },
        );
    }

    /// Which compose window holds keyboard focus, if any.
    fn focused_compose(&self, ctx: &egui::Context) -> Option<u64> {
        let layer = ctx.memory(|m| m.focused()).and_then(|id| ctx.read_response(id)).map(|r| r.layer_id)?;
        self.composes.iter().find(|c| compose_layer(c.id) == layer).map(|c| c.id)
    }

    fn ui_composes(&mut self, ctx: &egui::Context) {
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
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        let hovered_drop = ctx.input(|i| i.pointer.hover_pos());
        let mut acts = Vec::new();
        let mut drop_target: Option<u64> = None;
        for c in self.composes.iter_mut() {
            let mut open = true;
            let id = c.id;
            let locked = c.sending || c.saving.is_some();
            let resp = egui::Window::new(c.title())
                .id(Id::new(("compose", id)))
                .open(&mut open)
                .default_size([640.0, 520.0])
                .min_width(420.0)
                .resizable(true)
                .collapsible(true)
                .show(ctx, |ui| {
                    ui.add_enabled_ui(!locked, |ui| {
                        egui::Grid::new(("cgrid", id)).num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                            ui.label("From");
                            ui.horizontal(|ui| {
                                let current = accts
                                    .iter()
                                    .find(|(i, _, _)| *i == c.acct)
                                    .and_then(|(_, _, ids)| ids.get(c.from).or(ids.first()).cloned())
                                    .unwrap_or_else(|| "Choose an account".into());
                                egui::ComboBox::from_id_salt(("from", id))
                                    .selected_text(current)
                                    .width(ui.available_width() - 70.0)
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
                                if !c.show_cc && ui.small_button("Cc/Bcc").clicked() {
                                    c.show_cc = true;
                                }
                            });
                            ui.end_row();
                            let field = |ui: &mut egui::Ui, label: &str, val: &mut String, hint: &str, focus: bool| {
                                ui.label(label);
                                let r = ui.add(egui::TextEdit::singleline(val).hint_text(hint).desired_width(f32::INFINITY));
                                if focus {
                                    r.request_focus();
                                }
                                ui.end_row();
                            };
                            let focus_to = c.focus && c.to.is_empty();
                            field(ui, "To", &mut c.to, "name@example.com, …", focus_to);
                            if c.show_cc {
                                field(ui, "Cc", &mut c.cc, "", false);
                                field(ui, "Bcc", &mut c.bcc, "", false);
                            }
                            field(ui, "Subject", &mut c.subject, "", false);
                        });
                        let focus_body = c.focus && !c.to.is_empty();
                        c.focus = false;

                        if !c.files.is_empty() || !c.carried.is_empty() {
                            ui.horizontal_wrapped(|ui| {
                                let mut remove_file = None;
                                for (i, p) in c.files.iter().enumerate() {
                                    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                                    let size = std::fs::metadata(p).map(|m| m.len() as usize).unwrap_or(0);
                                    if ui
                                        .button(format!("📎 {name} · {}  ✖", model::fmt_size(size)))
                                        .on_hover_text(p.display().to_string())
                                        .clicked()
                                    {
                                        remove_file = Some(i);
                                    }
                                }
                                if let Some(i) = remove_file {
                                    c.files.remove(i);
                                }
                                let mut remove_carried = None;
                                for (i, (name, _, data)) in c.carried.iter().enumerate() {
                                    if ui.button(format!("📎 {name} · {}  ✖", model::fmt_size(data.len()))).clicked() {
                                        remove_carried = Some(i);
                                    }
                                }
                                if let Some(i) = remove_carried {
                                    c.carried.remove(i);
                                    c.carried_dirty = true;
                                }
                            });
                        }

                        let bottom_h = 40.0 + if c.error.is_some() || c.note.is_some() { 40.0 } else { 0.0 };
                        egui::ScrollArea::vertical().id_salt(("cbody", id)).max_height((ui.available_height() - bottom_h).max(120.0)).show(
                            ui,
                            |ui| {
                                let r = ui.add(
                                    egui::TextEdit::multiline(&mut c.body)
                                        .desired_width(f32::INFINITY)
                                        .desired_rows(16)
                                        .font(egui::TextStyle::Body),
                                );
                                if focus_body {
                                    r.request_focus();
                                    if let Some(mut st) = egui::TextEdit::load_state(ui.ctx(), r.id) {
                                        st.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(0))));
                                        st.store(ui.ctx(), r.id);
                                    }
                                }
                            },
                        );
                    });

                    if let Some(e) = &c.error {
                        ui.add(egui::Label::new(RichText::new(e).color(ui.visuals().error_fg_color)).wrap());
                    } else if let Some(n) = &c.note {
                        ui.label(RichText::new(n).small().weak());
                    }
                    ui.horizontal(|ui| {
                        let send = ui
                            .add_enabled(!locked, egui::Button::new(RichText::new("Send").color(Color32::WHITE)).fill(ACCENT))
                            .on_hover_text("Ctrl+Enter");
                        if send.clicked() {
                            acts.push(Action::Send(id));
                        }
                        if c.sending {
                            ui.spinner();
                            ui.label(RichText::new("Sending… (closing the app waits for this)").weak());
                        } else if c.saving.is_some() {
                            ui.spinner();
                        }
                        ui.add_enabled_ui(!locked, |ui| {
                            if ui.button("📎 Attach…").clicked() {
                                acts.push(Action::AttachFiles(id));
                            }
                            if ui.button("Save draft").clicked() {
                                acts.push(Action::SaveDraft(id, false));
                            }
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.add_enabled(!locked, egui::Button::new("Discard")).clicked() {
                                acts.push(Action::RequestClose(id));
                            }
                        });
                    });
                });
            if !open {
                acts.push(Action::RequestClose(id));
            }
            if let (Some(r), Some(p)) = (&resp, hovered_drop) {
                if r.response.rect.contains(p) && !locked {
                    drop_target = Some(id);
                }
            }
        }
        // Files dropped on a compose window (or anywhere, if one is open) attach to it.
        if !dropped.is_empty() {
            let target = drop_target.or_else(|| self.composes.iter().rev().find(|c| !c.sending && c.saving.is_none()).map(|c| c.id));
            match target {
                Some(t) => {
                    if let Some(c) = self.composes.iter_mut().find(|c| c.id == t) {
                        c.files.extend(dropped);
                    }
                }
                None => {
                    let a = self.current_acct();
                    if let Some(c) = self.new_compose(a) {
                        c.files.extend(dropped);
                    }
                }
            }
        }
        self.acts.extend(acts);
    }

    fn ui_editor(&mut self, ctx: &egui::Context) {
        let Some(ed) = self.editor.as_mut() else { return };
        let settings = self.cfg.settings.clone();
        let mut open = true;
        let mut save = false;
        let mut cancel = false;
        let title = if ed.editing.is_some() { "Edit account" } else { "Add account" };
        egui::Window::new(title)
            .id(Id::new("account-editor"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(500.0)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().max_height(ctx.screen_rect().height() - 140.0).show(ui, |ui| {
                    egui::Grid::new("acct-grid").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                        ui.label("Provider");
                        let mut p = ed.provider;
                        egui::ComboBox::from_id_salt("provider").selected_text(p.label()).width(320.0).show_ui(ui, |ui| {
                            for opt in Provider::ALL {
                                ui.selectable_value(&mut p, opt, opt.label());
                            }
                        });
                        if p != ed.provider {
                            Self::apply_preset(ed, p, &settings);
                        }
                        ui.end_row();

                        ui.label("Your name");
                        ui.add(egui::TextEdit::singleline(&mut ed.name).hint_text("Jane Doe").desired_width(320.0));
                        ui.end_row();
                        ui.label("Email");
                        let r = ui.add(egui::TextEdit::singleline(&mut ed.email).hint_text("jane@example.com").desired_width(320.0));
                        if r.changed() {
                            if !ed.username_touched {
                                ed.username = ed.email.trim().to_string();
                            }
                            if let Some(g) = Provider::guess(&ed.email) {
                                if g != ed.provider && ed.editing.is_none() {
                                    Self::apply_preset(ed, g, &settings);
                                }
                            }
                        }
                        ui.end_row();

                        ui.label("Sign-in");
                        let mut auth = ed.auth;
                        let name = |a: AuthKind| match a {
                            AuthKind::Password => "Password / app password",
                            AuthKind::OAuthGoogle => "Google (OAuth)",
                            AuthKind::OAuthMicrosoft => "Microsoft (OAuth)",
                        };
                        egui::ComboBox::from_id_salt("auth").selected_text(name(auth)).width(320.0).show_ui(ui, |ui| {
                            for a in [AuthKind::Password, AuthKind::OAuthGoogle, AuthKind::OAuthMicrosoft] {
                                ui.selectable_value(&mut auth, a, name(a));
                            }
                        });
                        if auth != ed.auth {
                            ed.auth = auth;
                            ed.oauth = OAuthState::Idle;
                            Self::default_client(ed, &settings);
                        }
                        ui.end_row();
                    });

                    // Setup guidance for this provider and sign-in method.
                    let (text, links) = ed.provider.guide(ed.auth);
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.add(egui::Label::new(RichText::new(text).small()).wrap());
                        for (label, url) in links.iter() {
                            ui.hyperlink_to(RichText::new(format!("↗ {label}")).small(), *url);
                        }
                    });
                    ui.add_space(4.0);

                    egui::Grid::new("acct-grid2").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| match ed.auth {
                        AuthKind::Password => {
                            ui.label("Password");
                            ui.add(
                                egui::TextEdit::singleline(&mut ed.password)
                                    .password(true)
                                    .hint_text(if ed.editing.is_some() { "unchanged" } else { "app password" })
                                    .desired_width(320.0),
                            );
                            ui.end_row();
                        }
                        _ => {
                            ui.label("Client ID");
                            ui.add(egui::TextEdit::singleline(&mut ed.client_id).desired_width(320.0));
                            ui.end_row();
                            if ed.auth == AuthKind::OAuthGoogle {
                                ui.label("Client secret");
                                ui.add(egui::TextEdit::singleline(&mut ed.client_secret).password(true).desired_width(320.0));
                                ui.end_row();
                            }
                            ui.label("");
                            ui.horizontal_wrapped(|ui| {
                                let waiting = matches!(ed.oauth, OAuthState::Waiting);
                                let valid = oauth::validate_client_id(ed.auth, &ed.client_id);
                                let btn = ui.add_enabled(!waiting && valid.is_ok(), egui::Button::new("Sign in with browser"));
                                if let Err(e) = &valid {
                                    btn.on_disabled_hover_text(e.as_str());
                                    if !ed.client_id.trim().is_empty() {
                                        ui.label(RichText::new(e).small().color(ui.visuals().warn_fg_color));
                                    }
                                } else if btn.clicked() {
                                    ed.oauth = OAuthState::Waiting;
                                    let (kind, cid, secret, hint) = (ed.auth, ed.client_id.trim().to_string(), ed.client_secret.trim().to_string(), ed.email.trim().to_string());
                                    let (tx, ctx2, eid) = (self.tx.clone(), ctx.clone(), ed.id);
                                    std::thread::spawn(move || {
                                        let result = oauth::authorize(kind, &cid, &secret, &hint);
                                        let _ = tx.send(Event { acct: String::new(), gen: 0, kind: Ev::OAuth { editor: eid, result } });
                                        ctx2.request_repaint();
                                    });
                                }
                                match &ed.oauth {
                                    OAuthState::Idle => {}
                                    OAuthState::Waiting => {
                                        ui.spinner();
                                        ui.label(RichText::new("Waiting for browser…").weak());
                                    }
                                    OAuthState::Done(_) => {
                                        ui.label(RichText::new("✔ Signed in").color(GOOD));
                                    }
                                    OAuthState::Failed(e) => {
                                        ui.add(egui::Label::new(RichText::new(e).small().color(ui.visuals().error_fg_color)).wrap());
                                    }
                                }
                            });
                            ui.end_row();
                        }
                    });

                    ui.add_space(4.0);
                    egui::CollapsingHeader::new("Server settings").default_open(ed.provider == Provider::Custom).show(ui, |ui| {
                        egui::Grid::new("srv-grid").num_columns(4).spacing([8.0, 6.0]).show(ui, |ui| {
                            for (label, srv) in [("IMAP", &mut ed.imap), ("SMTP", &mut ed.smtp)] {
                                ui.label(label);
                                ui.add(egui::TextEdit::singleline(&mut srv.host).hint_text("host").desired_width(200.0));
                                ui.add(egui::DragValue::new(&mut srv.port).range(1..=65535));
                                egui::ComboBox::from_id_salt(("sec", label))
                                    .selected_text(match srv.security {
                                        Security::Tls => "SSL/TLS",
                                        Security::StartTls => "STARTTLS",
                                    })
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(&mut srv.security, Security::Tls, "SSL/TLS");
                                        ui.selectable_value(&mut srv.security, Security::StartTls, "STARTTLS");
                                    });
                                ui.end_row();
                            }
                            ui.label("User");
                            if ui.add(egui::TextEdit::singleline(&mut ed.username).desired_width(200.0)).changed() {
                                ed.username_touched = true;
                            }
                            ui.end_row();
                        });
                        ui.checkbox(&mut ed.save_sent, "Save a copy of sent mail to the Sent folder").on_hover_text("Gmail and Outlook do this automatically; leave off for them.");
                    });
                    egui::CollapsingHeader::new("Sending identities (aliases)").show(ui, |ui| {
                        ui.label(RichText::new("Other addresses this mailbox may send as, one per line, e.g. \"Jane Doe <jane@work.example>\". Your provider must allow sending as them.").small().weak());
                        ui.add(egui::TextEdit::multiline(&mut ed.aliases).desired_rows(2).desired_width(f32::INFINITY));
                    });
                    egui::CollapsingHeader::new("Signature & notifications").show(ui, |ui| {
                        ui.add(egui::TextEdit::multiline(&mut ed.signature).hint_text("Signature").desired_rows(3).desired_width(f32::INFINITY));
                        ui.checkbox(&mut ed.notify, "Notify me about new mail in this account");
                    });

                    if let Some(e) = &ed.error {
                        ui.add(egui::Label::new(RichText::new(e).color(ui.visuals().error_fg_color)).wrap());
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.add(egui::Button::new(RichText::new("Save").color(Color32::WHITE)).fill(ACCENT)).clicked() {
                            save = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                    });
                });
            });
        if save {
            match self.save_editor() {
                Ok(()) => self.editor = None,
                Err(e) => {
                    if let Some(ed) = self.editor.as_mut() {
                        ed.error = Some(e);
                    }
                }
            }
        } else if cancel || !open {
            self.editor = None;
        }
    }

    fn ui_settings(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut open = true;
        let mut changed = false;
        let mut update_now = false;
        let mut install: Option<update::Release> = None;
        let accts: Vec<(String, String)> =
            self.accts.iter().filter(|a| a.live()).map(|a| (a.cfg.id.clone(), a.cfg.label().to_string())).collect();
        egui::Window::new("Settings").open(&mut open).collapsible(false).resizable(false).default_width(460.0).show(ctx, |ui| {
            let s = &mut self.cfg.settings;
            egui::Grid::new("settings").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                ui.label("Check for mail every");
                changed |= ui
                    .add(egui::DragValue::new(&mut s.poll_secs).range(15..=3600).suffix(" s"))
                    .on_hover_text("Used when the server doesn't support push (IDLE). With push, new mail arrives instantly and this becomes a 5-minute safety check.")
                    .changed();
                ui.end_row();
                ui.label("Messages per folder");
                changed |= ui
                    .add(egui::DragValue::new(&mut s.initial_fetch).range(50..=5000))
                    .on_hover_text("How many recent messages to sync per folder (and per 'Load older'). Applies after restart.")
                    .changed();
                ui.end_row();
                ui.label("New messages from");
                let current = accts.iter().find(|(id, _)| *id == s.default_account).map(|(_, l)| l.clone()).unwrap_or_else(|| "First account".into());
                egui::ComboBox::from_id_salt("default-acct").selected_text(current).show_ui(ui, |ui| {
                    for (id, label) in &accts {
                        changed |= ui.selectable_value(&mut s.default_account, id.clone(), label).changed();
                    }
                });
                ui.end_row();
                ui.label("");
                changed |= ui.checkbox(&mut s.mark_read_on_open, "Mark messages read when opened").changed();
                ui.end_row();
                ui.label("");
                changed |= ui.checkbox(&mut s.notifications, "Desktop notifications for new mail").changed();
                ui.end_row();
                ui.label("");
                changed |= ui.add_enabled(s.notifications, egui::Checkbox::new(&mut s.notify_preview, "Show sender and subject in notifications")).changed();
                ui.end_row();
            });
            ui.add_space(6.0);
            ui.label(RichText::new("OAuth client IDs").strong());
            ui.add(egui::Label::new(RichText::new("Pre-filled for new Google / Microsoft accounts. The account editor explains how to create them.").small().weak()).wrap());
            egui::Grid::new("oauth-ids").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Google client ID");
                changed |= ui.add(egui::TextEdit::singleline(&mut s.google_client_id).desired_width(280.0)).changed();
                ui.end_row();
                ui.label("Google client secret");
                changed |= ui.add(egui::TextEdit::singleline(&mut s.google_client_secret).password(true).desired_width(280.0)).changed();
                ui.end_row();
                ui.label("Microsoft client ID");
                changed |= ui.add(egui::TextEdit::singleline(&mut s.microsoft_client_id).desired_width(280.0)).changed();
                ui.end_row();
            });
            ui.add_space(6.0);
            ui.label(RichText::new("Updates").strong());
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("Version {}", update::current()));
                changed |= ui.checkbox(&mut s.auto_update, "Install updates automatically").changed();
            });
            ui.horizontal_wrapped(|ui| {
                let busy = matches!(self.update, UpdateState::Checking | UpdateState::Downloading(_));
                if ui.add_enabled(!busy && self.staged.is_none() && update::platform().is_some(), egui::Button::new("Check now")).clicked() {
                    update_now = true;
                }
                match &self.update {
                    UpdateState::Idle => {}
                    UpdateState::Checking => {
                        ui.spinner();
                        ui.label(RichText::new("Checking…").weak());
                    }
                    UpdateState::UpToDate => {
                        ui.label(RichText::new("✔ You're up to date").color(GOOD));
                    }
                    UpdateState::Available(r) => {
                        if ui.button(format!("Install {}", r.version)).clicked() {
                            install = Some(r.clone());
                        }
                    }
                    UpdateState::Downloading(v) => {
                        ui.spinner();
                        ui.label(RichText::new(format!("Downloading {v}…")).weak());
                    }
                    UpdateState::Ready => {
                        ui.label(RichText::new("✔ Update ready: restart Mail to finish").color(GOOD));
                    }
                    UpdateState::Failed(e) => {
                        ui.add(egui::Label::new(RichText::new(format!("Update failed: {e}")).small().color(ui.visuals().warn_fg_color)).wrap());
                    }
                }
            });
            ui.add_space(6.0);
            ui.label(RichText::new(format!("Data: {}", crate::config::cache_dir().display())).small().weak());
        });
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
        self.settings_open = open;
    }

    fn ui_dialogs(&mut self, ctx: &egui::Context) {
        if let Some((title, src)) = &self.source {
            let mut open = true;
            egui::Window::new(format!("Source — {title}")).id(Id::new("source")).open(&mut open).default_size([760.0, 560.0]).show(
                ctx,
                |ui| {
                    egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(src).monospace()).extend().selectable(true));
                    });
                },
            );
            if !open {
                self.source = None;
            }
        }
        if self.shortcuts_open {
            let mut open = true;
            egui::Window::new("Keyboard shortcuts").id(Id::new("shortcuts")).open(&mut open).collapsible(false).resizable(false).show(
                ctx,
                |ui| {
                    egui::Grid::new("sc").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
                        for (k, d) in SHORTCUTS {
                            ui.label(RichText::new(*k).monospace());
                            ui.label(*d);
                            ui.end_row();
                        }
                    });
                },
            );
            self.shortcuts_open = open;
        }

        // Three-way prompt for closing a compose window with unsaved changes.
        if let Some(Confirm::CloseCompose(id)) = &self.confirm {
            let id = *id;
            let mut choice = None;
            egui::Window::new("Unsaved message").collapsible(false).resizable(false).anchor(Align2::CENTER_CENTER, Vec2::ZERO).show(
                ctx,
                |ui| {
                    ui.label("Save this message as a draft before closing?");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.add(egui::Button::new(RichText::new("Save draft").color(Color32::WHITE)).fill(ACCENT)).clicked() {
                            choice = Some(1);
                        }
                        if ui.button("Discard").clicked() {
                            choice = Some(2);
                        }
                        if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(K::Escape)) {
                            choice = Some(0);
                        }
                    });
                },
            );
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
            let (title, text) = match c {
                Confirm::RemoveAccount(i) => (
                    "Remove account",
                    format!(
                        "Remove {} from this app? Its local cache and saved credentials are deleted. Nothing is deleted on the server. Open messages from this account move to another account.",
                        self.accts.get(*i).map(|a| a.cfg.label()).unwrap_or("")
                    ),
                ),
                Confirm::DeleteForever(keys) => ("Delete permanently", format!("Permanently delete {} message(s)? This cannot be undone.", keys.len())),
                Confirm::CloseCompose(_) => unreachable!(),
            };
            egui::Window::new(title).collapsible(false).resizable(false).anchor(Align2::CENTER_CENTER, Vec2::ZERO).show(ctx, |ui| {
                ui.add(egui::Label::new(text).wrap());
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(egui::Button::new(RichText::new("Confirm").color(Color32::WHITE)).fill(Color32::from_rgb(0xdc, 0x26, 0x26)))
                        .clicked()
                    {
                        decision = Some(true);
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(K::Escape)) {
                        decision = Some(false);
                    }
                });
            });
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
        let n = self.outstanding();
        egui::Window::new("Finishing up").collapsible(false).resizable(false).anchor(Align2::CENTER_CENTER, Vec2::ZERO).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!("Waiting for {n} change(s) to reach the server…"));
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Quit anyway").on_hover_text("Unfinished changes may not be saved on the server").clicked() {
                    self.force_quit = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if ui.button("Keep the app open").clicked() {
                    self.quitting = false;
                }
            });
        });
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.wants_keyboard_input() || self.confirm.is_some() || self.editor.is_some() || self.quitting {
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
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        self.focused.store(focused, Ordering::Relaxed);
        self.handle_events();
        self.poll(focused);
        if self.rows_dirty {
            self.rebuild_rows();
        }
        self.update_title();

        self.ui_top(ctx);
        self.ui_update_banner(ctx);
        self.ui_status(ctx);
        self.ui_folders(ctx);
        self.ui_list(ctx);
        self.ui_reader(ctx);
        self.ui_composes(ctx);
        self.ui_editor(ctx);
        self.ui_settings(ctx);
        self.ui_dialogs(ctx);
        self.ui_toast(ctx);
        self.shortcuts(ctx);
        self.autosave();
        self.guard_exit(ctx);

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
