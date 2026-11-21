//! Attach the current terminal to a server session: raw bytes both ways.

use std::io::{Read, Write};

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use crate::Client;

/// Ctrl-\ detaches and leaves the session running.
const DETACH: u8 = 0x1c;

struct RawMode {
    #[cfg(windows)]
    vt: win_console::VtModes,
}

impl RawMode {
    fn enter() -> Result<Self> {
        crossterm::terminal::enable_raw_mode().context("this command needs a terminal")?;
        Ok(RawMode {
            #[cfg(windows)]
            vt: win_console::VtModes::enable(),
        })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        // Leave the alternate screen, show the cursor, reset attributes and
        // mouse/paste modes the remote application may have enabled.
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[?1049l\x1b[?25h\x1b[0m\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1l\x1b>\r\n");
        let _ = out.flush();
        #[cfg(windows)]
        self.vt.restore();
    }
}

/// The Windows console only speaks escape sequences when asked to: without
/// VT input, keys such as arrows or F1 never show up as bytes on stdin, and
/// without VT processing a legacy console prints sequences literally.
#[cfg(windows)]
mod win_console {
    type Handle = *mut core::ffi::c_void;

    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
    const DISABLE_NEWLINE_AUTO_RETURN: u32 = 0x0008;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(id: u32) -> Handle;
        fn GetConsoleMode(handle: Handle, mode: *mut u32) -> i32;
        fn SetConsoleMode(handle: Handle, mode: u32) -> i32;
    }

    fn mode(id: u32) -> Option<(Handle, u32)> {
        let mut mode = 0u32;
        // SAFETY: plain Win32 calls on this process's own standard handles.
        unsafe {
            let handle = GetStdHandle(id);
            (GetConsoleMode(handle, &mut mode) != 0).then_some((handle, mode))
        }
    }

    /// Console modes as they were before the attach, to put back on exit.
    pub struct VtModes {
        input: Option<(Handle, u32)>,
        output: Option<(Handle, u32)>,
    }

    impl VtModes {
        /// Call after raw mode is on. Failures are ignored: a console too old
        /// for VT modes keeps working for plain text.
        pub fn enable() -> Self {
            let input = mode(STD_INPUT_HANDLE);
            let output = mode(STD_OUTPUT_HANDLE);
            // SAFETY: as above.
            unsafe {
                if let Some((h, m)) = input {
                    SetConsoleMode(h, m | ENABLE_VIRTUAL_TERMINAL_INPUT);
                }
                if let Some((h, m)) = output {
                    SetConsoleMode(h, m | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN);
                }
            }
            VtModes { input, output }
        }

        pub fn restore(&self) {
            // SAFETY: as above.
            unsafe {
                // Raw mode is already off: only drop the VT input bit again.
                if let Some((h, m)) = self.input {
                    if m & ENABLE_VIRTUAL_TERMINAL_INPUT == 0 {
                        if let Some((_, now)) = mode(STD_INPUT_HANDLE) {
                            SetConsoleMode(h, now & !ENABLE_VIRTUAL_TERMINAL_INPUT);
                        }
                    }
                }
                if let Some((h, m)) = self.output {
                    SetConsoleMode(h, m);
                }
            }
        }
    }
}

pub async fn attach(client: &Client, id: &str) -> Result<()> {
    let ws_url = format!("{}/api/sessions/{id}/attach", client.url.replacen("http", "ws", 1));
    let mut req = ws_url.into_client_request()?;
    if let Some(t) = &client.token {
        req.headers_mut().insert("Authorization", format!("Bearer {t}").parse()?);
    }
    let (ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .with_context(|| format!("cannot attach to session {id}"))?;
    let (mut tx, mut rx) = ws.split();

    let raw = RawMode::enter()?;
    if let Ok((cols, rows)) = crossterm::terminal::size() {
        tx.send(Message::Text(json!({ "type": "resize", "cols": cols, "rows": rows }).to_string().into())).await?;
    }

    // stdin is blocking: read it on a thread and forward through a channel.
    let (in_tx, mut in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        // Rust's Windows console reader reports Ctrl-Z as end of input. A
        // console has no real end of input, so pass the key on instead.
        let ctrl_z_is_eof = cfg!(windows) && std::io::IsTerminal::is_terminal(&stdin);
        let mut buf = [0u8; 4096];
        while let Ok(n) = stdin.read(&mut buf) {
            let bytes = if n == 0 && ctrl_z_is_eof { vec![0x1a] } else { buf[..n].to_vec() };
            if bytes.is_empty() || in_tx.blocking_send(bytes).is_err() {
                break;
            }
        }
    });

    #[cfg(unix)]
    let mut winch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    #[cfg(not(unix))]
    let mut resize_tick = tokio::time::interval(std::time::Duration::from_millis(500));
    #[cfg(not(unix))]
    let mut last_size = crossterm::terminal::size().ok();

    let mut stdout = std::io::stdout();
    let outcome: String = loop {
        #[cfg(unix)]
        let resized = winch.recv();
        #[cfg(not(unix))]
        let resized = resize_tick.tick();

        tokio::select! {
            msg = rx.next() => match msg {
                Some(Ok(Message::Binary(bytes))) => {
                    stdout.write_all(&bytes)?;
                    stdout.flush()?;
                }
                Some(Ok(Message::Text(text))) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&text) {
                        if v["type"] == "exit" {
                            break format!("[session {id} exited, code {}]", v["code"]);
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None => break format!("[session {id} closed]"),
                Some(Err(e)) => break format!("[connection lost: {e}]"),
                Some(Ok(_)) => {}
            },
            input = in_rx.recv() => match input {
                Some(bytes) => {
                    if bytes.contains(&DETACH) {
                        break format!("[detached from {id}; reattach with: almighty attach {id}]");
                    }
                    tx.send(Message::Binary(bytes.into())).await?;
                }
                None => break "[stdin closed]".to_string(),
            },
            _ = resized => {
                if let Ok((cols, rows)) = crossterm::terminal::size() {
                    #[cfg(not(unix))]
                    {
                        if last_size == Some((cols, rows)) { continue; }
                        last_size = Some((cols, rows));
                    }
                    tx.send(Message::Text(json!({ "type": "resize", "cols": cols, "rows": rows }).to_string().into())).await?;
                }
            }
        }
    };
    drop(raw);
    eprintln!("{outcome}");
    // The stdin thread is blocked in read(); end the process instead of joining it.
    std::process::exit(0);
}
