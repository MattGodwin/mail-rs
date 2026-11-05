//! One background thread per account owns that account's IMAP connection.
//! The UI sends `Cmd`s and receives `Event`s; it never blocks on the network.
//!
//! - Commands run by priority, so opening a message never waits behind a sync.
//! - Mutations carry an operation id and are acknowledged with `Ev::Done`.
//! - Only idempotent commands are retried after a dropped connection; others
//!   report an "outcome unknown" error instead of risking duplicates.
//! - The worker schedules its own inbox checks (slowly while push/IDLE is
//!   active) and raises new-mail notifications, so both work while minimised.

use crate::config::Account;
use crate::model::{self, FolderIndex, FolderInfo, MsgFlag, Special, Summary};
use crate::net::{self, ConnErr, Session};
use crate::store::Store;
use eframe::egui;
use imap::types::{Fetch, Flag, NameAttribute, UnsolicitedResponse};
use mail_parser::MessageParser;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

/// Headers needed for the message list. Tiny compared to full headers.
const HEADER_QUERY: &str = "(UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[HEADER.FIELDS (FROM TO SUBJECT DATE MESSAGE-ID CONTENT-TYPE)])";
const KEEPALIVE: Duration = Duration::from_secs(20 * 60);
const COUNTS_EVERY: Duration = Duration::from_secs(10 * 60);
/// Inbox safety-net check while push (IDLE) is working.
const PUSH_SAFETY_POLL: u64 = 5 * 60;
const PREFETCH_COUNT: usize = 15;
const PREFETCH_MAX_SIZE: u32 = 300 * 1024;
const MAX_STATUS_FOLDERS: usize = 60;

pub type OpId = u64;

#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    /// Refresh the folder list.
    Folders,
    /// User-requested retry: clears an earlier sign-in failure and reconnects.
    Reconnect,
    Sync {
        folder: String,
        more: bool,
    },
    /// Server-side unread counts for all folders (STATUS).
    Counts,
    Body {
        folder: String,
        uid: u32,
    },
    Prefetch {
        folder: String,
    },
    Flag {
        op: OpId,
        folder: String,
        uv: u32,
        uids: Vec<u32>,
        flag: MsgFlag,
        on: bool,
    },
    Move {
        op: OpId,
        folder: String,
        uv: u32,
        uids: Vec<u32>,
        dest: String,
    },
    /// Permanently delete.
    Expunge {
        op: OpId,
        folder: String,
        uv: u32,
        uids: Vec<u32>,
    },
    /// Append a message (e.g. a copy of sent mail).
    Append {
        op: OpId,
        folder: String,
        data: Vec<u8>,
        flags: Vec<MsgFlag>,
    },
    /// Save a draft and remove its older versions (found by Message-ID prefix).
    SaveDraft {
        op: OpId,
        folder: String,
        data: Vec<u8>,
        key: String,
        msg_id: String,
        replaces: Option<(String, u32, u32)>,
    },
    /// Remove all saved versions of a draft (after sending).
    DeleteDraft {
        op: OpId,
        folder: String,
        key: String,
        replaces: Option<(String, u32, u32)>,
    },
    Search {
        req: u64,
        folder: String,
        query: String,
    },
    Shutdown,
}

impl Cmd {
    fn priority(&self) -> u8 {
        match self {
            Cmd::Shutdown | Cmd::Reconnect => 0,
            Cmd::Body { .. } => 1,
            Cmd::Flag { .. }
            | Cmd::Move { .. }
            | Cmd::Expunge { .. }
            | Cmd::SaveDraft { .. }
            | Cmd::DeleteDraft { .. }
            | Cmd::Append { .. } => 2,
            Cmd::Search { .. } => 3,
            Cmd::Folders => 4,
            Cmd::Sync { .. } => 5,
            Cmd::Counts => 6,
            Cmd::Prefetch { .. } => 9,
        }
    }

