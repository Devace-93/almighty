//! `almighty tmux-pane %3`: shows one tmux pane in this terminal and types
//! into it, through tmux's control mode. The server runs it as the command
//! of a session when a person opens a pane; it also works by hand.
//!
//! Nothing in the pane is restarted. Leaving (the session ends, or the
//! terminal closes) only detaches from tmux.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use almighty_server::tmux::{base, is_pane_id};
use anyhow::{bail, Context, Result};

/// History lines shown above the screen when the pane is first painted.
const HISTORY: u32 = 500;

/// What the next reply block of the control channel answers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Reply {
    Ignore,
    /// The pane's text; `true` on the first paint.
    Capture(bool),
    /// Cursor and modes, sent right after a capture.
    State(bool),
}

/// Commands go to tmux and their reply kinds to the queue under one lock,
/// so replies are matched to commands in order.
struct Control {
    stdin: ChildStdin,
    pending: VecDeque<Reply>,
}

impl Control {
    fn send(&mut self, command: &str, reply: Reply) -> std::io::Result<()> {
        self.pending.push_back(reply);
        self.stdin.write_all(command.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()
    }

    fn repaint(&mut self, pane: &str, first: bool, history: bool) -> std::io::Result<()> {
        let from = if history { format!(" -S -{HISTORY}") } else { String::new() };
        self.send(&format!("capture-pane -p -e{from} -t {pane}"), Reply::Capture(first))?;
        self.send(&format!("display-message -p -t {pane} \"{STATE}\""), Reply::State(first))
    }
}

const STATE: &str = "#{cursor_x} #{cursor_y} #{cursor_flag} #{alternate_on} #{keypad_cursor_flag} #{mouse_any_flag} #{mouse_sgr_flag} #{window_panes}";

/// tmux writes bytes below space and the backslash as `\ooo` in `%output`.
pub fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        let octal = |b: u8| (b'0'..=b'7').contains(&b);
        if data[i] == b'\\' && i + 3 < data.len() && data[i + 1..i + 4].iter().all(|b| octal(*b)) {
            let n = data[i + 1..i + 4].iter().fold(0u32, |n, b| n * 8 + u32::from(b - b'0'));
            out.push(n as u8);
            i += 4;
        } else {
            out.push(data[i]);
            i += 1;
        }
    }
    out
}

/// Programs inside tmux name their window with `ESC k name ESC \`, which
/// only tmux and screen understand; other terminals would print the name.
/// Drops those sequences; keeps its place between chunks.
#[derive(Default)]
pub struct TitleFilter {
    state: u8,
}

impl TitleFilter {
    pub fn pass(&mut self, data: &[u8]) -> Vec<u8> {
        const TEXT: u8 = 0;
        const ESC: u8 = 1;
        const TITLE: u8 = 2;
        const TITLE_ESC: u8 = 3;
        let mut out = Vec::with_capacity(data.len());
        for &b in data {
            self.state = match (self.state, b) {
                (TEXT, 0x1b) => ESC,
                (TEXT, _) => {
                    out.push(b);
                    TEXT
                }
                (ESC, b'k') => TITLE,
                (ESC, _) => {
                    out.extend_from_slice(&[0x1b, b]);
                    TEXT
                }
                (TITLE, 0x1b) => TITLE_ESC,
                // A title never holds control bytes: one means it was cut short.
                (TITLE, 0x07 | b'\n' | b'\r') => TEXT,
                (TITLE, _) => TITLE,
                (TITLE_ESC, b'\\') => TEXT,
                (TITLE_ESC, 0x1b) => TITLE_ESC,
                (_, _) => TITLE,
            };
        }
        out
    }
}

/// The command that types `bytes` into the pane exactly as given.
pub fn keys_command(pane: &str, bytes: &[u8]) -> String {
    let mut cmd = format!("send-keys -H -t {pane}");
    for b in bytes {
        cmd.push_str(&format!(" {b:02x}"));
    }
    cmd
}

