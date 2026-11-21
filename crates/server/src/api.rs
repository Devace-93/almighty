//! HTTP + WebSocket API.
//!
//! Every request except health, workspace info, first-run setup and login
//! carries a token (`Authorization: Bearer`, or `?token=` for browser
//! WebSockets). A token resolves to a [`Principal`]: a person with a role, or
//! the agent of one terminal session.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, Path, Query, Request, State};
use axum::http::{header, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

use crate::keys::{encode_key, encode_text};
use crate::session::{Actor, CreateSession, Creator, InputError, Scope, Session, SpawnContext};
use crate::store::{Principal, Role, Store};

const LOGIN_TTL: u64 = 30 * 24 * 3600;
const MAX_LOGIN_FAILURES: u32 = 5;
const LOGIN_LOCKOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct Config {
    pub addr: SocketAddr,
    pub data_dir: PathBuf,
}

impl Config {
    pub fn from_env() -> Self {
        let addr = std::env::var("ALMIGHTY_ADDR")
            .ok()
            .and_then(|a| a.parse().ok())
            .unwrap_or_else(|| ([127, 0, 0, 1], 8092).into());
        let data_dir = std::env::var_os("ALMIGHTY_DATA")
            .map(PathBuf::from)
            .or_else(|| dirs::data_dir().map(|d| d.join("almighty")))
            .unwrap_or_else(|| PathBuf::from(".almighty"));
        Config { addr, data_dir }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("almighty.db")
    }
}

pub struct AppState {
    pub config: Config,
    pub store: Store,
    /// Needed to create the first owner over the API; printed at start-up.
    pub setup_code: String,
    sessions: RwLock<HashMap<String, Arc<Session>>>,
    next_id: AtomicU64,
    /// Host reachability answers for bare-domain link detection.
    probes: Mutex<HashMap<String, (bool, Instant)>>,
    events: broadcast::Sender<String>,
    login_failures: Mutex<HashMap<String, (u32, Instant)>>,
}

pub type App = Arc<AppState>;

impl AppState {
    fn emit(&self, event: Value) {
        let _ = self.events.send(event.to_string());
    }
}

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}

fn forbidden(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::FORBIDDEN, msg.into())
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

type Who = Extension<Principal>;
type ApiResult = Result<Json<Value>, ApiError>;

/// What `who` may do with `session`. `None`: it must not even see it.
///
/// People: guests read, everyone else writes. An agent reads its own
/// session, writes sessions it started, and gets whatever a person granted.
fn access(who: &Principal, session: &Session) -> Option<Scope> {
    match who {
        Principal::User { role, .. } => Some(if *role >= Role::Member { Scope::Write } else { Scope::Read }),
        Principal::Agent { session: own, .. } => {
            if session.created_by == Creator::Agent(own.clone()) {
                return Some(Scope::Write);
            }
            let granted = session.grants.lock().unwrap().get(own).copied();
            if session.id == *own {
                return Some(granted.unwrap_or(Scope::Read));
            }
            granted
        }
    }
}

/// Looks a session up and checks the caller's access. A session the caller
/// may not see answers 404, the same as one that does not exist.
fn need(app: &App, who: &Principal, id: &str, scope: Scope) -> Result<Arc<Session>, ApiError> {
    let missing = || ApiError(StatusCode::NOT_FOUND, format!("no session {id}"));
    let session = app.sessions.read().unwrap().get(id).cloned().ok_or_else(missing)?;
    match access(who, &session) {
        None => Err(missing()),
        Some(have) if have < scope => Err(forbidden("read-only access to this session")),
        Some(_) => Ok(session),
    }
}

fn need_role(who: &Principal, min: Role) -> Result<(i64, &str), ApiError> {
    match who {
        Principal::User { id, name, role, .. } if *role >= min => Ok((*id, name.as_str())),
        Principal::User { .. } => Err(forbidden(format!("needs role {} or higher", min.as_str()))),
        Principal::Agent { .. } => Err(forbidden("not available to agents")),
    }
}

