//! A terminal session: a PTY owned by the server plus a headless emulator.
//! The process outlives clients; any number of clients attach to it.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, watch};

use crate::keys::Modes;

/// Agent input is refused for this long after a person typed in the session.
pub const HUMAN_PRIORITY_WINDOW: Duration = Duration::from_secs(3);
const SCROLLBACK_LINES: usize = 5000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Actor {
    #[default]
    Human,
    Agent,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
    pub command: Vec<String>,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
    pub created_at: u64,
    pub exited: bool,
    pub exit_code: Option<u32>,
    pub locked: bool,
    pub agents: usize,
    /// `user:<name>` or `agent:<session>`.
    pub created_by: String,
    /// Still running, but taken out of the default session list.
    pub background: bool,
    /// How many inline images the session has shown so far.
    pub media: u64,
}

/// Who started a session; decides which agent may drive it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Creator {
    User(String),
    Agent(String),
}

impl Creator {
    pub fn label(&self) -> String {
        match self {
            Creator::User(name) => format!("user:{name}"),
            Creator::Agent(session) => format!("agent:{session}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Read,
    Write,
}

/// What `Session::spawn` needs besides the request itself.
pub struct SpawnContext {
    pub server_url: String,
    /// Token for the process inside the session (hooks, MCP, CLI).
    pub agent_token: Option<String>,
    pub created_by: Creator,
    /// Called whenever something a client lists has changed (e.g. exit).
    pub on_change: Box<dyn Fn() + Send + Sync>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentNode {
    pub agent_id: String,
    pub agent_type: String,
    pub parent: Option<String>,
    pub description: Option<String>,
    pub status: String, // running | done
    pub transcript_path: Option<String>,
    pub started_at: u64,
    pub ended_at: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct CreateSession {
    pub command: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug)]
pub enum InputError {
    Exited,
    Locked,
    HumanActive,
    Io(std::io::Error),
}

pub struct Session {
    pub id: String,
    pub title: String,
    pub command: Vec<String>,
    pub cwd: String,
    pub created_at: u64,
    /// Emulator state. Also the lock that orders "process output" against
    /// "snapshot + subscribe", so an attaching client never misses bytes.
    parser: Mutex<vt100::Parser<TermEvents>>,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    killer: Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>,
    out_tx: broadcast::Sender<Vec<u8>>,
    rev: AtomicU64,
    rev_tx: watch::Sender<u64>,
    exited: AtomicBool,
    exit_code: Mutex<Option<u32>>,
    locked: AtomicBool,
    last_human_input: Mutex<Option<Instant>>,
    pub agents: Mutex<Vec<AgentNode>>,
    /// Raw hook payloads, newest last (bounded), for debugging adapters.
    pub hook_events: Mutex<Vec<serde_json::Value>>,
    /// Agent-tool calls seen but not yet matched to a SubagentStart.
    pub pending_spawns: Mutex<Vec<PendingSpawn>>,
    pub created_by: Creator,
    /// Access given by a person to agents of other sessions, by session id.
    pub grants: Mutex<HashMap<String, Scope>>,
    on_change: Box<dyn Fn() + Send + Sync>,
    background: AtomicBool,
}

#[derive(Debug, Clone)]
pub struct PendingSpawn {
    pub subagent_type: String,
    pub description: Option<String>,
    pub parent: Option<String>,
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Session {
    pub fn spawn(id: String, req: CreateSession, ctx: SpawnContext) -> Result<Arc<Session>> {
        anyhow::ensure!(!req.command.is_empty(), "command is empty");
        let cols = req.cols.unwrap_or(120).clamp(2, 500);
        let rows = req.rows.unwrap_or(32).clamp(2, 300);
        let pair = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("openpty")?;

        let mut cmd = CommandBuilder::new(&req.command[0]);
        cmd.args(&req.command[1..]);
        let cwd = req
            .cwd
            .clone()
            .or_else(crate::home_dir)
            .unwrap_or_else(|| ".".into());
        cmd.cwd(&cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        // Lets a process inside the session (an agent CLI, a hook) find its way back.
        cmd.env("ALMIGHTY_SESSION", &id);
        cmd.env("ALMIGHTY_URL", &ctx.server_url);
        for (k, v) in &req.env {
            cmd.env(k, v);
        }
        // Set last: a caller cannot swap in another identity through `env`.
        match &ctx.agent_token {
            Some(token) => cmd.env("ALMIGHTY_TOKEN", token),
            None => cmd.env_remove("ALMIGHTY_TOKEN"),
        }

        let mut child = pair.slave.spawn_command(cmd).context("spawn")?;
        drop(pair.slave);
        let killer = child.clone_killer();
        let mut reader = pair.master.try_clone_reader().context("pty reader")?;
        let writer = pair.master.take_writer().context("pty writer")?;

        let (out_tx, _) = broadcast::channel(1024);
        let (rev_tx, _) = watch::channel(0u64);
        let session = Arc::new(Session {
            title: req.title.unwrap_or_else(|| req.command.join(" ")),
            command: req.command,
            cwd,
            created_at: now_secs(),
            parser: Mutex::new(vt100::Parser::new_with_callbacks(nz(rows), nz(cols), SCROLLBACK_LINES, TermEvents::default())),
            writer: Mutex::new(Some(writer)),
            master: Mutex::new(Some(pair.master)),
            killer: Mutex::new(killer),
            out_tx,
            rev: AtomicU64::new(0),
            rev_tx,
            exited: AtomicBool::new(false),
            exit_code: Mutex::new(None),
            locked: AtomicBool::new(false),
            last_human_input: Mutex::new(None),
            agents: Mutex::new(Vec::new()),
            hook_events: Mutex::new(Vec::new()),
            pending_spawns: Mutex::new(Vec::new()),
            created_by: ctx.created_by,
            grants: Mutex::new(HashMap::new()),
            on_change: ctx.on_change,
            background: AtomicBool::new(false),
            id,
        });

        let (exit_tx, exit_rx) = std::sync::mpsc::channel::<()>();
        // PTY reads are blocking: one OS thread per session.
        let s = session.clone();
        std::thread::Builder::new()
            .name(format!("pty-{}", s.id))
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let mut parser = s.parser.lock().unwrap();
                            let media_before = parser.callbacks().media_seq;
                            parser.process(&buf[..n]);
                            let new_media = parser.callbacks().media_seq != media_before;
                            let replies = std::mem::take(&mut parser.callbacks_mut().replies);
                            let _ = s.out_tx.send(buf[..n].to_vec());
                            drop(parser);
                            if !replies.is_empty() {
                                if let Some(w) = s.writer.lock().unwrap().as_mut() {
                                    let _ = w.write_all(&replies).and_then(|_| w.flush());
                                }
                            }
                            s.bump();
                            if new_media {
                                (s.on_change)();
                            }
                        }
                    }
                }
                // All output is in the emulator; now wait for the exit code so
                // "exited" is never visible before the last bytes.
                let _ = exit_rx.recv_timeout(Duration::from_secs(5));
                s.exited.store(true, Ordering::SeqCst);
                s.bump();
                (s.on_change)();
            })
            .context("pty thread")?;

        // The child is awaited on its own thread: on Windows (ConPTY) the
        // reader does not see EOF when the process exits, only once the
        // master side is closed.
        let s = session.clone();
        std::thread::Builder::new()
            .name(format!("wait-{}", s.id))
            .spawn(move || {
                let code = child.wait().ok().map(|st| st.exit_code());
                *s.exit_code.lock().unwrap() = code;
                if cfg!(windows) {
                    // Let ConPTY flush its final repaint before closing it.
                    std::thread::sleep(Duration::from_millis(150));
                    s.writer.lock().unwrap().take();
                    s.master.lock().unwrap().take();
                }
                let _ = exit_tx.send(());
            })
            .context("wait thread")?;

        Ok(session)
    }

    fn bump(&self) {
        let v = self.rev.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.rev_tx.send(v);
    }

    pub fn info(&self) -> SessionInfo {
        let (rows, cols) = size(self.parser.lock().unwrap().screen());
        SessionInfo {
            id: self.id.clone(),
            title: self.title.clone(),
            command: self.command.clone(),
            cwd: self.cwd.clone(),
            cols,
            rows,
            created_at: self.created_at,
            exited: self.exited(),
            exit_code: *self.exit_code.lock().unwrap(),
            locked: self.locked.load(Ordering::SeqCst),
            agents: self.agents.lock().unwrap().len(),
            created_by: self.created_by.label(),
            background: self.background.load(Ordering::SeqCst),
            media: self.parser.lock().unwrap().callbacks().media_seq,
        }
    }

    /// Inline images, oldest first: id, name, type and size.
    pub fn media_list(&self) -> Vec<serde_json::Value> {
        let parser = self.parser.lock().unwrap();
        parser
            .callbacks()
            .media
            .iter()
            .map(|m| serde_json::json!({ "id": m.id, "name": m.name, "mime": m.mime, "size": m.data.len() }))
            .collect()
    }

    pub fn media_get(&self, id: u64) -> Option<Media> {
        self.parser.lock().unwrap().callbacks().media.iter().find(|m| m.id == id).cloned()
    }

    pub fn background(&self) -> bool {
        self.background.load(Ordering::SeqCst)
    }

    pub fn set_background(&self, background: bool) {
        self.background.store(background, Ordering::SeqCst);
    }

    pub fn exited(&self) -> bool {
        self.exited.load(Ordering::SeqCst)
    }

    pub fn exit_code(&self) -> Option<u32> {
        *self.exit_code.lock().unwrap()
    }

    pub fn set_locked(&self, locked: bool) {
        self.locked.store(locked, Ordering::SeqCst);
        self.bump();
    }

    pub fn watch_rev(&self) -> watch::Receiver<u64> {
        self.rev_tx.subscribe()
    }

    pub fn modes(&self) -> Modes {
        let p = self.parser.lock().unwrap();
        let s = p.screen();
        Modes {
            application_cursor: s.application_cursor(),
            application_keypad: s.application_keypad(),
            bracketed_paste: s.bracketed_paste(),
        }
    }

    /// Escape sequences that redraw the current screen, plus a live stream of
    /// everything after it. Taken under one lock so nothing is lost or doubled.
    pub fn attach(&self) -> (Vec<u8>, broadcast::Receiver<Vec<u8>>) {
        let parser = self.parser.lock().unwrap();
        // state_formatted covers contents, attributes, cursor and input modes.
        let snapshot = parser.screen().state_formatted().into_bytes();
        let rx = self.out_tx.subscribe();
        (snapshot, rx)
    }

    pub fn write_input(&self, bytes: &[u8], actor: Actor) -> Result<(), InputError> {
        if self.exited() {
            return Err(InputError::Exited);
        }
        match actor {
            Actor::Human => *self.last_human_input.lock().unwrap() = Some(Instant::now()),
            Actor::Agent => {
                if self.locked.load(Ordering::SeqCst) {
                    return Err(InputError::Locked);
                }
                let last = *self.last_human_input.lock().unwrap();
                if last.is_some_and(|t| t.elapsed() < HUMAN_PRIORITY_WINDOW) {
                    return Err(InputError::HumanActive);
                }
            }
        }
        let mut guard = self.writer.lock().unwrap();
        let Some(w) = guard.as_mut() else { return Err(InputError::Exited) };
        w.write_all(bytes).and_then(|_| w.flush()).map_err(InputError::Io)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let cols = cols.clamp(2, 500);
        let rows = rows.clamp(2, 300);
        let mut parser = self.parser.lock().unwrap();
        if let Some(master) = self.master.lock().unwrap().as_ref() {
            master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }).context("resize pty")?;
        }
        parser.set_size(nz(rows), nz(cols));
        drop(parser);
        self.bump();
        Ok(())
    }

    pub fn kill(&self) {
        let _ = self.killer.lock().unwrap().kill();
    }

    /// Rendered screen as plain text (what a person would read), trailing
    /// blank lines removed.
    pub fn screen_text(&self) -> ScreenText {
        let p = self.parser.lock().unwrap();
        let s = p.screen();
        let (rows, cols) = size(s);
        let (cr, cc) = s.cursor_position();
        ScreenText {
            text: s.contents().trim_end_matches('\n').to_string(),
            cursor_row: cr,
            cursor_col: cc,
            rows,
            cols,
            title: p.callbacks().title.clone(),
            alternate_screen: s.alternate_screen(),
            exited: self.exited(),
            exit_code: self.exit_code(),
            rev: self.rev.load(Ordering::SeqCst),
            scrollback: None,
            scrollback_len: s.scrollback_len(),
        }
    }

    /// Up to `max_lines` of history above the screen, oldest first.
    pub fn scrollback_text(&self, max_lines: usize) -> String {
        let mut p = self.parser.lock().unwrap();
        let (rows, cols) = size(p.screen());
        let total = p.screen().scrollback_len();
        let mut lines: Vec<String> = Vec::new();
        // Page through the history from the oldest line down to the screen.
        let mut offset = total.min(max_lines.div_ceil(rows as usize).saturating_mul(rows as usize).max(max_lines));
        while offset > 0 {
            p.screen_mut().set_scrollback(offset);
            let take = (rows as usize).min(offset);
            lines.extend(p.screen().rows(0, cols).take(take).map(|l| l.trim_end().to_string()));
            offset -= take;
        }
        p.screen_mut().set_scrollback(0);
        let skip = lines.len().saturating_sub(max_lines);
        lines[skip..].join("\n")
    }

    /// Styled grid for clients that draw the terminal themselves.
    pub fn grid(&self) -> Grid {
        self.grid_at(0)
    }

    /// The grid as seen `offset` lines up in the history (0 = live screen).
    /// The view is per caller: the session itself is never left scrolled.
    pub fn grid_at(&self, offset: usize) -> Grid {
        let mut p = self.parser.lock().unwrap();
        let scrollback_len = p.screen().scrollback_len();
        let offset = offset.min(scrollback_len);
        p.screen_mut().set_scrollback(offset);
        let grid = Self::build_grid(&p, self, offset, scrollback_len);
        p.screen_mut().set_scrollback(0);
        grid
    }

    fn build_grid(p: &vt100::Parser<TermEvents>, this: &Session, offset: usize, scrollback_len: usize) -> Grid {
        let s = p.screen();
        let (rows, cols) = size(s);
        let (cr, cc) = s.cursor_position();
        let mut out = Vec::with_capacity(rows as usize);
        for r in 0..rows {
            let mut runs: Vec<Run> = Vec::new();
            for c in 0..cols {
                let Some(cell) = s.cell(r, c) else { continue };
                if cell.is_wide_continuation() {
                    continue;
                }
                let style = Style {
                    fg: color(cell.fgcolor()),
                    bg: color(cell.bgcolor()),
                    bold: cell.bold(),
                    italic: cell.italic(),
                    underline: cell.underline(),
                    inverse: cell.inverse(),
                };
                let text = if cell.has_contents() { cell.contents().to_string() } else { " ".to_string() };
                match runs.last_mut() {
                    Some(last) if last.style == style => last.text.push_str(&text),
                    _ => runs.push(Run { text, style }),
                }
            }
            // Drop trailing unstyled blanks to keep frames small.
            if let Some(last) = runs.last_mut() {
                if last.style == Style::default() {
                    let trimmed = last.text.trim_end().len();
                    last.text.truncate(trimmed);
                    if last.text.is_empty() {
                        runs.pop();
                    }
                }
            }
            out.push(runs);
        }
        Grid {
            rows,
            cols,
            cursor_row: cr,
            cursor_col: cc,
            // The cursor belongs to the live screen, not to the history.
            cursor_visible: !s.hide_cursor() && offset == 0,
            title: p.callbacks().title.clone(),
            exited: this.exited(),
            exit_code: this.exit_code(),
            locked: this.locked.load(Ordering::SeqCst),
            offset,
            scrollback_len,
            lines: out,
        }
    }
}

