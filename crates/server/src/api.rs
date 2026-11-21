//! HTTP + WebSocket API (milestone-0 spike).
//!
//! Auth is a single optional bearer token (`ALMIGHTY_TOKEN`); workspaces and
//! users arrive in milestone 1. The default bind address is loopback only.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::broadcast::error::RecvError;

use crate::keys::{encode_key, encode_text};
use crate::session::{Actor, CreateSession, InputError, Session};

#[derive(Debug, Clone)]
pub struct Config {
    pub addr: SocketAddr,
    pub token: Option<String>,
}

impl Config {
    pub fn from_env() -> Self {
        let addr = std::env::var("ALMIGHTY_ADDR")
            .ok()
            .and_then(|a| a.parse().ok())
            .unwrap_or_else(|| ([127, 0, 0, 1], 8092).into());
        let token = std::env::var("ALMIGHTY_TOKEN").ok().filter(|t| !t.is_empty());
        Config { addr, token }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

pub struct AppState {
    pub config: Config,
    sessions: RwLock<HashMap<String, Arc<Session>>>,
    next_id: AtomicU64,
    /// Host reachability answers for bare-domain link detection.
    probes: Mutex<HashMap<String, (bool, Instant)>>,
}

type App = Arc<AppState>;

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}

impl From<InputError> for ApiError {
    fn from(e: InputError) -> Self {
        match e {
            InputError::Exited => ApiError(StatusCode::GONE, "session has exited".into()),
            InputError::Locked => ApiError(StatusCode::CONFLICT, "session is locked by the user".into()),
            InputError::HumanActive => ApiError(
                StatusCode::CONFLICT,
                "a person is typing in this session; agent input is paused, retry shortly".into(),
            ),
            InputError::Io(e) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        }
    }
}

fn find(app: &App, id: &str) -> Result<Arc<Session>, ApiError> {
    app.sessions
        .read()
        .unwrap()
        .get(id)
        .cloned()
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no session {id}")))
}

pub fn router(config: Config) -> Router {
    let app: App = Arc::new(AppState {
        config,
        sessions: RwLock::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        probes: Mutex::new(HashMap::new()),
    });
    Router::new()
        .route("/api/health", get(|| async { Json(json!({ "ok": true })) }))
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/sessions/{id}", get(get_session).delete(close_session))
        .route("/api/sessions/{id}/screen", get(screen))
        .route("/api/sessions/{id}/input", post(input))
        .route("/api/sessions/{id}/wait", post(wait))
        .route("/api/sessions/{id}/resize", post(resize))
        .route("/api/sessions/{id}/lock", post(lock))
        .route("/api/sessions/{id}/agents", get(agents))
        .route("/api/sessions/{id}/agents/{agent_id}/transcript", get(agent_transcript))
        .route("/api/sessions/{id}/hook-events", get(hook_events))
        .route("/api/sessions/{id}/attach", get(attach_ws))
        .route("/api/sessions/{id}/grid", get(grid_ws))
        .route("/api/hooks/claude", post(claude_hook))
        .route("/api/probe", get(probe))
        .layer(middleware::from_fn_with_state(app.clone(), auth))
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(app)
}

pub async fn serve(config: Config) -> anyhow::Result<()> {
    if !config.addr.ip().is_loopback() && config.token.is_none() {
        anyhow::bail!("refusing to listen on {} without ALMIGHTY_TOKEN", config.addr);
    }
    let listener = tokio::net::TcpListener::bind(config.addr).await.map_err(|e| {
        anyhow::anyhow!(
            "cannot listen on {}: {e}. The port may be taken or reserved by the system; \
             set ALMIGHTY_ADDR (e.g. 127.0.0.1:8192) to use another one",
            config.addr
        )
    })?;
    eprintln!("almightyd listening on {}", config.url());
    axum::serve(listener, router(config)).await?;
    Ok(())
}

async fn auth(State(app): State<App>, req: Request, next: Next) -> Response {
    let Some(expected) = &app.config.token else { return next.run(req).await };
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    // Browsers cannot set headers on WebSocket requests: allow ?token= too.
    let query = req.uri().query().and_then(|q| {
        q.split('&').find_map(|kv| kv.strip_prefix("token="))
    });
    let ok = [bearer, query].into_iter().flatten().any(|t| constant_time_eq(t.as_bytes(), expected.as_bytes()));
    if ok || req.method() == axum::http::Method::OPTIONS {
        next.run(req).await
    } else {
        ApiError(StatusCode::UNAUTHORIZED, "missing or wrong token".into()).into_response()
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn list_sessions(State(app): State<App>) -> Json<Value> {
    let mut list: Vec<_> = app.sessions.read().unwrap().values().map(|s| s.info()).collect();
    list.sort_by_key(|s| s.created_at);
    Json(json!({ "sessions": list }))
}

async fn create_session(State(app): State<App>, Json(req): Json<CreateSession>) -> Result<Json<Value>, ApiError> {
    let id = format!("s{}", app.next_id.fetch_add(1, Ordering::SeqCst));
    let session = Session::spawn(id.clone(), req, &app.config.url()).map_err(|e| bad(format!("{e:#}")))?;
    let info = session.info();
    app.sessions.write().unwrap().insert(id, session);
    Ok(Json(json!(info)))
}

async fn get_session(State(app): State<App>, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!(find(&app, &id)?.info())))
}

async fn close_session(State(app): State<App>, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    let session = find(&app, &id)?;
    session.kill();
    app.sessions.write().unwrap().remove(&id);
    Ok(Json(json!({ "closed": id })))
}

async fn screen(State(app): State<App>, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!(find(&app, &id)?.screen_text())))
}

