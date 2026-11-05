//! Push mail: a second IMAP connection per account that sits in IDLE on the
//! inbox and tells the worker to sync as soon as the server reports a change.
//!
//! The watcher never touches the cache itself; it only nudges the worker.
//! A watchdog thread bounds every blocking step (including the library's
//! IDLE clean-up, which reads without a timeout) by shutting the socket down,
//! detects sleep/resume, and handles stop/kick requests.

use crate::config::Account;
use crate::net::{self, ConnErr};
use crate::worker::{Cmd, Ev, Event, Shared};
use eframe::egui;
use imap::extensions::idle::WaitOutcome;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Re-issue IDLE well inside common NAT/firewall idle timeouts (and RFC 2177's 29 minutes).
const RENEW: Duration = Duration::from_secs(9 * 60);
/// Extra time allowed for IDLE termination / connect before the watchdog steps in.
const GRACE: Duration = Duration::from_secs(60);
/// A connection that has stayed up this long resets the backoff.
const HEALTHY: Duration = Duration::from_secs(120);

struct Inner {
    stop: AtomicBool,
    kick: AtomicBool,
    /// Unix millis by which the current blocking step must finish (0 = none).
    deadline: AtomicU64,
    sock: Mutex<Option<TcpStream>>,
}

pub struct Watcher {
    inner: Arc<Inner>,
}

impl Watcher {
    /// Stop for good (account removed, paused or edited).
    pub fn stop(&self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        self.close_socket();
    }

    /// Reconnect now (e.g. the user pressed Retry).
    pub fn kick(&self) {
        self.inner.kick.store(true, Ordering::SeqCst);
        self.close_socket();
    }

    fn close_socket(&self) {
        if let Some(s) = self.inner.sock.lock().unwrap().as_ref() {
            net::shutdown(s);
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop();
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn jitter(d: Duration) -> Duration {
    let mut b = [0u8; 2];
    let _ = getrandom::fill(&mut b);
    let f = 0.8 + (u16::from_le_bytes(b) as f64 / u16::MAX as f64) * 0.4;
    d.mul_f64(f)
}

pub struct Spawn {
    pub acct: Account,
    pub gen: u64,
    pub folder: String,
    pub start_delay: Duration,
    pub worker: Sender<Cmd>,
    pub shared: Arc<Shared>,
    pub events: Sender<Event>,
    pub ctx: egui::Context,
}

pub fn spawn(s: Spawn) -> Watcher {
    let inner =
        Arc::new(Inner { stop: AtomicBool::new(false), kick: AtomicBool::new(false), deadline: AtomicU64::new(0), sock: Mutex::new(None) });
    // Watchdog.
    let wd = inner.clone();
    std::thread::Builder::new()
        .name(format!("idle-watchdog-{}", s.acct.email))
        .spawn(move || {
            let mut last_wall = SystemTime::now();
            while !wd.stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_secs(2));
                let wall = SystemTime::now();
                let slept = wall.duration_since(last_wall).unwrap_or_default() > Duration::from_secs(30);
                last_wall = wall;
                let d = wd.deadline.load(Ordering::SeqCst);
                if slept || (d != 0 && now_ms() > d) {
                    if slept {
                        wd.kick.store(true, Ordering::SeqCst);
                    }
                    if let Some(sock) = wd.sock.lock().unwrap().as_ref() {
                        net::shutdown(sock);
                    }
                }
            }
        })
        .expect("spawn watchdog");

    let w = inner.clone();
    std::thread::Builder::new().name(format!("idle-{}", s.acct.email)).spawn(move || run(s, w)).expect("spawn watcher");
    Watcher { inner }
}

fn run(s: Spawn, inner: Arc<Inner>) {
    let emit = |kind: Ev| {
        let _ = s.events.send(Event { acct: s.acct.id.clone(), gen: s.gen, kind });
        s.ctx.request_repaint();
    };
    let set_push = |on: bool| {
        if s.shared.push.swap(on, Ordering::SeqCst) != on {
            emit(Ev::Push(on));
        }
    };
    let stopped = || inner.stop.load(Ordering::SeqCst);
    // Sleep in small steps so stop/kick are honoured quickly.
    let pause = |d: Duration| {
        let until = Instant::now() + d;
        while Instant::now() < until && !stopped() && !inner.kick.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(250));
        }
    };

    pause(s.start_delay);
    let mut failures: u32 = 0;
    while !stopped() {
        inner.kick.store(false, Ordering::SeqCst);
        inner.deadline.store(now_ms() + GRACE.as_millis() as u64, Ordering::SeqCst);
        let conn = net::connect(&s.acct);
        inner.deadline.store(0, Ordering::SeqCst);
        let mut c = match conn {
            Ok(c) => c,
            Err(e) => {
                set_push(false);
                let wait = match e {
                    // Leave sign-in errors to the main worker; try again much later.
                    ConnErr::Auth(_) => Duration::from_secs(15 * 60),
                    // Don't compete for scarce connections; polling covers us.
                    ConnErr::Limit(_) => Duration::from_secs(30 * 60),
                    ConnErr::Net(_) => {
                        failures += 1;
                        Duration::from_secs((5u64 << failures.min(6)).min(300))
                    }
                };
                pause(jitter(wait));
                continue;
            }
        };
        if !c.caps.idle {
            // No push on this server: the worker keeps polling at the normal rate.
            set_push(false);
            inner.stop.store(true, Ordering::SeqCst);
            return;
        }
        *inner.sock.lock().unwrap() = c.sock.try_clone().ok();
        if stopped() {
            break;
        }
        if c.sess.select(&s.folder).is_err() {
            *inner.sock.lock().unwrap() = None;
            pause(jitter(Duration::from_secs(60)));
            continue;
        }
        set_push(true);
        // Catch up on anything that arrived while we were disconnected.
        let _ = s.worker.send(Cmd::Sync { folder: s.folder.clone(), more: false });
        let connected_at = Instant::now();
        loop {
            if stopped() || inner.kick.load(Ordering::SeqCst) {
                break;
            }
            let handle = match c.sess.idle() {
                Ok(h) => h,
                Err(_) => break,
            };
            inner.deadline.store(now_ms() + (RENEW + GRACE).as_millis() as u64, Ordering::SeqCst);
            let outcome = handle.wait_with_timeout(RENEW);
            inner.deadline.store(0, Ordering::SeqCst);
            while c.sess.unsolicited_responses.try_recv().is_ok() {}
            match outcome {
                Ok(WaitOutcome::TimedOut) => continue,
                Ok(WaitOutcome::MailboxChanged) => {
                    // Let bursts settle, then ask for one sync (the worker de-duplicates).
                    std::thread::sleep(Duration::from_millis(400));
                    let _ = s.worker.send(Cmd::Sync { folder: s.folder.clone(), more: false });
                }
                Err(_) => break,
            }
        }
        *inner.sock.lock().unwrap() = None;
        set_push(false);
        if connected_at.elapsed() >= HEALTHY {
            failures = 0;
        }
        if !inner.kick.load(Ordering::SeqCst) && !stopped() {
            failures += 1;
            pause(jitter(Duration::from_secs((5u64 << failures.min(6)).min(300))));
        }
    }
    s.shared.push.store(false, Ordering::SeqCst);
    inner.stop.store(true, Ordering::SeqCst);
}