/// Side effects of escape sequences that are not part of the screen grid.
#[derive(Debug, Default)]
pub struct TermEvents {
    pub title: String,
    /// Images the application printed inline (iTerm2 `OSC 1337 ; File=`).
    pub media: Vec<Media>,
    media_seq: u64,
    /// Replies owed to the application (device status reports). A headless
    /// emulator has to answer these itself: ConPTY on Windows opens every
    /// session with `ESC[6n` and stalls until it gets the cursor position.
    pub replies: Vec<u8>,
}

/// An image printed into the terminal, kept for clients in rich mode.
#[derive(Debug, Clone)]
pub struct Media {
    pub id: u64,
    pub name: String,
    pub mime: &'static str,
    pub data: Arc<Vec<u8>>,
}

const MAX_MEDIA: usize = 40;
const MAX_MEDIA_BYTES: usize = 8 * 1024 * 1024;

/// Only real images are kept; the type comes from the bytes, not from the name.
fn image_type(data: &[u8]) -> Option<&'static str> {
    match data {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xff, 0xd8, 0xff, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        _ => None,
    }
}

impl TermEvents {
    /// `1337 ; File=name=<b64>;size=N;inline=1:<base64 data>`.
    fn inline_file(&mut self, params: &[&[u8]]) {
        use base64::Engine;
        let joined = params[1..].join(&b';');
        let Some(rest) = joined.strip_prefix(b"File=") else { return };
        let Some(colon) = rest.iter().position(|b| *b == b':') else { return };
        let (args, data) = (String::from_utf8_lossy(&rest[..colon]).to_string(), &rest[colon + 1..]);
        if !args.split(';').any(|a| a == "inline=1") || data.len() > MAX_MEDIA_BYTES * 4 / 3 + 16 {
            return;
        }
        let engine = base64::engine::general_purpose::STANDARD;
        let Ok(bytes) = engine.decode(data) else { return };
        let Some(mime) = image_type(&bytes) else { return };
        let name = args
            .split(';')
            .find_map(|a| a.strip_prefix("name="))
            .and_then(|n| engine.decode(n).ok())
            .map(|n| String::from_utf8_lossy(&n).chars().filter(|c| !c.is_control()).take(120).collect())
            .unwrap_or_else(|| "image".to_string());
        self.media_seq += 1;
        if self.media.len() == MAX_MEDIA {
            self.media.remove(0);
        }
        self.media.push(Media { id: self.media_seq, name, mime, data: Arc::new(bytes) });
    }
}

