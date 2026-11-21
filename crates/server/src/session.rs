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
    pub fn spawn(id: String, req: CreateSession, server_url: &str) -> Result<Arc<Session>> {
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
            .or_else(|| crate::home_dir())
            .unwrap_or_else(|| ".".into());
        cmd.cwd(&cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        // Lets a process inside the session (an agent CLI, a hook) find its way back.
        cmd.env("ALMIGHTY_SESSION", &id);
        cmd.env("ALMIGHTY_URL", server_url);
        for (k, v) in &req.env {
            cmd.env(k, v);
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
                            parser.process(&buf[..n]);
                            let replies = std::mem::take(&mut parser.callbacks_mut().replies);
                            let _ = s.out_tx.send(buf[..n].to_vec());
                            drop(parser);
                            if !replies.is_empty() {
                                if let Some(w) = s.writer.lock().unwrap().as_mut() {
                                    let _ = w.write_all(&replies).and_then(|_| w.flush());
                                }
                            }
                            s.bump();
                        }
                    }
                }
                // All output is in the emulator; now wait for the exit code so
                // "exited" is never visible before the last bytes.
                let _ = exit_rx.recv_timeout(Duration::from_secs(5));
                s.exited.store(true, Ordering::SeqCst);
                s.bump();
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
        }
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
        }
    }

    /// Styled grid for clients that draw the terminal themselves.
    pub fn grid(&self) -> Grid {
        let p = self.parser.lock().unwrap();
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
            cursor_visible: !s.hide_cursor(),
            title: p.callbacks().title.clone(),
            exited: self.exited(),
            exit_code: self.exit_code(),
            locked: self.locked.load(Ordering::SeqCst),
            lines: out,
        }
    }
}

/// Side effects of escape sequences that are not part of the screen grid.
#[derive(Debug, Default)]
pub struct TermEvents {
    pub title: String,
    /// Replies owed to the application (device status reports). A headless
    /// emulator has to answer these itself: ConPTY on Windows opens every
    /// session with `ESC[6n` and stalls until it gets the cursor position.
    pub replies: Vec<u8>,
}

impl vt100::Callbacks for TermEvents {
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
    pub lines: Vec<Vec<Run>>,
}