#[derive(Deserialize)]
struct InputReq {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    keys: Vec<String>,
    /// Who is typing. HTTP callers are agents unless they say otherwise.
    #[serde(default = "agent")]
    actor: Actor,
}

fn agent() -> Actor {
    Actor::Agent
}

fn encode_input(session: &Session, text: Option<&str>, keys: &[String]) -> Result<Vec<u8>, ApiError> {
    let modes = session.modes();
    let mut bytes = Vec::new();
    if let Some(t) = text {
        bytes.extend(encode_text(t, modes));
    }
    for k in keys {
        bytes.extend(encode_key(k, modes).map_err(|e| bad(e.to_string()))?);
    }
    Ok(bytes)
}

async fn input(State(app): State<App>, Path(id): Path<String>, Json(req): Json<InputReq>) -> Result<Json<Value>, ApiError> {
    let session = find(&app, &id)?;
    let bytes = encode_input(&session, req.text.as_deref(), &req.keys)?;
    if bytes.is_empty() {
        return Err(bad("nothing to send: give `text` and/or `keys`"));
    }
    session.write_input(&bytes, req.actor)?;
    Ok(Json(json!({ "sent_bytes": bytes.len() })))
}

#[derive(Deserialize)]
struct WaitReq {
    /// Regex searched in the rendered screen text.
    #[serde(default)]
    pattern: Option<String>,
    /// Succeed once the screen has not changed for this long.
    #[serde(default)]
    idle_ms: Option<u64>,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
}

fn default_timeout() -> u64 {
    30_000
}

async fn wait(State(app): State<App>, Path(id): Path<String>, Json(req): Json<WaitReq>) -> Result<Json<Value>, ApiError> {
    let session = find(&app, &id)?;
    // Multi-line: `^` and `$` match at line boundaries of the screen.
    let re = req
        .pattern
        .as_deref()
        .map(|p| regex::RegexBuilder::new(p).multi_line(true).build())
        .transpose()
        .map_err(|e| bad(e.to_string()))?;
    if re.is_none() && req.idle_ms.is_none() {
        return Err(bad("give `pattern` and/or `idle_ms`"));
    }
    let deadline = tokio::time::Instant::now() + Duration::from_millis(req.timeout_ms.min(600_000));
    let mut rev = session.watch_rev();
    let reason = loop {
        if let Some(re) = &re {
            if re.is_match(&session.screen_text().text) {
                break "pattern";
            }
        }
        if session.exited() {
            break "exit";
        }
        let idle = req.idle_ms.map(Duration::from_millis);
        let changed = async {
            let _ = rev.changed().await;
        };
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break "timeout",
            _ = tokio::time::sleep(idle.unwrap_or(Duration::from_secs(3600))), if idle.is_some() => break "idle",
            _ = changed => {}
        }
    };
    Ok(Json(json!({
        "reason": reason,
        "matched": reason == "pattern" || (reason == "idle" && re.is_none()),
        "screen": session.screen_text(),
    })))
}

#[derive(Deserialize)]
struct ResizeReq {
    cols: u16,
    rows: u16,
}

async fn resize(State(app): State<App>, Path(id): Path<String>, Json(req): Json<ResizeReq>) -> Result<Json<Value>, ApiError> {
    find(&app, &id)?.resize(req.cols, req.rows).map_err(|e| bad(format!("{e:#}")))?;
    Ok(Json(json!({ "cols": req.cols, "rows": req.rows })))
}

#[derive(Deserialize)]
struct LockReq {
    locked: bool,
}