impl vt100::Callbacks for TermEvents {
    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        if params.len() >= 2 && params[0] == b"1337" {
            self.inline_file(params);
        }
    }

    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = String::from_utf8_lossy(title).chars().take(200).collect();
    }

    fn unhandled_csi(&mut self, screen: &mut vt100::Screen, i1: Option<u8>, _i2: Option<u8>, params: &[&[u16]], c: char) {
        if c != 'n' || i1.is_some() {
            return;
        }
        match params.first().and_then(|p| p.first()) {
            Some(5) => self.replies.extend_from_slice(b"\x1b[0n"),
            Some(6) => {
                let (row, col) = screen.cursor_position();
                self.replies.extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
            }
            _ => {}
        }
    }
}

fn nz(n: u16) -> std::num::NonZeroU16 {
    std::num::NonZeroU16::new(n.max(1)).unwrap()
}

fn size(s: &vt100::Screen) -> (u16, u16) {
    let (rows, cols) = s.size();
    (rows.get(), cols.get())
}

fn color(c: vt100::Color) -> Option<String> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(format!("i{i}")),
        vt100::Color::Rgb(r, g, b) => Some(format!("#{r:02x}{g:02x}{b:02x}")),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ScreenText {
    pub text: String,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub rows: u16,
    pub cols: u16,
    pub title: String,
    pub alternate_screen: bool,
    pub exited: bool,
    pub exit_code: Option<u32>,
    pub rev: u64,
    /// Lines that scrolled off the top, oldest first; only when asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scrollback: Option<String>,
    /// How many lines of history exist in total.
    pub scrollback_len: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct Style {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bg: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub bold: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub italic: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub underline: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub inverse: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Run {
    #[serde(rename = "t")]
    pub text: String,
    #[serde(flatten)]
    pub style: Style,
}

#[derive(Debug, Clone, Serialize)]
pub struct Grid {
    pub rows: u16,
    pub cols: u16,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub cursor_visible: bool,
    pub title: String,
    pub exited: bool,
    pub exit_code: Option<u32>,
    pub locked: bool,
    /// Lines scrolled up from the live screen in this view.
    pub offset: usize,
    pub scrollback_len: usize,
    pub lines: Vec<Vec<Run>>,
}