fn actor_of(who: &Principal) -> Actor {
    match who {
        Principal::User { .. } => Actor::Human,
        Principal::Agent { .. } => Actor::Agent,
    }
}

pub fn build(config: Config, store: Store) -> (Router, App) {
    let (events, _) = broadcast::channel(256);
    let code: [u8; 5] = rand::random();
    let app: App = Arc::new(AppState {
        config,
        store,
        setup_code: code.iter().map(|b| format!("{b:02x}")).collect(),
        sessions: RwLock::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        probes: Mutex::new(HashMap::new()),
        events,
        login_failures: Mutex::new(HashMap::new()),
    });
    let router = Router::new()
        .route("/api/health", get(|| async { Json(json!({ "ok": true })) }))
        .route("/api/workspace", get(workspace))
        .route("/api/setup", post(setup))
        .route("/api/auth/login", post(login))
        .route("/api/auth/me", get(me))
        .route("/api/auth/logout", post(logout))
        .route("/api/tokens", get(list_tokens).post(create_token))
        .route("/api/tokens/{id}", delete(delete_token))
        .route("/api/users", get(list_users).post(create_user))
        .route("/api/audit", get(audit))
        .route("/api/events", get(events_ws))
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/sessions/{id}", get(get_session).delete(close_session))
        .route("/api/sessions/{id}/screen", get(screen))
        .route("/api/sessions/{id}/input", post(input))
        .route("/api/sessions/{id}/wait", post(wait))
        .route("/api/sessions/{id}/resize", post(resize))
        .route("/api/sessions/{id}/lock", post(lock))
        .route("/api/sessions/{id}/grants", get(list_grants).post(set_grant))
        .route("/api/sessions/{id}/agents", get(agents))
        .route("/api/sessions/{id}/agents/{agent_id}/transcript", get(agent_transcript))
        .route("/api/sessions/{id}/hook-events", get(hook_events))
        .route("/api/sessions/{id}/attach", get(attach_ws))
        .route("/api/sessions/{id}/grid", get(grid_ws))
        .route("/api/hooks/claude", post(claude_hook))
        .route("/api/probe", get(probe))
        .layer(middleware::from_fn_with_state(app.clone(), auth))
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(app.clone());
    (router, app)
}

pub async fn serve(config: Config) -> anyhow::Result<()> {
    let store = Store::open(&config.db_path())?;
    let listener = tokio::net::TcpListener::bind(config.addr).await.map_err(|e| {
        anyhow::anyhow!(
            "cannot listen on {}: {e}. The port may be taken or reserved by the system; \
             set ALMIGHTY_ADDR (e.g. 127.0.0.1:8192) to use another one",
            config.addr
        )
    })?;
    let (router, app) = build(config, store);
    eprintln!("almightyd listening on {} (data: {})", app.config.url(), app.config.data_dir.display());
    if app.store.user_count() == 0 {
        eprintln!("No users yet. Create the owner with `almighty user add <name> --role owner`,");
        eprintln!("or from a client with this one-time setup code: {}", app.setup_code);
    }
    axum::serve(listener, router).await?;
    Ok(())
}

const PUBLIC: &[&str] = &["/api/health", "/api/workspace", "/api/setup", "/api/auth/login"];

async fn auth(State(app): State<App>, mut req: Request, next: Next) -> Response {
    if req.method() == Method::OPTIONS || PUBLIC.contains(&req.uri().path()) {
        return next.run(req).await;
    }
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);
    // Browsers cannot set headers on WebSocket requests: allow ?token= too.
    let query = req
        .uri()
        .query()
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("token=")))
        .map(str::to_string);
    let principal = bearer.or(query).and_then(|t| app.store.principal_for(&t));
    match principal {
        Some(p) => {
            req.extensions_mut().insert(p);
            next.run(req).await
        }
        None => ApiError(StatusCode::UNAUTHORIZED, "sign in first: missing, wrong or expired token".into()).into_response(),
    }
}

// ---------- workspace, accounts, tokens ----------