/// What to write to the terminal so that it shows the captured pane.
/// `state` is the reply to the `STATE` format.
pub fn paint(lines: &[Vec<u8>], state: &str, first: bool) -> Vec<u8> {
    let flags: Vec<u32> = state.split_whitespace().map(|f| f.parse().unwrap_or(0)).collect();
    let flag = |i: usize| flags.get(i).copied().unwrap_or(0);
    let mut out = Vec::new();
    if first && flag(3) == 1 {
        out.extend_from_slice(b"\x1b[?1049h");
    }
    out.extend_from_slice(b"\x1b[0m\x1b[H\x1b[2J");
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(line);
    }
    out.extend_from_slice(format!("\x1b[0m\x1b[{};{}H", flag(1) + 1, flag(0) + 1).as_bytes());
    if first {
        // Modes the program set before this terminal was looking.
        if flag(4) == 1 {
            out.extend_from_slice(b"\x1b[?1h");
        }
        if flag(5) == 1 {
            out.extend_from_slice(b"\x1b[?1000h\x1b[?1002h");
            if flag(6) == 1 {
                out.extend_from_slice(b"\x1b[?1006h");
            }
        }
    }
    out.extend_from_slice(if flag(2) == 0 { b"\x1b[?25l" } else { b"\x1b[?25h" });
    out
}

