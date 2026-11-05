<img src="assets/icon.svg" width="64" height="64" alt="">

# Mail

[![CI](https://github.com/MattGodwin/mail-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/MattGodwin/mail-rs/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/MattGodwin/mail-rs?include_prereleases)](https://github.com/MattGodwin/mail-rs/releases)

A fast, minimal desktop email client written in Rust. It works with any IMAP/SMTP provider
(Gmail, Outlook/Hotmail/Microsoft 365, iCloud, Yahoo, Fastmail and others), keeps all your
accounts signed in side by side, and **never modifies your messages**.

**[Download the latest release](https://github.com/MattGodwin/mail-rs/releases/latest)** for
Windows, Linux or macOS (Apple Silicon and Intel).

- Single native binary of about 9 MB. The window opens in about a third of a second.
- **Push mail:** new messages arrive within seconds over IMAP IDLE, with polling as a fallback.
  Desktop notifications appear even while the window is minimised.
- **Multiple accounts at once:** a unified inbox, every folder of every account, live status
  for each account (synced, offline, sign-in problem), pause/resume, sending aliases and a
  default sending account.
- **Essentials:** read, reply, reply-all, forward, compose and attach files (button or
  drag-and-drop). Archive, delete, move, star and mark read/unread, with **Undo**.
- **You never lose a draft.** Compose windows are saved on this computer as you type and come
  back after a restart or crash. Server drafts keep Bcc and replace earlier versions, and
  quitting waits for anything still being sent.
- Instant local filter, plus server-side search (press Enter in the filter box).
- Save or open attachments (executables are save-only), or save the original `.eml`.
- Keyboard driven (press `?` in the app).

## Leaving your email alone

- **Messages are cached byte for byte.** They're fetched with `BODY.PEEK[]` and stored as
  `.eml` files; the client never re-encodes, re-wraps or "fixes" stored mail. *Save as .eml*
  and *View source* show exactly what the server sent.
- **Opening a message is the only thing that marks it read.** Downloading or prefetching never
  changes read state on the server, and you can turn off mark-on-open in Settings.
- **Remote content never loads.** HTML mail is shown as text in the app, so tracking pixels
  never fire. *Open original in browser* opens the untouched HTML if you want it.
- **Outgoing mail is sent exactly as typed.** It's plain text; attachments and forwarded
  attachments go out unchanged.
- **Deletes and moves only touch the messages you chose.** If a server can't remove just those
  messages, Mail marks them deleted rather than expunging anything else.
- **Changes that might have half-happened are never repeated blindly.** If a save or copy is
  interrupted, Mail re-checks the server instead of creating duplicates.
- **Credentials stay in the OS keychain** (Windows Credential Manager, macOS Keychain, Linux
  keyring), never in the config file.
- **Connections are always encrypted:** implicit TLS or mandatory STARTTLS, with no plaintext
  fallback.

## Why it's fast

- **Background connections per account.** Each account has its own IMAP thread plus a push
  (IDLE) connection on the inbox, so the UI never waits on the network.
- **Prioritised commands.** Opening a message jumps ahead of syncing and prefetching.
- **Incremental sync.** After the first sync, each check fetches new UIDs and flag changes
  only (`UIDVALIDITY` is honoured), and unchanged folders cause no disk writes or redraws.
- **Instant startup.** Cached state shows immediately, then syncs in the background. Account
  start-up is staggered so many accounts don't all connect at once.
- **Prefetch and cache.** The newest small messages are downloaded in advance and open
  instantly. Everything is cached on disk after the first open.
- **Virtualised message list.** Only visible rows are laid out, so 10 or 10,000 messages
  scroll the same. The UI is immediate-mode ([egui](https://github.com/emilk/egui)) and
  repaints only when something changes.

## Adding accounts

Click **+ Account**, choose your provider and enter your address. Server settings are filled
in for you, and the dialog shows step-by-step instructions and links for the sign-in method you
pick. Choose **Other** to enter any IMAP/SMTP server by hand.

| Provider | Sign-in |
|---|---|
| Gmail | An **app password** (requires 2-Step Verification) is the default. OAuth works with your own client ID. |
| iCloud, Yahoo, Fastmail, most others | An **app password** from your provider's security settings. |
| Outlook.com / Hotmail / Microsoft 365 | **OAuth only.** Microsoft no longer accepts passwords over IMAP/SMTP. |

### OAuth client IDs (Microsoft, and optionally Google)

OAuth sign-in needs a client ID registered to *you*. It's free and only needs doing once. The
account dialog walks you through it and checks the ID's format before opening the browser.
Saving it in **Settings** pre-fills it for new accounts.

**Microsoft (Outlook.com, Hotmail, Microsoft 365)**
1. Go to <https://entra.microsoft.com> → *App registrations* → *New registration*.
2. Supported account types: *Accounts in any organizational directory and personal Microsoft accounts*.
3. Redirect URI: platform **Public client/native (mobile & desktop)**, value `http://localhost`.
4. *API permissions* → add the delegated permissions `IMAP.AccessAsUser.All`, `SMTP.Send`
   and `offline_access`.
5. Copy the *Application (client) ID* into Mail. No secret is needed.

**Google (Gmail / Workspace)**
1. Go to <https://console.cloud.google.com> → create a project → enable the **Gmail API**.
2. *OAuth consent screen*: External, add the scope `https://mail.google.com/`, and add
   yourself as a test user.
3. *Credentials* → *Create credentials* → *OAuth client ID* → **Desktop app**.
4. Copy the client ID and secret into Mail.

## Keyboard shortcuts

| Key | Action |
|---|---|
| `J` / `↓`, `K` / `↑`, `PgDn` / `PgUp` | Next / previous message |
| `C` or `N` | Compose |
| `R` / `A` / `F` | Reply / Reply all / Forward |
| `E` | Archive |
| `Del` / `Backspace` / `#` | Delete (move to Trash; in Trash, delete permanently after confirming) |
| `Ctrl+Z` | Undo the last archive / delete / move (within 5 seconds) |
| `U` / `S` | Toggle read / star |
| `/` | Focus the filter (`Enter` searches the server, `Esc` clears) |
| `F5` | Check for new mail |
| `Ctrl+Enter` | Send (in a compose window) |
| `?` | Show all shortcuts |

Inbox shortcuts never fire while a compose window or dialog has focus. Right-click a message for
*Move to*, or an account name to edit, pause or remove it.

## Building from source

You need Rust (`rustup`, stable toolchain).

```sh
cargo build --release
./target/release/mail-rs        # mail-rs.exe on Windows
cargo test
```

**Linux** also needs `libssl-dev pkg-config libxkbcommon-dev libxcb-render0-dev
libxcb-shape0-dev libxcb-xfixes0-dev`.

**Windows:** the MSVC toolchain works out of the box. With the GNU toolchain
(`x86_64-pc-windows-gnu`) you also need a full MinGW-w64 toolchain on `PATH` for `dlltool` and
`as`, for example `winget install BrechtSanders.WinLibs.POSIX.UCRT`.

## CI and releases

- **CI** (`.github/workflows/ci.yml`) runs on every push to `main` and every pull request. It
  checks formatting with `cargo fmt`, runs `cargo clippy` with warnings treated as errors, and
  builds and tests on Windows, Linux and macOS.
- **Releases** (`.github/workflows/release.yml`) build optimised binaries for Windows x64,
  Linux x64, macOS Apple Silicon and macOS Intel, then publish them with SHA-256 checksums
  and generated release notes.
- **Dependabot** keeps Rust dependencies and GitHub Actions up to date.

To cut a release:

```sh
# 1. bump `version` in Cargo.toml (e.g. 0.2.0), commit, and push to main
# 2. tag and push; the tag must match the Cargo.toml version
git tag v0.2.0
git push origin v0.2.0
```

Tags with a suffix (e.g. `v0.3.0-beta.1`) are published as pre-releases. To rebuild an existing
tag, run the *Release* workflow manually from the Actions tab.

The binaries aren't code-signed, so Windows SmartScreen and macOS Gatekeeper will warn on
first launch. On macOS, run `xattr -d com.apple.quarantine mail-rs` after extracting.

## Where things live

| What | Windows | macOS | Linux |
|---|---|---|---|
| Config (no secrets) | `%APPDATA%\mail-rs\config\` | `~/Library/Application Support/mail-rs/` | `~/.config/mail-rs/` |
| Unsent message recovery | `…\config\compose\` | `…/compose/` | `…/compose/` |
| Cache (`.eml` + indexes) | `%LOCALAPPDATA%\mail-rs\cache\` | `~/Library/Caches/mail-rs/` | `~/.cache/mail-rs/` |
| Credentials | Credential Manager (`mail-rs`) | Keychain | kernel keyring |

Removing an account deletes its cache and stored credentials. Nothing is deleted on the server.

## Code layout

| File | Purpose |
|---|---|
| `src/main.rs` | Entry point and window setup |
| `src/app.rs` | UI: panels, list, reader, compose windows, account editor, undo, exit guard |
| `src/worker.rs` | Per-account IMAP thread: prioritised queue, sync, acknowledged mutations, polling, notifications |
| `src/watcher.rs` | Push mail: IMAP IDLE connection with watchdog, backoff and sleep/resume handling |
| `src/net.rs` | Connecting and authenticating (password or XOAUTH2), error classification |
| `src/smtp.rs` | Sending over SMTP (TLS / STARTTLS) |
| `src/oauth.rs` | OAuth 2.0 authorization code + PKCE with loopback redirect, serialised token refresh |
| `src/compose.rs` | Building messages and drafts, reply/forward, identities and aliases |
| `src/drafts.rs` | Local recovery copies of unsent messages |
| `src/view.rs` | Parsing a raw message for display, attachments, link detection |
| `src/store.rs` | On-disk cache (raw `.eml` files, folder indexes) |
| `src/model.rs` | Shared types, IMAP modified UTF-7, formatting helpers |
| `src/config.rs` | Accounts, settings, provider presets and setup guides |
| `src/notify.rs` | Desktop notifications |
| `src/secrets.rs` | OS keychain access |

## Current limitations

- Push watches the inbox. Other folders refresh while you're viewing them and on a timer.
- HTML mail is rendered as text in the app; use *Open original in browser* for the full layout.
- Search uses the server's IMAP `SEARCH`; there's no local full-text index yet.
- One message is selected at a time (no multi-select yet), and there's no conversation view.
- On Windows, notifications are attributed to "Windows PowerShell" until the app gets an
  installer that registers its own notification identity.
- No PGP/S/MIME, calendar or contacts. CJK text needs a font fallback that isn't bundled yet.
