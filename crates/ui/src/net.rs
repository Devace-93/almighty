//! Small platform seam: WebSocket and sleep differ between the browser
//! (wasm) and native targets. Everything else in the UI is shared.

use futures_channel::mpsc::UnboundedReceiver;
use futures_util::{FutureExt, SinkExt, StreamExt};

pub async fn sleep_ms(ms: u32) {
    #[cfg(target_arch = "wasm32")]
    gloo_timers::future::TimeoutFuture::new(ms).await;
    #[cfg(not(target_arch = "wasm32"))]
    tokio::time::sleep(std::time::Duration::from_millis(ms as u64)).await;
}

/// Runs one WebSocket until it closes: text frames in go to `on_text`,
/// strings from `outgoing` are sent as text frames.
pub async fn run_ws(url: String, outgoing: &mut UnboundedReceiver<String>, mut on_text: impl FnMut(String)) {
    #[cfg(target_arch = "wasm32")]
    {
        use gloo_net::websocket::{futures::WebSocket, Message};
        let Ok(ws) = WebSocket::open(&url) else { return };
        let (mut tx, mut rx) = ws.split();
        loop {
            futures_util::select! {
                msg = rx.next().fuse() => match msg {
                    Some(Ok(Message::Text(t))) => on_text(t),
                    Some(Ok(_)) => {}
                    _ => break,
                },
                out = outgoing.next().fuse() => match out {
                    Some(t) => { if tx.send(Message::Text(t)).await.is_err() { break; } }
                    None => break,
                },
            }
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        use tokio_tungstenite::tungstenite::Message;
        let Ok((ws, _)) = tokio_tungstenite::connect_async(url.as_str()).await else { return };
        let (mut tx, mut rx) = ws.split();
        loop {
            futures_util::select! {
                msg = rx.next().fuse() => match msg {
                    Some(Ok(Message::Text(t))) => on_text(t.to_string()),
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => {}
                },
                out = outgoing.next().fuse() => match out {
                    Some(t) => { if tx.send(Message::Text(t.into())).await.is_err() { break; } }
                    None => break,
                },
            }
        }
    }
}

/// Server the UI talks to. In the browser: same host as the page, port 8092
/// (the page itself is served by `dx` during development).
pub fn default_server() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(location) = web_sys::window().map(|w| w.location()) {
            // Served by the server itself: same origin. Served by `dx` while developing: the server is on 8092.
            let dev = location.port().is_ok_and(|p| p == "8093");
            match (dev, location.origin(), location.hostname()) {
                (false, Ok(origin), _) if origin.starts_with("http") => return origin,
                (_, _, Ok(host)) if !host.is_empty() => return format!("http://{host}:8092"),
                _ => {}
            }
        }
    }
    // Native builds follow the same variable as the CLI.
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(url) = std::env::var("ALMIGHTY_URL").ok().filter(|u| !u.is_empty()) {
        return url.trim_end_matches('/').to_string();
    }
    "http://127.0.0.1:8092".to_string()
}

/// Empties a text field by element id. JavaScript is confined to this file:
/// an uncontrolled field cannot be cleared from Rust without replacing the
/// element, which would close the on-screen keyboard.
pub async fn clear_field(id: &str) -> Result<(), dioxus::document::EvalError> {
    let script = format!("const e = document.getElementById('{id}'); if (e) e.value = ''; return true;");
    dioxus::document::eval(&script).await.map(|_| ())
}

/// Empties the inputs matched by a fixed CSS selector (never built from user text).
pub fn clear_inputs(selector: &'static str) {
    let script = format!("document.querySelectorAll({selector:?}).forEach(e => e.value = '');");
    let _ = dioxus::document::eval(&script);
}

/// Small persistent key/value store for the client's own settings (the
/// workspace list). Browser: localStorage. Desktop: a private file in the
/// user's configuration directory. Phones: not persisted yet.
pub fn storage_get(key: &str) -> Option<String> {
    #[cfg(target_arch = "wasm32")]
    {
        return web_sys::window()?.local_storage().ok()??.get_item(key).ok()?;
    }
    #[cfg(not(any(target_arch = "wasm32", target_os = "android", target_os = "ios")))]
    {
        return std::fs::read_to_string(storage_path(key)?).ok();
    }
    #[allow(unreachable_code)]
    {
        let _ = key;
        None
    }
}