async fn lock(State(app): State<App>, Path(id): Path<String>, Json(req): Json<LockReq>) -> Result<Json<Value>, ApiError> {
    find(&app, &id)?.set_locked(req.locked);
    Ok(Json(json!({ "locked": req.locked })))
}

async fn agents(State(app): State<App>, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    let session = find(&app, &id)?;
    let list = session.agents.lock().unwrap().clone();
    Ok(Json(json!({ "agents": list })))
}

async fn agent_transcript(
    State(app): State<App>,
    Path((id, agent_id)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let session = find(&app, &id)?;
    let path = session
        .agents
        .lock()
        .unwrap()
        .iter()
        .find(|a| a.agent_id == agent_id)
        .and_then(|a| a.transcript_path.clone())
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "no such agent".into()))?;
    let lines = tokio::task::spawn_blocking(move || crate::hooks::read_transcript(&path, 400))
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    Ok(Json(json!({ "lines": lines })))
}

async fn hook_events(State(app): State<App>, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    let session = find(&app, &id)?;
    let events = session.hook_events.lock().unwrap().clone();
    Ok(Json(json!({ "events": events })))
}

/// Receives Claude Code hook payloads (`type: "http"` hooks or `almighty hook`).
async fn claude_hook(
    State(app): State<App>,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let id = q.get("session").ok_or_else(|| bad("missing ?session="))?;
    let session = find(&app, id)?;
    let payload: Value = serde_json::from_slice(&body).map_err(|e| bad(e.to_string()))?;
    crate::hooks::apply(&session, payload);
    // An empty object means "no decision": the hook never blocks the agent.
    Ok(Json(json!({})))
}

/// Raw byte stream for real terminals (`almighty attach`).
/// Binary frames both ways; text frames carry JSON control messages.
async fn attach_ws(State(app): State<App>, Path(id): Path<String>, ws: WebSocketUpgrade) -> Result<Response, ApiError> {
    let session = find(&app, &id)?;
    Ok(ws.on_upgrade(move |socket| attach_loop(socket, session)))
}

