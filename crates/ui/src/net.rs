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
        if let Some(host) = web_sys::window().and_then(|w| w.location().hostname().ok()) {
            if !host.is_empty() {
                return format!("http://{host}:8092");
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

/// Empties a text field by element id. The one place the UI runs JavaScript:
/// an uncontrolled field cannot be cleared from Rust without replacing the
/// element, which would close the on-screen keyboard.
pub async fn clear_field(id: &str) -> Result<(), dioxus::document::EvalError> {
    let script = format!("const e = document.getElementById('{id}'); if (e) e.value = ''; return true;");
    dioxus::document::eval(&script).await.map(|_| ())
}