pub fn storage_set(key: &str, value: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(Ok(Some(storage))) = web_sys::window().map(|w| w.local_storage()) {
            let _ = storage.set_item(key, value);
        }
    }
    #[cfg(not(any(target_arch = "wasm32", target_os = "android", target_os = "ios")))]
    {
        let Some(path) = storage_path(key) else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            // It holds sign-in tokens: private from the moment it exists.
            if let Ok(mut f) = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&path) {
                let _ = f.write_all(value.as_bytes());
            }
        }
        #[cfg(not(unix))]
        {
            let _ = std::fs::write(&path, value);
        }
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let _ = (key, value);
    }
}

#[cfg(not(any(target_arch = "wasm32", target_os = "android", target_os = "ios")))]
fn storage_path(key: &str) -> Option<std::path::PathBuf> {
    Some(dirs::config_dir()?.join("almighty").join(format!("{key}.json")))
}

// Clipboard. On the web the page's own clipboard API is the only option.
// Desktop webviews do not grant it reliably (WebView2 serves the app from a
// non-secure origin; WebKitGTK refuses reads), so desktop builds go to the
// system clipboard directly.

/// Copies the current text selection to the clipboard.
#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
pub async fn copy_selection() {
    let _ = dioxus::document::eval(
        "const t = window.getSelection().toString(); if (t) { await navigator.clipboard.writeText(t); } return true;",
    )
    .await;
}

/// Text on the clipboard, if the platform lets the page read it.
#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
pub async fn read_clipboard() -> Option<String> {
    let value = dioxus::document::eval("try { return await navigator.clipboard.readText(); } catch (e) { return null; }")
        .await
        .ok()?;
    value.as_str().map(str::to_string).filter(|t| !t.is_empty())
}

/// The text currently selected in the page, if any.
pub async fn selected_text() -> Option<String> {
    let value = dioxus::document::eval("return window.getSelection().toString();").await.ok()?;
    value.as_str().map(str::to_string).filter(|t| !t.is_empty())
}

/// Copies the current text selection to the clipboard.
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub async fn copy_selection() {
    let Some(text) = selected_text().await else { return };
    if let Ok(mut clipboard) = arboard::Clipboard::new() {
        let _ = clipboard.set_text(text);
    }
}

/// Text on the clipboard, if there is any.
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub async fn read_clipboard() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok().filter(|t| !t.is_empty())
}

// Linux: the window is GTK, so its clipboard works on both X11 and Wayland.
// These run on the GTK main thread, like the rest of the desktop UI.

/// Copies the current text selection to the clipboard.
#[cfg(target_os = "linux")]
pub async fn copy_selection() {
    let Some(text) = selected_text().await else { return };
    gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD).set_text(&text);
}

/// Text on the clipboard, if there is any.
#[cfg(target_os = "linux")]
pub async fn read_clipboard() -> Option<String> {
    let (tx, rx) = futures_channel::oneshot::channel();
    gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD).request_text(move |_, text| {
        let _ = tx.send(text.map(str::to_string));
    });
    rx.await.ok().flatten().filter(|t| !t.is_empty())
}

/// Shows a system notification. Best effort: a platform or a person may
/// refuse it, and that is not an error.
pub fn notify(title: &str, body: &str) {
    #[cfg(all(not(target_arch = "wasm32"), any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let (title, body) = (title.to_string(), body.to_string());
        // Off the UI thread: the notification service may take its time.
        std::thread::spawn(move || {
            let _ = notify_rust::Notification::new().appname("almighty").summary(&title).body(&body).show();
        });
    }
    #[cfg(target_arch = "wasm32")]
    {
        // The page asks for permission the first time; text is passed as data, never as script.
        let args = serde_json::json!([title, body]).to_string();
        let script = format!(
            "const [t, b] = {args}; if ('Notification' in window) {{              if (Notification.permission === 'granted') {{ new Notification(t, {{ body: b }}); }}              else if (Notification.permission !== 'denied') {{ Notification.requestPermission().then(p => {{ if (p === 'granted') new Notification(t, {{ body: b }}); }}); }} }}"
        );
        let _ = dioxus::document::eval(&script);
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let _ = (title, body);
    }
}

/// Whether this build can sign in through an identity provider.
pub const HAS_SSO: bool = cfg!(any(target_arch = "wasm32", target_os = "linux", target_os = "macos", target_os = "windows"));