    fn op(&self) -> Option<OpId> {
        match self {
            Cmd::Flag { op, .. }
            | Cmd::Move { op, .. }
            | Cmd::Expunge { op, .. }
            | Cmd::Append { op, .. }
            | Cmd::SaveDraft { op, .. }
            | Cmd::DeleteDraft { op, .. } => Some(*op),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AcctState {
    Connecting,
    Online,
    Offline(String),
    AuthFailed(String),
}

#[derive(Debug, Clone)]
pub struct OpErr {
    pub msg: String,
    /// The connection dropped mid-operation: it may or may not have happened.
    pub unknown: bool,
}

pub enum Ev {
    Folders(Vec<FolderInfo>),
    Index {
        folder: String,
        index: FolderIndex,
    },
    /// (folder, unseen, messages) from STATUS.
    Counts(Vec<(String, u32, u32)>),
    Body {
        folder: String,
        uv: u32,
        uid: u32,
        raw: Arc<Vec<u8>>,
    },
    BodyFailed {
        folder: String,
        uid: u32,
        error: String,
    },
    Search {
        req: u64,
        folder: String,
        uv: u32,
        msgs: Vec<Summary>,
        total: usize,
    },
    Busy(bool),
    State(AcctState),
    Synced {
        folder: String,
        at: SystemTime,
    },
    Error(String),
    /// Acknowledgement of a mutation. Ok carries an optional note for the user.
    Done {
        op: OpId,
        result: Result<Option<String>, OpErr>,
    },
    /// Push (IDLE) status from the watcher.
    Push(bool),
    // App-level events (not from a worker):
    /// Ok carries the formatted message (for the Sent-folder copy).
    Sent {
        compose: u64,
        result: Result<Vec<u8>, String>,
    },
    OAuth {
        editor: u64,
        result: Result<String, String>,
    },
    NotifyClick {
        folder: String,
        uid: u32,
    },
    Update(crate::update::Msg),
}

pub struct Event {
    pub acct: String,
    pub gen: u64,
    pub kind: Ev,
}

/// Settings the worker reads live, without a round trip through the UI.
pub struct Shared {
    pub poll_secs: AtomicU64,
    pub push: AtomicBool,
    pub notify: AtomicBool,
    pub preview: AtomicBool,
    /// Whether the app window is focused (global, shared by all accounts).
    pub focused: Arc<AtomicBool>,
}

pub struct Handle {
    pub tx: Sender<Cmd>,
    pub join: Option<JoinHandle<()>>,
    pub stop: Arc<AtomicBool>,
}

impl Handle {
    pub fn send(&self, c: Cmd) {
        let _ = self.tx.send(c);
    }
    /// Ask the worker to stop after its current command; returns its thread handle.
    pub fn shutdown(mut self) -> Option<JoinHandle<()>> {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.tx.send(Cmd::Shutdown);
        self.join.take()
    }
}

enum WErr {
    Conn(String),
    Other(String),
}

impl From<imap::Error> for WErr {
    fn from(e: imap::Error) -> Self {
        match e {
            imap::Error::No(s) => WErr::Other(format!("Server refused: {s}")),
            imap::Error::Bad(s) => WErr::Other(format!("Server rejected command: {s}")),
            imap::Error::Validate(v) => WErr::Other(format!("Invalid input: {v}")),
            other => WErr::Conn(other.to_string()),
        }
    }
}

type R<T = ()> = Result<T, WErr>;

pub struct Spawn {
    pub acct: Account,
    pub gen: u64,
    pub initial_fetch: u32,
    /// Stagger background work when many accounts start at once.
    pub start_delay: Duration,
    /// A previous worker for the same account that must finish first.
    pub wait_for: Option<JoinHandle<()>>,
    pub shared: Arc<Shared>,
    pub events: Sender<Event>,
    pub ctx: egui::Context,
}

pub fn spawn(s: Spawn) -> Handle {
    let (tx, rx) = std::sync::mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let name = format!("imap-{}", s.acct.email);
    let stop2 = stop.clone();
    let join = std::thread::Builder::new()
        .name(name)
        .spawn(move || {
            if let Some(prev) = s.wait_for {
                let _ = prev.join();
            }
            let w = Worker {
                store: Store::new(&s.acct.id),
                acct: s.acct,
                gen: s.gen,
                initial_fetch: s.initial_fetch.max(20),
                rx,
                events: s.events,
                ctx: s.ctx,
                shared: s.shared,
                stop: stop2,
                sess: None,
                caps: Default::default(),
                selected: None,
                queue: Vec::new(),
                indexes: HashMap::new(),
                folders: Vec::new(),
                auth_failed: false,
                busy: false,
                state: None,
                start_at: Instant::now() + s.start_delay,
                next_inbox_poll: Instant::now() + Duration::from_secs(30),
                next_counts: Instant::now() + Duration::from_secs(20),
                last_wall: SystemTime::now(),
                suppress_new: HashSet::new(),
            };
            w.run();
        })
        .expect("spawn worker thread");
    Handle { tx, join: Some(join), stop }
}

struct Worker {
    acct: Account,
    gen: u64,
    store: Store,
    initial_fetch: u32,
    rx: Receiver<Cmd>,
    events: Sender<Event>,
    ctx: egui::Context,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    sess: Option<Session>,
    caps: net::Caps,
    /// Currently selected mailbox and its UIDVALIDITY.
    selected: Option<(String, u32)>,
    queue: Vec<Cmd>,
    indexes: HashMap<String, FolderIndex>,
    folders: Vec<FolderInfo>,
    auth_failed: bool,
    busy: bool,
    state: Option<AcctState>,
    start_at: Instant,
    next_inbox_poll: Instant,
    next_counts: Instant,
    last_wall: SystemTime,
    /// Folders we just moved messages into: their next new arrivals are ours, not new mail.
    suppress_new: HashSet<String>,
}

impl Worker {
    fn emit(&self, kind: Ev) {
        let _ = self.events.send(Event { acct: self.acct.id.clone(), gen: self.gen, kind });
        self.ctx.request_repaint();
    }

    fn set_state(&mut self, s: AcctState) {
        if self.state.as_ref() != Some(&s) {
            self.state = Some(s.clone());
            self.emit(Ev::State(s));
        }
    }

    fn enqueue(&mut self, c: Cmd) {
        let dedupe = matches!(c, Cmd::Folders | Cmd::Sync { .. } | Cmd::Prefetch { .. } | Cmd::Body { .. } | Cmd::Counts);
        if dedupe && self.queue.contains(&c) {
            return;
        }
        self.queue.push(c);
    }

    /// Pull everything waiting on the channel into the queue. False if the UI is gone.
    fn drain(&mut self) -> bool {
        loop {
            match self.rx.try_recv() {
                Ok(c) => self.enqueue(c),
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => return false,
            }
        }
    }

    fn set_busy(&mut self, busy: bool) {
        if self.busy != busy {
            self.busy = busy;
            self.emit(Ev::Busy(busy));
        }
    }

    fn inbox(&self) -> String {
        self.folders.iter().find(|f| f.special == Some(Special::Inbox)).map(|f| f.name.clone()).unwrap_or_else(|| "INBOX".into())
    }

    fn poll_interval(&self) -> Duration {
        let secs =
            if self.shared.push.load(Ordering::Relaxed) { PUSH_SAFETY_POLL } else { self.shared.poll_secs.load(Ordering::Relaxed).max(15) };
        Duration::from_secs(secs)
    }

    /// Timers: inbox polling, folder counts, sleep/resume detection.
    fn tick(&mut self) {
        let now = Instant::now();
        // A wall-clock jump much larger than expected means the machine slept:
        // the old connection is almost certainly dead, so start fresh.
        let wall = SystemTime::now();
        let gap = wall.duration_since(self.last_wall).unwrap_or_default();
        self.last_wall = wall;
        if gap > self.poll_interval() + Duration::from_secs(90) && gap > Duration::from_secs(120) {
            self.sess = None;
            self.selected = None;
            self.next_inbox_poll = now;
        }
        if now >= self.next_inbox_poll {
            self.next_inbox_poll = now + self.poll_interval();
            let inbox = self.inbox();
            self.enqueue(Cmd::Sync { folder: inbox, more: false });
        }
        if now >= self.next_counts {
            self.next_counts = now + COUNTS_EVERY;
            self.enqueue(Cmd::Counts);
        }
    }

    fn run(mut self) {
        if let Some(f) = self.store.load_folders() {
            self.folders = f;
        }
        loop {
            if self.stop.load(Ordering::SeqCst) || !self.drain() {
                break;
            }
            self.tick();
            // Before the staggered start time, only run what the user asked for.
            let ready = Instant::now() >= self.start_at || self.queue.iter().any(|c| c.priority() <= 3);
            if self.queue.is_empty() || !ready {
                self.set_busy(false);
                let wake = [self.next_inbox_poll, self.next_counts, if ready { far() } else { self.start_at }].into_iter().min().unwrap();
                let timeout = wake.saturating_duration_since(Instant::now()).clamp(Duration::from_millis(50), KEEPALIVE);
                match self.rx.recv_timeout(timeout) {
                    Ok(c) => self.enqueue(c),
                    Err(RecvTimeoutError::Timeout) => {
                        if let Some(s) = self.sess.as_mut() {
                            if timeout >= KEEPALIVE && s.noop().is_err() {
                                self.sess = None;
                                self.selected = None;
                            }
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                continue;
            }
            let i = (0..self.queue.len()).min_by_key(|&i| (self.queue[i].priority(), i)).unwrap();
            let cmd = self.queue.remove(i);
            if cmd == Cmd::Shutdown {
                break;
            }
            self.set_busy(true);
            self.exec(cmd);
            if let Some(s) = self.sess.as_mut() {
                // Untagged responses pile up in this channel; we re-sync instead.
                while s.unsolicited_responses.try_recv().is_ok() {}
            }
        }
        // Anything still queued will never run: say so, so nothing waits forever.
        self.drain();
        for c in std::mem::take(&mut self.queue) {
            if let Some(op) = c.op() {
                self.emit(Ev::Done {
                    op,
                    result: Err(OpErr { msg: "The account was stopped before this finished".into(), unknown: false }),
                });
            }
        }
        self.set_busy(false);
        if let Some(mut s) = self.sess.take() {
            let _ = s.logout();
        }
    }

    /// Whether running the command twice is harmless.
    fn retry_safe(&self, cmd: &Cmd) -> bool {
        match cmd {
            Cmd::Append { .. } | Cmd::SaveDraft { .. } => false,
            Cmd::Move { .. } => self.caps.mv,
            _ => true,
        }
    }

    fn exec(&mut self, cmd: Cmd) {
        if cmd == Cmd::Reconnect {
            self.auth_failed = false;
            self.sess = None;
            self.selected = None;
            let inbox = self.inbox();
            self.enqueue(Cmd::Folders);
            self.enqueue(Cmd::Sync { folder: inbox, more: false });
            return;
        }
        for attempt in 0..2 {
            if self.sess.is_none() {
                if self.auth_failed {
                    self.fail(&cmd, "Sign-in failed earlier; use \"Sign in again\" or Retry".into(), false);
                    return;
                }
                self.set_state(AcctState::Connecting);
                match net::connect(&self.acct) {
                    Ok(c) => {
                        self.sess = Some(c.sess);
                        self.caps = c.caps;
                        self.set_state(AcctState::Online);
                    }
                    Err(e) => {
                        let msg = e.message().to_string();
                        match e {
                            ConnErr::Auth(_) => {
                                self.auth_failed = true;
                                self.set_state(AcctState::AuthFailed(msg.clone()));
                            }
                            _ => self.set_state(AcctState::Offline(msg.clone())),
                        }
                        self.fail(&cmd, msg, false);
                        return;
                    }
                }
            }
            match self.run_cmd(&cmd) {
                Ok(note) => {
                    if let Some(op) = cmd.op() {
                        self.emit(Ev::Done { op, result: Ok(note) });
                    }
                    return;
                }
                Err(WErr::Conn(e)) => {
                    self.sess = None;
                    self.selected = None;
                    if attempt == 1 || !self.retry_safe(&cmd) {
                        self.set_state(AcctState::Offline(e.clone()));
                        let unknown = !self.retry_safe(&cmd);
                        let msg = if unknown {
                            format!("Connection lost; it's unclear whether this completed ({e}). Refreshing to check.")
                        } else {
                            format!("Connection error: {e}")
                        };
                        self.fail(&cmd, msg, unknown);
                        return;
                    }
                }
                Err(WErr::Other(e)) => {
                    self.fail(&cmd, e, false);
                    return;
                }
            }
        }
    }

    fn fail(&self, cmd: &Cmd, error: String, unknown: bool) {
        match cmd {
            Cmd::Body { folder, uid } => self.emit(Ev::BodyFailed { folder: folder.clone(), uid: *uid, error: error.clone() }),
            Cmd::Search { req, folder, .. } => {
                self.emit(Ev::Search { req: *req, folder: folder.clone(), uv: 0, msgs: vec![], total: 0 });
            }
            _ => {}
        }
        if let Some(op) = cmd.op() {
            self.emit(Ev::Done { op, result: Err(OpErr { msg: error, unknown }) });
            return;
        }
        // Background work failing is shown through the account state, not as an error popup.
        if !matches!(cmd, Cmd::Prefetch { .. } | Cmd::Counts | Cmd::Sync { .. } | Cmd::Folders) {
            self.emit(Ev::Error(error));
        }
    }

    fn sess(&mut self) -> R<&mut Session> {
        self.sess.as_mut().ok_or_else(|| WErr::Conn("not connected".into()))
    }

    /// SELECT the folder unless it already is; returns UIDVALIDITY.
    fn ensure_selected(&mut self, folder: &str) -> R<u32> {
        if let Some((f, uv)) = &self.selected {
            if f == folder {
                return Ok(*uv);
            }
        }
        self.select(folder)
    }

    fn select(&mut self, folder: &str) -> R<u32> {
        let mb = self.sess()?.select(folder)?;
        let uv = mb.uid_validity.unwrap_or(0);
        self.selected = Some((folder.to_string(), uv));
        Ok(uv)
    }

    /// Refuse to act on UIDs from an older incarnation of the folder.
    fn check_uv(&mut self, folder: &str, expected: u32) -> R<u32> {
        let uv = self.ensure_selected(folder)?;
        if expected != 0 && uv != expected {
            self.enqueue(Cmd::Sync { folder: folder.to_string(), more: false });
            return Err(WErr::Other("This folder was reset on the server; refreshing it. Please try again.".into()));
        }
        Ok(uv)
    }

    fn index_mut(&mut self, folder: &str) -> &mut FolderIndex {
        if !self.indexes.contains_key(folder) {
            let idx = self.store.load_index(folder).unwrap_or_default();
            self.indexes.insert(folder.to_string(), idx);
        }
        self.indexes.get_mut(folder).unwrap()
    }

    fn run_cmd(&mut self, cmd: &Cmd) -> R<Option<String>> {
        match cmd {
            Cmd::Folders => self.folders().map(|_| None),
            Cmd::Sync { folder, more } => self.sync(folder, *more).map(|_| None),
            Cmd::Counts => self.counts().map(|_| None),
            Cmd::Body { folder, uid } => self.body(folder, *uid).map(|_| None),
            Cmd::Prefetch { folder } => self.prefetch(folder).map(|_| None),
            Cmd::Flag { folder, uv, uids, flag, on, .. } => self.flag(folder, *uv, uids, *flag, *on).map(|_| None),
            Cmd::Move { folder, uv, uids, dest, .. } => self.move_to(folder, *uv, uids, Some(dest)),
            Cmd::Expunge { folder, uv, uids, .. } => self.move_to(folder, *uv, uids, None),
            Cmd::Append { folder, data, flags, .. } => {
                let flags: Vec<Flag<'static>> = flags.iter().map(|f| to_flag(*f)).collect();
                self.sess()?.append_with_flags(folder, data, &flags)?;
                if self.store.load_index(folder).is_some() {
                    self.enqueue(Cmd::Sync { folder: folder.clone(), more: false });
                }
                Ok(None)
            }
            Cmd::SaveDraft { folder, data, key, msg_id, replaces, .. } => self.save_draft(folder, data, key, msg_id, replaces.as_ref()),
            Cmd::DeleteDraft { folder, key, replaces, .. } => self.delete_draft(folder, key, replaces.as_ref()),
            Cmd::Search { req, folder, query } => self.search(*req, folder, query).map(|_| None),
            Cmd::Reconnect | Cmd::Shutdown => Ok(None),
        }
    }

    fn folders(&mut self) -> R {
        let names = self.sess()?.list(Some(""), Some("*"))?;
        let mut out: Vec<(FolderInfo, bool, String)> = Vec::new();
        for n in names.iter() {
            let attrs: Vec<String> = n
                .attributes()
                .iter()
                .map(|a| match a {
                    NameAttribute::NoInferiors => "\\Noinferiors".to_string(),
                    NameAttribute::NoSelect => "\\Noselect".to_string(),
                    NameAttribute::Marked => "\\Marked".to_string(),
                    NameAttribute::Unmarked => "\\Unmarked".to_string(),
                    NameAttribute::Custom(c) => c.to_string(),
                })
                .collect();
            let delim = n.delimiter().unwrap_or("").to_string();
            let raw = n.name().to_string();
            let decoded = model::decode_mutf7(&raw);
            let parts: Vec<&str> = if delim.is_empty() { vec![decoded.as_str()] } else { decoded.split(delim.as_str()).collect() };
            let selectable = !attrs.iter().any(|a| a.eq_ignore_ascii_case("\\Noselect") || a.eq_ignore_ascii_case("\\NonExistent"));
            let class = model::classify(&raw, &delim, &attrs);
            let sort_key = parts.join("\u{0}").to_lowercase();
            out.push((
                FolderInfo {
                    display: if raw.eq_ignore_ascii_case("INBOX") {
                        "Inbox".to_string()
                    } else {
                        parts.last().copied().unwrap_or("").to_string()
                    },
                    depth: parts.len().saturating_sub(1),
                    special: class.map(|c| c.0),
                    selectable,
                    name: raw,
                },
                class.is_some_and(|c| c.1),
                sort_key,
            ));
        }
        // A SPECIAL-USE attribute wins over a name-based guess for the same role.
        let by_attr: HashSet<Special> =
            out.iter().filter(|(f, attr, _)| *attr && f.special.is_some()).filter_map(|(f, _, _)| f.special).collect();
        for (f, attr, _) in out.iter_mut() {
            if !*attr && f.special.is_some_and(|s| by_attr.contains(&s)) {
                f.special = None;
            }
        }
        out.sort_by(|a, b| match (a.0.special, b.0.special) {
            (Some(x), Some(y)) => x.cmp(&y).then(a.2.cmp(&b.2)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.2.cmp(&b.2),
        });
        let mut folders: Vec<FolderInfo> = out.into_iter().map(|(f, _, _)| f).collect();
        for f in folders.iter_mut().filter(|f| f.special.is_some()) {
            f.depth = 0;
        }
        // Hide non-selectable containers that end up with no children (e.g. "[Gmail]").
        let keep: Vec<bool> = (0..folders.len())
            .map(|i| {
                let f = &folders[i];
                f.selectable || folders.get(i + 1).is_some_and(|n| n.special.is_none() && n.depth > f.depth)
            })
            .collect();
        let mut i = 0;
        folders.retain(|_| {
            i += 1;
            keep[i - 1]
        });
        self.store.save_folders(&folders);
        self.folders = folders.clone();
        self.emit(Ev::Folders(folders));
        self.enqueue(Cmd::Counts);
        Ok(())
    }

    fn sync(&mut self, folder: &str, more: bool) -> R {
        // Always re-SELECT for fresh EXISTS / UIDNEXT.
        let mb = self.sess()?.select(folder)?;
        let uv = mb.uid_validity.unwrap_or(0);
        self.selected = Some((folder.to_string(), uv));
        let n = self.initial_fetch;
        let before = self.index_mut(folder).clone();
        let mut idx = before.clone();
        let mut reset = false;
        if idx.uidvalidity != uv {
            if idx.uidvalidity != 0 {
                self.store.clear_folder(folder);
            }
            idx = FolderIndex { uidvalidity: uv, ..Default::default() };
            reset = true;
        }
        idx.exists = mb.exists;
        let initial = idx.msgs.is_empty();
        let mut arrived: Vec<Summary> = Vec::new();

        let s = self.sess()?;
        if mb.exists == 0 {
            idx.msgs.clear();
        } else if initial {
            let start = mb.exists.saturating_sub(n - 1).max(1);
            let fetched = s.fetch(format!("{start}:{}", mb.exists), HEADER_QUERY)?;
            idx.msgs.extend(fetched.iter().filter_map(summarize));
        } else {
            let (min, max) = (idx.min_uid(), idx.max_uid());
            // Flag changes and deletions within the window we hold.
            let flags = s.uid_fetch(format!("{min}:{max}"), "(UID FLAGS)")?;
            let mut current: HashMap<u32, Summary> = HashMap::new();
            for f in flags.iter() {
                if let Some(uid) = f.uid {
                    let mut s = Summary::default();
                    if apply_flags(&mut s, f.flags()) {
                        current.insert(uid, s);
                    }
                }
            }
            idx.msgs.retain_mut(|m| match current.get(&m.uid) {
                Some(fl) => {
                    m.seen = fl.seen;
                    m.flagged = fl.flagged;
                    m.answered = fl.answered;
                    m.draft = fl.draft;
                    true
                }
                None => false,
            });
            if mb.uid_next.is_none_or(|next| next > max + 1) {
                let fetched = s.uid_fetch(format!("{}:*", max + 1), HEADER_QUERY)?;
                arrived = fetched.iter().filter_map(summarize).filter(|m| m.uid > max).collect();
                idx.msgs.extend(arrived.iter().cloned());
            }
            let min = idx.min_uid();
            if more && min > 1 {
                let mut older: Vec<u32> = s.uid_search(format!("UID 1:{}", min - 1))?.into_iter().filter(|&u| u < min).collect();
                older.sort_unstable();
                let take = older.split_off(older.len().saturating_sub(n as usize));
                if !take.is_empty() {
                    let fetched = s.uid_fetch(model::uid_set(&take), HEADER_QUERY)?;
                    idx.msgs.extend(fetched.iter().filter_map(summarize));
                }
            }
        }
        // Authoritative unread count (the index only holds the newest messages).
        idx.unseen = Some(s.uid_search("UNSEEN")?.len() as u32);

        idx.msgs.sort_unstable_by_key(|m| m.uid);
        idx.msgs.dedup_by_key(|m| m.uid);
        idx.sort();
        let changed = idx != before;
        *self.index_mut(folder) = idx;
        if changed {
            let idx = self.index_mut(folder).clone();
            self.store.save_index(folder, &idx);
            self.emit(Ev::Index { folder: folder.to_string(), index: idx });
        }
        self.emit(Ev::Synced { folder: folder.to_string(), at: SystemTime::now() });

        let is_inbox = folder == self.inbox();
        let ours = self.suppress_new.remove(folder);
        if is_inbox && !initial && !reset && !more && !ours {
            arrived.retain(|m| !m.seen);
            if !arrived.is_empty() {
                self.notify(folder, &arrived);
            }
        }
        if is_inbox && (initial || !arrived.is_empty()) {
            self.enqueue(Cmd::Prefetch { folder: folder.to_string() });
        }
        Ok(())
    }

    fn notify(&self, folder: &str, msgs: &[Summary]) {
        let sh = &self.shared;
        if !sh.notify.load(Ordering::Relaxed) || sh.focused.load(Ordering::Relaxed) {
            return;
        }
        let preview = sh.preview.load(Ordering::Relaxed);
        let (title, body) = if msgs.len() == 1 && preview {
            (format!("{} — {}", msgs[0].sender(), self.acct.label()), msgs[0].subject.clone())
        } else if preview {
            let senders: Vec<&str> = msgs.iter().take(3).map(|m| m.sender()).collect();
            (format!("{} new messages — {}", msgs.len(), self.acct.label()), format!("From {}", senders.join(", ")))
        } else {
            (format!("New mail — {}", self.acct.label()), format!("{} new message(s)", msgs.len()))
        };
        let newest = msgs.iter().max_by_key(|m| m.date).map(|m| m.uid).unwrap_or(0);
        let (events, ctx, acct, gen, folder) = (self.events.clone(), self.ctx.clone(), self.acct.id.clone(), self.gen, folder.to_string());
        crate::notify::show(title, body, move || {
            let _ = events.send(Event { acct, gen, kind: Ev::NotifyClick { folder, uid: newest } });
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            ctx.request_repaint();
        });
        self.ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(egui::UserAttentionType::Informational));
    }

    /// Server-side unread/total counts for every folder.
    fn counts(&mut self) -> R {
        let folders: Vec<String> = self.folders.iter().filter(|f| f.selectable).map(|f| f.name.clone()).take(MAX_STATUS_FOLDERS).collect();
        let mut out = Vec::new();
        for f in folders {
            if self.selected.as_ref().is_some_and(|(s, _)| *s == f) {
                // STATUS on the selected mailbox is discouraged; ask directly.
                let s = self.sess()?;
                let unseen = s.uid_search("UNSEEN")?.len() as u32;
                let total = self.indexes.get(&f).map(|i| i.exists).unwrap_or(0);
                out.push((f, unseen, total));
                continue;
            }
            let s = self.sess()?;
            match s.status(&f, "(MESSAGES UNSEEN)") {
                Ok(_) => {}
                Err(imap::Error::No(_)) | Err(imap::Error::Bad(_)) => continue,
                Err(e) => return Err(e.into()),
            }
            // imap 2.4 delivers STATUS data on the unsolicited channel.
            let (mut unseen, mut total) = (None, None);
            while let Ok(r) = s.unsolicited_responses.try_recv() {
                if let UnsolicitedResponse::Status { mailbox, attributes } = r {
                    if mailbox == f {
                        for a in attributes {
                            match a {
                                imap::types::StatusAttribute::Unseen(n) => unseen = Some(n),
                                imap::types::StatusAttribute::Messages(n) => total = Some(n),
                                _ => {}
                            }
                        }
                    }
                }
            }
            if let (Some(u), Some(t)) = (unseen, total) {
                out.push((f, u, t));
            }
        }
        self.emit(Ev::Counts(out));
        Ok(())
    }

    fn body(&mut self, folder: &str, uid: u32) -> R {
        let uv = self.ensure_selected(folder)?;
        let raw = match self.store.load_body(folder, uv, uid) {
            Some(raw) => raw,
            None => {
                let fetched = self.sess()?.uid_fetch(uid.to_string(), "(UID BODY.PEEK[])")?;
                let raw = fetched
                    .iter()
                    .find(|f| f.uid == Some(uid))
                    .and_then(|f| f.body())
                    .map(|b| b.to_vec())
                    .ok_or_else(|| WErr::Other("The message no longer exists on the server".into()))?;
                self.store.save_body(folder, uv, uid, &raw);
                raw
            }
        };
        self.emit(Ev::Body { folder: folder.to_string(), uv, uid, raw: Arc::new(raw) });
        Ok(())
    }

    /// Download a few of the newest small messages so opening them is instant.
    fn prefetch(&mut self, folder: &str) -> R {
        let uv = self.ensure_selected(folder)?;
        let want: Vec<u32> =
            self.index_mut(folder).msgs.iter().take(PREFETCH_COUNT).filter(|m| m.size <= PREFETCH_MAX_SIZE).map(|m| m.uid).collect();
        let want: Vec<u32> = want.into_iter().filter(|&u| !self.store.has_body(folder, uv, u)).collect();
        for chunk in want.chunks(3) {
            let fetched = self.sess()?.uid_fetch(model::uid_set(chunk), "(UID BODY.PEEK[])")?;
            for f in fetched.iter() {
                if let (Some(uid), Some(b)) = (f.uid, f.body()) {
                    self.store.save_body(folder, uv, uid, b);
                }
            }
            // Yield to anything the user asked for in the meantime.
            if !self.drain() || self.stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            if self.queue.iter().any(|c| c.priority() < 5) {
                self.enqueue(Cmd::Prefetch { folder: folder.to_string() });
                return Ok(());
            }
        }
        Ok(())
    }

    fn flag(&mut self, folder: &str, uv: u32, uids: &[u32], flag: MsgFlag, on: bool) -> R {
        self.check_uv(folder, uv)?;
        let op = if on { "+" } else { "-" };
        self.sess()?.uid_store(model::uid_set(uids), format!("{op}FLAGS.SILENT ({})", flag.imap()))?;
        let idx = self.index_mut(folder);
        for m in idx.msgs.iter_mut().filter(|m| uids.contains(&m.uid)) {
            flag.apply(m, on);
        }
        if flag == MsgFlag::Seen {
            if let Some(u) = idx.unseen.as_mut() {
                *u = if on { u.saturating_sub(uids.len() as u32) } else { *u + uids.len() as u32 };
            }
        }
        let idx = idx.clone();
        self.store.save_index(folder, &idx);
        Ok(())
    }

    /// Permanently remove exactly these UIDs from the selected folder.
    /// Returns false if the server could not do that without also expunging
    /// other messages someone marked \Deleted; then nothing extra is removed
    /// and our messages stay flagged (hidden) until the folder is next compacted.
    fn expunge_exact(&mut self, uids: &[u32]) -> R<bool> {
        let set = model::uid_set(uids);
        let uidplus = self.caps.uidplus;
        let s = self.sess()?;
        s.uid_store(&set, "+FLAGS.SILENT (\\Deleted)")?;
        if uidplus {
            s.uid_expunge(&set)?;
            return Ok(true);
        }
        let mine: HashSet<u32> = uids.iter().copied().collect();
        let deleted = s.uid_search("DELETED")?;
        if deleted.iter().all(|u| mine.contains(u)) {
            s.expunge()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Move messages to `dest`, or permanently delete them when `dest` is None.
    fn move_to(&mut self, folder: &str, uv: u32, uids: &[u32], dest: Option<&String>) -> R<Option<String>> {
        self.check_uv(folder, uv)?;
        let set = model::uid_set(uids);
        let mut note = None;
        match dest {
            Some(dest) if self.caps.mv => {
                self.sess()?.uid_mv(&set, dest)?;
            }
            _ => {
                if let Some(dest) = dest {
                    self.sess()?.uid_copy(&set, dest)?;
                }
                if !self.expunge_exact(uids)? {
                    note = Some(
                        "The server can't remove only these messages without also removing others another app marked as deleted, so they're marked deleted and hidden instead.".to_string(),
                    );
                }
            }
        }
        if let Some(dest) = dest {
            self.suppress_new.insert(dest.clone());
        }
        let idx = self.index_mut(folder);
        idx.msgs.retain(|m| !uids.contains(&m.uid));
        idx.exists = idx.exists.saturating_sub(uids.len() as u32);
        let idx = idx.clone();
        let uv = idx.uidvalidity;
        self.store.save_index(folder, &idx);
        for &uid in uids {
            self.store.remove_body(folder, uv, uid);
        }
        Ok(note)
    }

    fn draft_versions(&mut self, key: &str) -> R<HashSet<u32>> {
        Ok(self.sess()?.uid_search(format!("HEADER Message-ID \"draft-{key}-\""))?)
    }

    fn save_draft(
        &mut self,
        folder: &str,
        data: &[u8],
        key: &str,
        msg_id: &str,
        replaces: Option<&(String, u32, u32)>,
    ) -> R<Option<String>> {
        self.sess()?.append_with_flags(folder, data, &[Flag::Seen, Flag::Draft])?;
        // From here on the draft is safely stored; clean-up problems are only notes.
        let note = (|| -> R<Option<String>> {
            let uv = self.select(folder)?;
            let new = self.sess()?.uid_search(format!("HEADER Message-ID \"{msg_id}\""))?;
            if new.is_empty() {
                return Ok(Some("Draft saved, but older copies were kept because the new one couldn't be identified.".into()));
            }
            let mut old: Vec<u32> = self.draft_versions(key)?.into_iter().filter(|u| !new.contains(u)).collect();
            if let Some((f, ruv, uid)) = replaces {
                if f == folder && *ruv == uv && !new.contains(uid) {
                    old.push(*uid);
                }
            }
            if !old.is_empty() && !self.expunge_exact(&old)? {
                return Ok(Some("Draft saved; older copies are marked deleted.".into()));
            }
            Ok(None)
        })();
        let note = match note {
            Ok(n) => n,
            Err(WErr::Other(e)) | Err(WErr::Conn(e)) => Some(format!("Draft saved, but cleaning up older copies failed: {e}")),
        };
        if self.sess.is_some() {
            self.enqueue(Cmd::Sync { folder: folder.to_string(), more: false });
        }
        Ok(note)
    }

    fn delete_draft(&mut self, folder: &str, key: &str, replaces: Option<&(String, u32, u32)>) -> R<Option<String>> {
        let uv = self.select(folder)?;
        let mut uids: Vec<u32> = self.draft_versions(key)?.into_iter().collect();
        if let Some((f, ruv, uid)) = replaces {
            if f == folder && *ruv == uv {
                uids.push(*uid);
            }
        }
        uids.sort_unstable();
        uids.dedup();
        if !uids.is_empty() {
            self.expunge_exact(&uids)?;
        }
        self.enqueue(Cmd::Sync { folder: folder.to_string(), more: false });
        Ok(None)
    }

    fn search(&mut self, req: u64, folder: &str, query: &str) -> R {
        let uv = self.ensure_selected(folder)?;
        let quoted: String = query
            .chars()
            .filter(|c| !c.is_control())
            .flat_map(|c| match c {
                '"' | '\\' => vec!['\\', c],
                c => vec![c],
            })
            .collect();
        let cmd = if query.is_ascii() {
            format!("NOT DELETED TEXT \"{quoted}\"")
        } else {
            format!("CHARSET UTF-8 NOT DELETED TEXT \"{quoted}\"")
        };
        let mut uids: Vec<u32> = self.sess()?.uid_search(cmd)?.into_iter().collect();
        let total = uids.len();
        uids.sort_unstable();
        let take = uids.split_off(uids.len().saturating_sub(self.initial_fetch as usize));
        let mut msgs = Vec::new();
        if !take.is_empty() {
            let fetched = self.sess()?.uid_fetch(model::uid_set(&take), HEADER_QUERY)?;
            msgs.extend(fetched.iter().filter_map(summarize));
        }
        let mut idx = FolderIndex { msgs, ..Default::default() };
        idx.sort();
        self.emit(Ev::Search { req, folder: folder.to_string(), uv, msgs: idx.msgs, total });
        Ok(())
    }
}

fn far() -> Instant {
    Instant::now() + KEEPALIVE
}

fn to_flag(f: MsgFlag) -> Flag<'static> {
    match f {
        MsgFlag::Seen => Flag::Seen,
        MsgFlag::Flagged => Flag::Flagged,
        MsgFlag::Answered => Flag::Answered,
    }
}

/// Apply IMAP flags; returns false for messages marked \Deleted (treated as gone).
fn apply_flags(s: &mut Summary, flags: &[Flag<'_>]) -> bool {
    s.seen = false;
    s.flagged = false;
    s.answered = false;
    s.draft = false;
    for f in flags {
        match f {
            Flag::Seen => s.seen = true,
            Flag::Flagged => s.flagged = true,
            Flag::Answered => s.answered = true,
            Flag::Draft => s.draft = true,
            Flag::Deleted => return false,
            _ => {}
        }
    }
    true
}

fn summarize(f: &Fetch) -> Option<Summary> {
    let uid = f.uid?;
    let mut s = Summary { uid, size: f.size.unwrap_or(0), ..Default::default() };
    if !apply_flags(&mut s, f.flags()) {
        return None;
    }
    let mut header_date = None;
    if let Some(m) = f.header().and_then(|h| MessageParser::default().parse_headers(h)) {
        s.subject = model::one_line(m.subject().unwrap_or(""));
        if let Some(a) = m.from().and_then(|a| a.first()) {
            s.from_name = model::one_line(a.name().unwrap_or(""));
            s.from_addr = a.address().unwrap_or("").to_string();
        }
        s.to = m
            .to()
            .map(|t| t.mailboxes().map(|a| a.name().or(a.address()).unwrap_or("").to_string()).collect::<Vec<_>>().join(", "))
            .unwrap_or_default();
        s.to = model::one_line(&s.to);
        s.message_id = m.message_id().unwrap_or("").to_string();
        s.attach = m.content_type().is_some_and(|ct| {
            ct.ctype().eq_ignore_ascii_case("multipart") && ct.subtype().is_some_and(|st| st.eq_ignore_ascii_case("mixed"))
        });
        header_date = m.date().map(|d| d.to_timestamp());
    }
    s.date = f.internal_date().map(|d| d.timestamp()).or(header_date).unwrap_or(0);
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// Replays a canned server conversation; ignores what the client writes.
    struct Mock(std::io::Cursor<Vec<u8>>);
    impl Read for Mock {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.read(buf)
        }
    }
    impl Write for Mock {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn summarizes_header_fields_fetch() {
        let hdr = "From: =?UTF-8?Q?J=C3=BCrgen?= <j@example.de>\r\nTo: a@x.org, \"B\" <b@x.org>\r\nSubject: Quarterly\r\n report\r\nMessage-ID: <m1@x>\r\nContent-Type: multipart/mixed; boundary=z\r\n\r\n";
        let script = format!(
            "* OK ready\r\na1 OK LOGIN completed\r\n* 1 FETCH (UID 42 FLAGS (\\Seen \\Flagged) INTERNALDATE \"05-Nov-2026 10:00:00 +0000\" RFC822.SIZE 1234 BODY[HEADER.FIELDS (FROM TO SUBJECT DATE MESSAGE-ID CONTENT-TYPE)] {{{}}}\r\n{hdr})\r\n* 2 FETCH (UID 43 FLAGS (\\Deleted) RFC822.SIZE 10)\r\na2 OK FETCH completed\r\n",
            hdr.len()
        );
        let mut client = imap::Client::new(Mock(std::io::Cursor::new(script.into_bytes())));
        client.read_greeting().unwrap();
        let mut sess = client.login("u", "p").map_err(|(e, _)| e).unwrap();
        let fetched = sess.fetch("1:2", HEADER_QUERY).unwrap();
        let s = summarize(&fetched[0]).unwrap();
        assert_eq!(s.uid, 42);
        assert_eq!(s.from_name, "Jürgen");
        assert_eq!(s.from_addr, "j@example.de");
        assert_eq!(s.subject, "Quarterly report");
        assert_eq!(s.to, "a@x.org, B");
        assert_eq!(s.message_id, "m1@x");
        assert!(s.seen && s.flagged && !s.answered);
        assert!(s.attach);
        assert_eq!(s.size, 1234);
        assert_eq!(s.date, 1793872800);
        // Messages flagged \Deleted are treated as already gone.
        assert!(summarize(&fetched[1]).is_none());
    }

    #[test]
    fn status_arrives_on_unsolicited_channel() {
        let script = "* OK ready\r\na1 OK LOGIN completed\r\n* STATUS \"Archive\" (MESSAGES 12 UNSEEN 3)\r\na2 OK STATUS completed\r\n";
        let mut client = imap::Client::new(Mock(std::io::Cursor::new(script.as_bytes().to_vec())));
        client.read_greeting().unwrap();
        let mut sess = client.login("u", "p").map_err(|(e, _)| e).unwrap();
        sess.status("Archive", "(MESSAGES UNSEEN)").unwrap();
        let mut got = None;
        while let Ok(r) = sess.unsolicited_responses.try_recv() {
            if let UnsolicitedResponse::Status { mailbox, attributes } = r {
                got = Some((mailbox, attributes));
            }
        }
        let (mailbox, attrs) = got.expect("status response");
        assert_eq!(mailbox, "Archive");
        assert!(attrs.contains(&imap::types::StatusAttribute::Unseen(3)));
        assert!(attrs.contains(&imap::types::StatusAttribute::Messages(12)));
    }
}