async fn attach_loop(socket: WebSocket, session: Arc<Session>) {
    let (mut tx, mut rx) = socket.split();
    let (snapshot, mut out) = session.attach();
    if tx.send(Message::Binary(snapshot.into())).await.is_err() {
        return;
    }
    let mut rev = session.watch_rev();
    // The emulator answers cursor/status queries; keep them from the client.
    let mut queries = crate::keys::QueryFilter::default();
    loop {
        tokio::select! {
            chunk = out.recv() => match chunk {
                Ok(bytes) => {
                    let bytes = queries.filter(&bytes);
                    if bytes.is_empty() { continue; }
                    if tx.send(Message::Binary(bytes.into())).await.is_err() { break; }
                }
                // Slow client fell behind: resend a full redraw instead of garbage.
                Err(RecvError::Lagged(_)) => {
                    let (snap, fresh) = session.attach();
                    out = fresh;
                    if tx.send(Message::Binary(snap.into())).await.is_err() { break; }
                }
                Err(RecvError::Closed) => break,
            },
            _ = rev.changed() => {
                if session.exited() {
                    // Flush what is still queued, then tell the client.
                    while let Ok(bytes) = out.try_recv() {
                        let _ = tx.send(Message::Binary(queries.filter(&bytes).into())).await;
                    }
                    let msg = json!({ "type": "exit", "code": session.exit_code() }).to_string();
                    let _ = tx.send(Message::Text(msg.into())).await;
                    break;
                }
            }
            msg = rx.next() => match msg {
                Some(Ok(Message::Binary(bytes))) => {
                    if session.write_input(&bytes, Actor::Human).is_err() { break; }
                }
                Some(Ok(Message::Text(text))) => handle_control(&session, &text),
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
}

fn handle_control(session: &Session, text: &str) {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return };
    match v.get("type").and_then(Value::as_str) {
        Some("resize") => {
            let n = |k: &str| v.get(k).and_then(Value::as_u64).map(|x| x as u16);
            if let (Some(cols), Some(rows)) = (n("cols"), n("rows")) {
                let _ = session.resize(cols, rows);
            }
        }
        Some("input") => {
            let keys: Vec<String> = v
                .get("keys")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|k| k.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let text = v.get("text").and_then(Value::as_str);
            if let Ok(bytes) = encode_input(session, text, &keys) {
                let _ = session.write_input(&bytes, Actor::Human);
            }
        }
        Some("lock") => {
            if let Some(l) = v.get("locked").and_then(Value::as_bool) {
                session.set_locked(l);
            }
        }
        _ => {}
    }
}

/// Styled grid stream for clients that draw the terminal themselves (the
/// Dioxus UI). Sends a full frame, then only the lines that changed.
async fn grid_ws(State(app): State<App>, Path(id): Path<String>, ws: WebSocketUpgrade) -> Result<Response, ApiError> {
    let session = find(&app, &id)?;
    Ok(ws.on_upgrade(move |socket| grid_loop(socket, session)))
}

async fn grid_loop(socket: WebSocket, session: Arc<Session>) {
    let (mut tx, mut rx) = socket.split();
    let mut rev = session.watch_rev();
    let mut prev: Option<crate::session::Grid> = None;
    loop {
        let grid = session.grid();
        let frame = match &prev {
            Some(p) if p.rows == grid.rows && p.cols == grid.cols => {
                let changed: Vec<Value> = grid
                    .lines
                    .iter()
                    .enumerate()
                    .filter(|(i, line)| p.lines.get(*i) != Some(*line))
                    .map(|(i, line)| json!({ "i": i, "runs": line }))
                    .collect();
                json!({
                    "type": "patch", "lines": changed,
                    "cursor_row": grid.cursor_row, "cursor_col": grid.cursor_col,
                    "cursor_visible": grid.cursor_visible, "title": grid.title,
                    "exited": grid.exited, "exit_code": grid.exit_code, "locked": grid.locked,
                })
            }
            _ => json!({ "type": "full", "grid": grid }),
        };
        if tx.send(Message::Text(frame.to_string().into())).await.is_err() {
            break;
        }
        prev = Some(grid);
        // Coalesce bursts of output into ~60 frames per second at most.
        tokio::time::sleep(Duration::from_millis(16)).await;
        loop {
            tokio::select! {
                changed = rev.changed() => {
                    if changed.is_err() { return; }
                    break;
                }
                msg = rx.next() => match msg {
                    Some(Ok(Message::Text(text))) => handle_control(&session, &text),
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                    Some(Ok(_)) => {}
                },
            }
        }
    }
}

const PROBE_TTL: Duration = Duration::from_secs(600);
const PROBE_CACHE_MAX: usize = 2000;

fn valid_hostname(host: &str) -> bool {
    let labels: Vec<&str> = host.split('.').collect();
    host.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        // The last label must be a name, not a number: no IPs, no "1.2.3".
        && labels.last().is_some_and(|l| l.len() >= 2 && l.bytes().all(|b| b.is_ascii_alphabetic()))
}

/// "Does this bare host name answer?" — the check behind treating text such
/// as `google.com` as a link. An ICMP ping needs privileges, so this resolves
/// the name and opens a TCP connection to port 443 (then 80).
async fn probe(State(app): State<App>, Query(q): Query<HashMap<String, String>>) -> Result<Json<Value>, ApiError> {
    let host = q.get("host").map(|h| h.to_ascii_lowercase()).ok_or_else(|| bad("missing ?host="))?;
    if !valid_hostname(&host) {
        return Ok(Json(json!({ "host": host, "ok": false })));
    }
    if let Some((ok, at)) = app.probes.lock().unwrap().get(&host) {
        if at.elapsed() < PROBE_TTL {
            return Ok(Json(json!({ "host": host, "ok": ok, "cached": true })));
        }
    }
    let mut ok = false;
    for port in [443u16, 80] {
        let attempt = tokio::time::timeout(Duration::from_millis(1500), tokio::net::TcpStream::connect((host.as_str(), port)));
        if matches!(attempt.await, Ok(Ok(_))) {
            ok = true;
            break;
        }
    }
    let mut cache = app.probes.lock().unwrap();
    if cache.len() >= PROBE_CACHE_MAX {
        cache.retain(|_, (_, at)| at.elapsed() < PROBE_TTL);
        if cache.len() >= PROBE_CACHE_MAX {
            cache.clear();
        }
    }
    cache.insert(host.clone(), (ok, Instant::now()));
    Ok(Json(json!({ "host": host, "ok": ok })))
}

#[cfg(test)]
mod tests {
    use super::valid_hostname;

    #[test]
    fn hostnames() {
        assert!(valid_hostname("google.com"));
        assert!(valid_hostname("docs.rs"));
        assert!(valid_hostname("a-b.example.org"));
        assert!(!valid_hostname("localhost"));
        assert!(!valid_hostname("1.2.3.4"));
        assert!(!valid_hostname("0.7.10"));
        assert!(!valid_hostname("-bad.com"));
        assert!(!valid_hostname("bad..com"));
        assert!(!valid_hostname("ex ample.com"));
        assert!(!valid_hostname("a.b/c"));
    }
}