async fn workspace(State(app): State<App>) -> Json<Value> {
    Json(json!({
        "name": app.store.workspace_name(),
        "setup_needed": app.store.user_count() == 0,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

#[derive(Deserialize)]
struct SetupReq {
    code: String,
    name: String,
    password: String,
    #[serde(default)]
    workspace: Option<String>,
}

/// First run only: creates the owner. Needs the code printed by the server,
/// so reaching the port is not enough to claim a fresh instance.
async fn setup(State(app): State<App>, Json(req): Json<SetupReq>) -> ApiResult {
    if app.store.user_count() > 0 {
        return Err(ApiError(StatusCode::CONFLICT, "this workspace is already set up".into()));
    }
    if !constant_time_eq(req.code.trim().as_bytes(), app.setup_code.as_bytes()) {
        tokio::time::sleep(Duration::from_millis(400)).await;
        return Err(forbidden("wrong setup code (it is printed where the server runs)"));
    }
    let id = app
        .store
        .create_first_owner(&req.name, &req.password)
        .map_err(|e| bad(e.to_string()))?
        .ok_or_else(|| ApiError(StatusCode::CONFLICT, "this workspace is already set up".into()))?;
    if let Some(ws) = req.workspace.as_deref().map(str::trim).filter(|w| !w.is_empty() && w.len() <= 80) {
        let _ = app.store.set_workspace_name(ws);
    }
    let (_, token) = app.store.create_user_token(id, "login", "setup", Some(LOGIN_TTL)).map_err(|e| bad(e.to_string()))?;
    app.store.audit(&format!("user:{}", req.name), None, "setup", "owner created");
    Ok(Json(json!({ "token": token, "name": req.name, "role": "owner" })))
}

#[derive(Deserialize)]
struct LoginReq {
    name: String,
    password: String,
    #[serde(default)]
    device: Option<String>,
}

async fn login(State(app): State<App>, Json(req): Json<LoginReq>) -> ApiResult {
    let key = req.name.to_ascii_lowercase();
    {
        let failures = app.login_failures.lock().unwrap();
        if let Some((count, at)) = failures.get(&key) {
            if *count >= MAX_LOGIN_FAILURES && at.elapsed() < LOGIN_LOCKOUT {
                return Err(ApiError(StatusCode::TOO_MANY_REQUESTS, "too many failed attempts; wait a minute".into()));
            }
        }
    }
    let store_app = app.clone();
    let (name, password) = (req.name.clone(), req.password.clone());
    // Argon2 is deliberately slow: keep it off the async workers.
    let user = tokio::task::spawn_blocking(move || store_app.store.verify_login(&name, &password)).await.ok().flatten();
    let Some(user_id) = user else {
        let mut failures = app.login_failures.lock().unwrap();
        if failures.len() > 10_000 {
            failures.clear();
        }
        let entry = failures.entry(key).or_insert((0, Instant::now()));
        if entry.1.elapsed() >= LOGIN_LOCKOUT {
            *entry = (0, Instant::now());
        }
        *entry = (entry.0 + 1, Instant::now());
        return Err(ApiError(StatusCode::UNAUTHORIZED, "wrong name or password".into()));
    };
    app.login_failures.lock().unwrap().remove(&key);
    let device = req.device.as_deref().unwrap_or("client").chars().take(60).collect::<String>();
    let (_, token) = app.store.create_user_token(user_id, "login", &device, Some(LOGIN_TTL)).map_err(|e| bad(e.to_string()))?;
    let principal = app.store.principal_for(&token);
    let role = match &principal {
        Some(Principal::User { role, .. }) => role.as_str(),
        _ => "guest",
    };
    app.store.audit(&format!("user:{}", req.name), None, "login", &device);
    Ok(Json(json!({ "token": token, "name": req.name, "role": role })))
}

async fn me(State(app): State<App>, Extension(who): Who) -> Json<Value> {
    let workspace = app.store.workspace_name();
    Json(match who {
        Principal::User { name, role, .. } => json!({ "kind": "user", "name": name, "role": role, "workspace": workspace }),
        Principal::Agent { session, .. } => json!({ "kind": "agent", "session": session, "workspace": workspace }),
    })
}

async fn logout(State(app): State<App>, Extension(who): Who) -> ApiResult {
    if let Principal::User { id, token_id, .. } = who {
        app.store.delete_user_token(id, token_id);
    }
    Ok(Json(json!({ "ok": true })))
}

async fn list_tokens(State(app): State<App>, Extension(who): Who) -> ApiResult {
    let (id, _) = need_role(&who, Role::Guest)?;
    Ok(Json(json!({ "tokens": app.store.list_user_tokens(id) })))
}

#[derive(Deserialize)]
struct TokenReq {
    name: String,
}

/// Long-lived token for scripts and MCP clients. Shown once.
async fn create_token(State(app): State<App>, Extension(who): Who, Json(req): Json<TokenReq>) -> ApiResult {
    let (id, user) = need_role(&who, Role::Member)?;
    let name: String = req.name.trim().chars().take(60).collect();
    if name.is_empty() {
        return Err(bad("give the token a name"));
    }
    let (token_id, token) = app.store.create_user_token(id, "api", &name, None).map_err(|e| bad(e.to_string()))?;
    app.store.audit(&format!("user:{user}"), None, "token.create", &name);
    Ok(Json(json!({ "id": token_id, "name": name, "token": token })))
}

async fn delete_token(State(app): State<App>, Extension(who): Who, Path(token_id): Path<i64>) -> ApiResult {
    let (id, _) = need_role(&who, Role::Guest)?;
    if !app.store.delete_user_token(id, token_id) {
        return Err(ApiError(StatusCode::NOT_FOUND, "no such token".into()));
    }
    Ok(Json(json!({ "deleted": token_id })))
}

async fn list_users(State(app): State<App>, Extension(who): Who) -> ApiResult {
    need_role(&who, Role::Admin)?;
    Ok(Json(json!({ "users": app.store.list_users() })))
}

#[derive(Deserialize)]
struct UserReq {
    name: String,
    password: String,
    role: Role,
}

async fn create_user(State(app): State<App>, Extension(who): Who, Json(req): Json<UserReq>) -> ApiResult {
    let (_, admin) = need_role(&who, Role::Admin)?;
    // Only an owner can create another owner.
    if req.role == Role::Owner {
        need_role(&who, Role::Owner)?;
    }
    let store_app = app.clone();
    let (name, password, role) = (req.name.clone(), req.password, req.role);
    let id = tokio::task::spawn_blocking(move || store_app.store.create_user(&name, &password, role))
        .await
        .map_err(|e| bad(e.to_string()))?
        .map_err(|e| bad(e.to_string()))?;
    app.store.audit(&format!("user:{admin}"), None, "user.create", &format!("{} ({})", req.name, req.role.as_str()));
    Ok(Json(json!({ "id": id, "name": req.name, "role": req.role })))
}

async fn audit(State(app): State<App>, Extension(who): Who, Query(q): Query<HashMap<String, String>>) -> ApiResult {
    need_role(&who, Role::Admin)?;
    let limit = q.get("limit").and_then(|l| l.parse().ok()).unwrap_or(200u32).min(1000);
    Ok(Json(json!({ "entries": app.store.audit_tail(q.get("session").map(String::as_str), limit) })))
}

/// Push channel: tells clients when the session list or an agent tree changed.
async fn events_ws(State(app): State<App>, Extension(_who): Who, ws: WebSocketUpgrade) -> Response {
    let mut rx = app.events.subscribe();
    ws.on_upgrade(move |socket| async move {
        let (mut tx, mut incoming) = socket.split();
        loop {
            tokio::select! {
                event = rx.recv() => match event {
                    Ok(text) => {
                        if tx.send(Message::Text(text.into())).await.is_err() { break; }
                    }
                    // Missed some: tell the client to refetch everything.
                    Err(RecvError::Lagged(_)) => {
                        if tx.send(Message::Text(json!({ "type": "resync" }).to_string().into())).await.is_err() { break; }
                    }
                    Err(RecvError::Closed) => break,
                },
                msg = incoming.next() => match msg {
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => {}
                },
            }
        }
    })
}

// ---------- terminal sessions ----------

async fn list_sessions(State(app): State<App>, Extension(who): Who) -> Json<Value> {
    let mut list: Vec<Value> = app
        .sessions
        .read()
        .unwrap()
        .values()
        .filter_map(|s| {
            let scope = access(&who, s)?;
            let mut info = json!(s.info());
            info["access"] = json!(scope);
            Some(info)
        })
        .collect();
    list.sort_by_key(|s| s["id"].as_str().and_then(|id| id[1..].parse::<u64>().ok()).unwrap_or(0));
    Json(json!({ "sessions": list }))
}

async fn create_session(State(app): State<App>, Extension(who): Who, Json(req): Json<CreateSession>) -> ApiResult {
    let created_by = match &who {
        Principal::User { name, .. } => {
            need_role(&who, Role::Member)?;
            Creator::User(name.clone())
        }
        Principal::Agent { session, .. } => Creator::Agent(session.clone()),
    };
    let id = format!("s{}", app.next_id.fetch_add(1, Ordering::SeqCst));
    let (_, agent_token) = app.store.create_agent_token(&id).map_err(|e| bad(e.to_string()))?;
    let notify = app.clone();
    let ctx = SpawnContext {
        server_url: app.config.url(),
        agent_token: Some(agent_token),
        created_by,
        on_change: Box::new(move || notify.emit(json!({ "type": "sessions" }))),
    };
    let session = Session::spawn(id.clone(), req, ctx).map_err(|e| {
        app.store.delete_agent_tokens(&id);
        bad(format!("{e:#}"))
    })?;
    let info = session.info();
    app.store.audit(&who.label(), Some(&id), "session.create", &info.title);
    app.sessions.write().unwrap().insert(id, session);
    app.emit(json!({ "type": "sessions" }));
    Ok(Json(json!(info)))
}

async fn get_session(State(app): State<App>, Extension(who): Who, Path(id): Path<String>) -> ApiResult {
    Ok(Json(json!(need(&app, &who, &id, Scope::Read)?.info())))
}

async fn close_session(State(app): State<App>, Extension(who): Who, Path(id): Path<String>) -> ApiResult {
    let session = need(&app, &who, &id, Scope::Write)?;
    session.kill();
    app.sessions.write().unwrap().remove(&id);
    app.store.delete_agent_tokens(&id);
    app.store.audit(&who.label(), Some(&id), "session.close", "");
    app.emit(json!({ "type": "sessions" }));
    Ok(Json(json!({ "closed": id })))
}

async fn screen(State(app): State<App>, Extension(who): Who, Path(id): Path<String>) -> ApiResult {
    Ok(Json(json!(need(&app, &who, &id, Scope::Read)?.screen_text())))
}

#[derive(Deserialize)]
struct InputReq {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    keys: Vec<String>,
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

/// Short, readable record of what was typed, for the audit log.
fn describe_input(text: Option<&str>, keys: &[String]) -> String {
    let mut parts = Vec::new();
    if let Some(t) = text {
        let shown: String = t.chars().take(200).collect();
        parts.push(format!("text: {shown:?}{}", if t.chars().count() > 200 { "…" } else { "" }));
    }
    if !keys.is_empty() {
        parts.push(format!("keys: {}", keys.join(" ")));
    }
    parts.join(" | ")
}

async fn input(State(app): State<App>, Extension(who): Who, Path(id): Path<String>, Json(req): Json<InputReq>) -> ApiResult {
    let session = need(&app, &who, &id, Scope::Write)?;
    let bytes = encode_input(&session, req.text.as_deref(), &req.keys)?;
    if bytes.is_empty() {
        return Err(bad("nothing to send: give `text` and/or `keys`"));
    }
    let actor = actor_of(&who);
    session.write_input(&bytes, actor)?;
    // Everything an agent types is attributable afterwards.
    if actor == Actor::Agent {
        app.store.audit(&who.label(), Some(&id), "input", &describe_input(req.text.as_deref(), &req.keys));
    }
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

async fn wait(State(app): State<App>, Extension(who): Who, Path(id): Path<String>, Json(req): Json<WaitReq>) -> ApiResult {
    let session = need(&app, &who, &id, Scope::Read)?;
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

async fn resize(State(app): State<App>, Extension(who): Who, Path(id): Path<String>, Json(req): Json<ResizeReq>) -> ApiResult {
    need(&app, &who, &id, Scope::Write)?.resize(req.cols, req.rows).map_err(|e| bad(format!("{e:#}")))?;
    Ok(Json(json!({ "cols": req.cols, "rows": req.rows })))
}

#[derive(Deserialize)]
struct LockReq {
    locked: bool,
}

/// Locking keeps agents out, so only a person can lock or unlock.
async fn lock(State(app): State<App>, Extension(who): Who, Path(id): Path<String>, Json(req): Json<LockReq>) -> ApiResult {
    need_role(&who, Role::Member)?;
    need(&app, &who, &id, Scope::Write)?.set_locked(req.locked);
    app.store.audit(&who.label(), Some(&id), if req.locked { "lock" } else { "unlock" }, "");
    app.emit(json!({ "type": "sessions" }));
    Ok(Json(json!({ "locked": req.locked })))
}

async fn list_grants(State(app): State<App>, Extension(who): Who, Path(id): Path<String>) -> ApiResult {
    need_role(&who, Role::Member)?;
    let session = need(&app, &who, &id, Scope::Read)?;
    let grants = session.grants.lock().unwrap().clone();
    Ok(Json(json!({ "grants": grants })))
}

#[derive(Deserialize)]
struct GrantReq {
    /// Session whose agent receives the access.
    agent: String,
    /// `read`, `write`, or null to take it away.
    scope: Option<Scope>,
}

/// A person lets the agent of another session read or drive this one.
async fn set_grant(State(app): State<App>, Extension(who): Who, Path(id): Path<String>, Json(req): Json<GrantReq>) -> ApiResult {
    need_role(&who, Role::Member)?;
    let session = need(&app, &who, &id, Scope::Write)?;
    if !app.sessions.read().unwrap().contains_key(&req.agent) {
        return Err(bad(format!("no session {}", req.agent)));
    }
    {
        let mut grants = session.grants.lock().unwrap();
        match req.scope {
            Some(scope) => grants.insert(req.agent.clone(), scope),
            None => grants.remove(&req.agent),
        };
    }
    let detail = format!("agent:{} -> {}", req.agent, req.scope.map(|s| format!("{s:?}").to_lowercase()).unwrap_or_else(|| "none".into()));
    app.store.audit(&who.label(), Some(&id), "grant", &detail);
    app.emit(json!({ "type": "sessions" }));
    Ok(Json(json!({ "agent": req.agent, "scope": req.scope })))
}

async fn agents(State(app): State<App>, Extension(who): Who, Path(id): Path<String>) -> ApiResult {
    let session = need(&app, &who, &id, Scope::Read)?;
    let list = session.agents.lock().unwrap().clone();
    Ok(Json(json!({ "agents": list })))
}

async fn agent_transcript(
    State(app): State<App>,
    Extension(who): Who,
    Path((id, agent_id)): Path<(String, String)>,
) -> ApiResult {
    let session = need(&app, &who, &id, Scope::Read)?;
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

async fn hook_events(State(app): State<App>, Extension(who): Who, Path(id): Path<String>) -> ApiResult {
    let session = need(&app, &who, &id, Scope::Read)?;
    let events = session.hook_events.lock().unwrap().clone();
    Ok(Json(json!({ "events": events })))
}

/// Receives Claude Code hook payloads from `almighty hook`. An agent token
/// can only report for its own session.
async fn claude_hook(
    State(app): State<App>,
    Extension(who): Who,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> ApiResult {
    let id = q.get("session").ok_or_else(|| bad("missing ?session="))?;
    match &who {
        Principal::Agent { session, .. } if session == id => {}
        Principal::Agent { .. } => return Err(forbidden("a session can only report its own hooks")),
        Principal::User { .. } => {
            need_role(&who, Role::Member)?;
        }
    }
    let session = app
        .sessions
        .read()
        .unwrap()
        .get(id)
        .cloned()
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no session {id}")))?;
    let payload: Value = serde_json::from_slice(&body).map_err(|e| bad(e.to_string()))?;
    crate::hooks::apply(&session, payload);
    app.emit(json!({ "type": "agents", "session": id }));
    // An empty object means "no decision": the hook never blocks the agent.
    Ok(Json(json!({})))
}

/// What a WebSocket client may do, fixed when it connects.
#[derive(Clone)]
struct Rights {
    write: bool,
    actor: Actor,
    /// Only people lock sessions.
    can_lock: bool,
    label: String,
}

fn rights(who: &Principal, session: &Session) -> Rights {
    Rights {
        write: access(who, session) == Some(Scope::Write),
        actor: actor_of(who),
        can_lock: matches!(who, Principal::User { role, .. } if *role >= Role::Member),
        label: who.label(),
    }
}

/// Raw byte stream for real terminals (`almighty attach`).
/// Binary frames both ways; text frames carry JSON control messages.
async fn attach_ws(State(app): State<App>, Extension(who): Who, Path(id): Path<String>, ws: WebSocketUpgrade) -> Result<Response, ApiError> {
    let session = need(&app, &who, &id, Scope::Read)?;
    let rights = rights(&who, &session);
    Ok(ws.on_upgrade(move |socket| attach_loop(socket, session, rights, app)))
}

async fn attach_loop(socket: WebSocket, session: Arc<Session>, rights: Rights, app: App) {
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
                    // Read-only clients may watch; their keys go nowhere.
                    if rights.write && matches!(session.write_input(&bytes, rights.actor), Err(InputError::Exited)) { break; }
                }
                Some(Ok(Message::Text(text))) => handle_control(&app, &session, &rights, &text),
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
}

fn handle_control(app: &App, session: &Session, rights: &Rights, text: &str) {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return };
    match v.get("type").and_then(Value::as_str) {
        Some("resize") if rights.write => {
            let n = |k: &str| v.get(k).and_then(Value::as_u64).map(|x| x as u16);
            if let (Some(cols), Some(rows)) = (n("cols"), n("rows")) {
                let _ = session.resize(cols, rows);
            }
        }
        Some("input") if rights.write => {
            let keys: Vec<String> = v
                .get("keys")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|k| k.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let text = v.get("text").and_then(Value::as_str);
            if let Ok(bytes) = encode_input(session, text, &keys) {
                if session.write_input(&bytes, rights.actor).is_ok() && rights.actor == Actor::Agent {
                    app.store.audit(&rights.label, Some(&session.id), "input", &describe_input(text, &keys));
                }
            }
        }
        Some("lock") if rights.can_lock && rights.write => {
            if let Some(l) = v.get("locked").and_then(Value::as_bool) {
                session.set_locked(l);
                app.store.audit(&rights.label, Some(&session.id), if l { "lock" } else { "unlock" }, "");
            }
        }
        _ => {}
    }
}

/// Styled grid stream for clients that draw the terminal themselves (the
/// Dioxus UI). Sends a full frame, then only the lines that changed.
async fn grid_ws(State(app): State<App>, Extension(who): Who, Path(id): Path<String>, ws: WebSocketUpgrade) -> Result<Response, ApiError> {
    let session = need(&app, &who, &id, Scope::Read)?;
    let rights = rights(&who, &session);
    Ok(ws.on_upgrade(move |socket| grid_loop(socket, session, rights, app)))
}

async fn grid_loop(socket: WebSocket, session: Arc<Session>, rights: Rights, app: App) {
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
            _ => json!({ "type": "full", "grid": grid, "can_write": rights.write }),
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
                    Some(Ok(Message::Text(text))) => handle_control(&app, &session, &rights, &text),
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                    Some(Ok(_)) => {}
                },
            }
        }
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
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
async fn probe(State(app): State<App>, Extension(_who): Who, Query(q): Query<HashMap<String, String>>) -> ApiResult {
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