/// Browser: leaves the page for the provider; the server sends the browser
/// back to this page with a one-time code (see `sso_returned`).
#[cfg(target_arch = "wasm32")]
pub async fn sso_code(server: &str) -> Result<String, String> {
    let window = web_sys::window().ok_or("no window")?;
    let location = window.location();
    let here = format!("{}{}", location.origin().unwrap_or_default(), location.pathname().unwrap_or_default());
    if let Ok(Some(storage)) = window.session_storage() {
        let _ = storage.set_item("almighty-sso-server", server);
    }
    location
        .set_href(&format!("{server}/api/auth/oidc/start?device=web&redirect={}", String::from(js_sys::encode_uri_component(&here))))
        .map_err(|_| "cannot leave the page".to_string())?;
    // The page is going away; the sign-in continues when it comes back.
    Err(String::new())
}

/// Browser: the server and one-time code of a sign-in that just came back,
/// removed from the address bar.
#[cfg(target_arch = "wasm32")]
pub fn sso_returned() -> Option<(String, String)> {
    let window = web_sys::window()?;
    let location = window.location();
    let search = location.search().ok()?;
    let code = search.trim_start_matches('?').split('&').find_map(|kv| kv.strip_prefix("almighty_code="))?.to_string();
    let storage = window.session_storage().ok()??;
    let server = storage.get_item("almighty-sso-server").ok()??;
    let _ = storage.remove_item("almighty-sso-server");
    if let (Ok(history), Ok(path)) = (window.history(), location.pathname()) {
        let _ = history.replace_state_with_url(&js_sys::wasm_bindgen::JsValue::NULL, "", Some(&path));
    }
    Some((server, code))
}

#[cfg(not(target_arch = "wasm32"))]
pub fn sso_returned() -> Option<(String, String)> {
    None
}

/// Desktop: opens the system browser for the provider and waits on a
/// loopback port for the server to send the one-time code back.
#[cfg(all(not(target_arch = "wasm32"), any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub async fn sso_code(server: &str) -> Result<String, String> {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    crate::open_external(&format!("{server}/api/auth/oidc/start?device=desktop&redirect=http://127.0.0.1:{port}/"));
    // Five minutes to finish at the provider.
    for _ in 0..1500 {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let code = request
                    .split_whitespace()
                    .nth(1)
                    .and_then(|path| path.split(['?', '&']).find_map(|kv| kv.strip_prefix("almighty_code=")))
                    .map(str::to_string);
                let page = "<!doctype html><meta charset=utf-8><title>almighty</title><p style=\"font:16px system-ui;padding:24px\">Signed in. You can close this tab and go back to almighty.</p>";
                let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}", page.len()).as_bytes());
                if let Some(code) = code {
                    return Ok(code);
                }
                // A browser asking for a favicon, or a stray request: keep waiting.
            }
            Err(_) => sleep_ms(200).await,
        }
    }
    Err("The sign-in was not finished in time.".into())
}

#[cfg(any(target_os = "android", target_os = "ios"))]
pub async fn sso_code(_server: &str) -> Result<String, String> {
    Err("Sign-in through an identity provider is not available on this device yet.".into())
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

/// `2026-11-30` (what a date field gives) to Unix seconds at 00:00 UTC.
pub fn date_to_unix(date: &str) -> Option<u64> {
    let mut parts = date.trim().split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, d) = (parts.next()??, parts.next()??, parts.next()??);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || !(1970..=9999).contains(&y) {
        return None;
    }
    u64::try_from(days_from_civil(y, m, d) * 86_400).ok()
}

/// Unix seconds to `2026-11-30` (UTC).
pub fn unix_to_date(secs: u64) -> String {
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod date_tests {
    use super::*;

    #[test]
    fn dates_round_trip() {
        assert_eq!(date_to_unix("1970-01-01"), Some(0));
        assert_eq!(date_to_unix("2026-11-21"), Some(1_795_219_200));
        assert_eq!(unix_to_date(1_795_219_200 + 86_399), "2026-11-21");
        assert_eq!(unix_to_date(date_to_unix("2028-02-29").unwrap()), "2028-02-29");
        assert_eq!(date_to_unix("2026-13-01"), None);
        assert_eq!(date_to_unix("tomorrow"), None);
    }
}