fn tmux_once(args: &[&str]) -> Option<String> {
    let base = base();
    let out = Command::new(&base[0]).args(&base[1..]).args(args).env_remove("TMUX").output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

struct Raw;

impl Drop for Raw {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[?1049l\x1b[?25h\x1b[0m\x1b[?1000l\x1b[?1002l\x1b[?1006l\x1b[?1l\r\n");
        let _ = out.flush();
    }
}

pub fn run(pane: String) -> Result<()> {
    if !is_pane_id(&pane) {
        bail!("write the pane as tmux names it, e.g. %3 (see `tmux list-panes -a`)");
    }
    let found = tmux_once(&["display-message", "-p", "-t", &pane, "#{session_id} #{alternate_on}"])
        .with_context(|| format!("there is no tmux pane {pane}"))?;
    let (session, alternate) = found.split_once(' ').unwrap_or((&found, "0"));
    let history = alternate != "1";

    let base = base();
    let mut child = Command::new(&base[0])
        .args(&base[1..])
        .args(["-C", "attach-session", "-t", session])
        .env_remove("TMUX")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("start tmux")?;
    let stdout = child.stdout.take().context("tmux output")?;
    let control = Arc::new(Mutex::new(Control { stdin: child.stdin.take().context("tmux input")?, pending: VecDeque::new() }));

    crossterm::terminal::enable_raw_mode().context("this command needs a terminal")?;
    let raw = Raw;

    control.lock().unwrap().repaint(&pane, true, history)?;

    // A pane alone in its window takes this terminal's size, the way tmux
    // sizes a window for the client used last. A pane of a split window
    // keeps its size: fitting it would move the person's other panes.
    let alone = Arc::new(AtomicBool::new(false));
    {
        let control = control.clone();
        let alone = alone.clone();
        std::thread::spawn(move || {
            let mut sent = None;
            loop {
                let now = crossterm::terminal::size().ok();
                if alone.load(Ordering::SeqCst) && now != sent {
                    if let Some((cols, rows)) = now {
                        let ok = control.lock().unwrap().send(&format!("refresh-client -C {cols}x{rows}"), Reply::Ignore);
                        if ok.is_err() {
                            return;
                        }
                    }
                    sent = now;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        });
    }
    {
        let control = control.clone();
        let pane = pane.clone();
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 512];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if control.lock().unwrap().send(&keys_command(&pane, &buf[..n]), Reply::Ignore).is_err() {
                            break;
                        }
                    }
                }
            }
            // The terminal is gone: closing tmux's input detaches the client.
            std::process::exit(0);
        });
    }

    let why = read_control(BufReader::new(stdout), &control, &pane, &alone);
    drop(raw);
    let _ = child.kill();
    let _ = child.wait();
    println!("{why}");
    Ok(())
}

/// Follows the control channel until tmux or the pane goes away; returns why.
fn read_control(mut tmux: impl BufRead, control: &Mutex<Control>, pane: &str, alone: &AtomicBool) -> String {
    let mut out = std::io::stdout();
    let mut titles = TitleFilter::default();
    let output_prefix = format!("%output {pane} ").into_bytes();
    // Lines of the reply block being read, when it answers one of our commands.
    let mut block: Option<Vec<Vec<u8>>> = None;
    let mut capture: Vec<Vec<u8>> = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        match tmux.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return "tmux went away".into(),
            Ok(_) => {}
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        if let Some(lines) = block.as_mut() {
            let ended = line.starts_with(b"%end ");
            if !ended && !line.starts_with(b"%error ") {
                lines.push(line.clone());
                continue;
            }
            let lines = block.take().unwrap_or_default();
            let reply = control.lock().unwrap().pending.pop_front().unwrap_or(Reply::Ignore);
            match reply {
                Reply::Ignore => {}
                Reply::Capture(_) | Reply::State(_) if !ended => return "the tmux pane closed".into(),
                Reply::Capture(_) => capture = lines,
                Reply::State(first) => {
                    let state = lines.first().map(|l| String::from_utf8_lossy(l).to_string()).unwrap_or_default();
                    alone.store(state.split_whitespace().nth(7) == Some("1"), Ordering::SeqCst);
                    let _ = out.write_all(&paint(&capture, &state, first));
                    let _ = out.flush();
                    capture.clear();
                }
            }
        } else if line.starts_with(b"%begin ") {
            // Flag 1: the block answers a command of this client.
            if line.ends_with(b" 1") {
                block = Some(Vec::new());
            }
        } else if let Some(data) = line.strip_prefix(output_prefix.as_slice()) {
            let _ = out.write_all(&titles.pass(&unescape(data)));
            let _ = out.flush();
        } else if line.starts_with(b"%layout-change ") || line.starts_with(b"%window-close ") || line.starts_with(b"%unlinked-window-close ") {
            // The pane may have a new size, or be gone: the repaint tells.
            if control.lock().unwrap().repaint(pane, false, false).is_err() {
                return "tmux went away".into();
            }
        } else if line.starts_with(b"%exit") {
            return "the tmux session ended".into();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescapes_tmux_output() {
        assert_eq!(unescape(br"ls\015\012"), b"ls\r\n");
        assert_eq!(unescape(br"\033[1mhi\134n"), b"\x1b[1mhi\\n");
        // Not an escape: too short, or not octal digits.
        assert_eq!(unescape(br"a\01"), br"a\01");
        assert_eq!(unescape(br"\189"), br"\189");
        assert_eq!(unescape(b"caf\xc3\xa9"), b"caf\xc3\xa9");
    }

    #[test]
    fn drops_window_titles_even_when_split_between_chunks() {
        let mut f = TitleFilter::default();
        assert_eq!(f.pass(b"a\x1bkecho\x1b\\b\x1b[1mc"), b"ab\x1b[1mc");
        assert_eq!(f.pass(b"x\x1b"), b"x");
        assert_eq!(f.pass(b"kna"), b"");
        assert_eq!(f.pass(b"me\x1b"), b"");
        assert_eq!(f.pass(b"\\y\x1b"), b"y");
        assert_eq!(f.pass(b"[2J"), b"\x1b[2J");
    }

    #[test]
    fn types_bytes_as_hex() {
        assert_eq!(keys_command("%3", b"ls\r"), "send-keys -H -t %3 6c 73 0d");
        assert_eq!(keys_command("%3", b"\x1b[A; kill-server"), "send-keys -H -t %3 1b 5b 41 3b 20 6b 69 6c 6c 2d 73 65 72 76 65 72");
    }

    #[test]
    fn paints_the_screen_and_puts_the_cursor_back() {
        let lines = [b"one".to_vec(), b"two".to_vec()];
        let got = String::from_utf8(paint(&lines, "3 1 1 0 0 0 0", true)).unwrap();
        assert_eq!(got, "\x1b[0m\x1b[H\x1b[2Jone\r\ntwo\x1b[0m\x1b[2;4H\x1b[?25h");
        // A full-screen program with the mouse on, seen for the first time.
        let got = String::from_utf8(paint(&lines, "0 0 0 1 1 1 1", true)).unwrap();
        assert!(got.starts_with("\x1b[?1049h"));
        assert!(got.ends_with("\x1b[1;1H\x1b[?1h\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?25l"));
        // Later repaints leave the modes to the program's own output.
        assert!(!String::from_utf8(paint(&lines, "0 0 1 1 1 1 1", false)).unwrap().contains("1049"));
    }
}
