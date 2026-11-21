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
use axum::extract::{DefaultBodyLimit, Extension, Path, Query, Request, State};
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
use crate::plugins::db::ItemQuery;
use crate::plugins::manifest::Permissions;
use crate::plugins::{Call, Plugins, MAX_COMPONENT_BYTES};
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

/// Directory with the built web client, served at `/` when set
/// (`ALMIGHTY_WEB_DIR`). Without it the server only speaks the API.
pub fn web_dir() -> Option<PathBuf> {
    std::env::var_os("ALMIGHTY_WEB_DIR").map(PathBuf::from).filter(|d| d.join("index.html").is_file())
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
    pub store: Arc<Store>,
    pub plugins: Arc<Plugins>,
    pub triage: crate::triage::Triage,
    pub signer: crate::service::Signer,
    pub oidc: crate::oidc::Oidc,
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
    let store = Arc::new(store);
    let plugins = Plugins::new(&config.data_dir, store.clone(), events.clone()).expect("plugin host");
    let signer = crate::service::Signer::new(&config.data_dir);
    let triage = crate::triage::Triage { store: store.clone(), data_dir: config.data_dir.clone(), events: events.clone() };
    let app: App = Arc::new(AppState {
        config,
        store,
        plugins,
        triage,
        signer,
        oidc: crate::oidc::Oidc::default(),
        setup_code: code.iter().map(|b| format!("{b:02x}")).collect(),
        sessions: RwLock::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        probes: Mutex::new(HashMap::new()),
        events,
        login_failures: Mutex::new(HashMap::new()),
    });
    let router = Router::new()
        .route("/api/health", get(|| async { Json(json!({ "ok": true })) }))
        .route("/.well-known/jwks.json", get(jwks))
        .route("/hooks/{plugin}", post(plugin_event))
        .route("/api/plugins/{id}/token", post(plugin_token))
        .route("/api/plugins/{id}/events-secret", post(rotate_events_secret))
        .route("/api/resolve", get(resolve_uri))
        .route("/api/workspace", get(workspace))
        .route("/api/setup", post(setup))
        .route("/api/auth/login", post(login))
        .route("/api/auth/oidc/start", get(oidc_start))
        .route("/api/auth/oidc/callback", get(oidc_callback))
        .route("/api/auth/oidc/exchange", post(oidc_exchange))
        .route("/api/users/{name}/role", post(set_user_role))
        .route("/api/users/{name}", delete(delete_user))
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
        .route("/api/sessions/{id}/background", post(background))
        .route("/api/sessions/{id}/grants", get(list_grants).post(set_grant))
        .route("/api/sessions/{id}/agents", get(agents))
        .route("/api/sessions/{id}/agents/{agent_id}/transcript", get(agent_transcript))
        .route("/api/sessions/{id}/hook-events", get(hook_events))
        .route("/api/sessions/{id}/attach", get(attach_ws))
        .route("/api/sessions/{id}/grid", get(grid_ws))
        .route("/api/hooks/claude", post(claude_hook))
        .route("/api/probe", get(probe))
        .route("/api/plugins", get(list_plugins).post(install_plugin).layer(DefaultBodyLimit::max(MAX_COMPONENT_BYTES * 2)))
        .route("/api/plugins/{id}", get(get_plugin).delete(remove_plugin))
        .route("/api/plugins/{id}/enabled", post(enable_plugin))
        .route("/api/plugins/{id}/secrets/{name}", post(set_secret).delete(delete_secret))
        .route("/api/plugins/{id}/logs", get(plugin_logs))
        .route("/api/plugins/{id}/poll", post(poll_plugin))
        .route("/api/plugins/{id}/panels/{panel}", get(render_panel))
        .route("/api/plugins/{id}/panels/{panel}/action", post(panel_action))
        .route("/api/plugins/{id}/commands/{command}", post(run_command))
        .route("/api/agent-tools", get(agent_tools))
        .route("/api/items", get(list_items))
        .route("/api/layout", get(layout))
        .route("/api/sessions/{id}/media", get(session_media))
        .route("/api/sessions/{id}/media/{media}", get(session_media_file))
        .route("/api/review/{item}", get(review))
        .route("/api/review/{item}/summary", post(review_summary))
        .route("/api/explain", post(explain))
        .route("/api/push", get(push_list).post(push_add))
        .route("/api/push/test", post(push_test))
        .route("/api/push/{id}", delete(push_remove))
        .route("/api/planner", get(planner).post(planner_add))
        .route("/api/planner/{id}", post(planner_change).delete(planner_delete))
        .route("/api/digest", get(digest))
        .route("/api/digest/summary", post(digest_summary))
        .route("/api/inbox", get(inbox))
        .route("/api/inbox/counts", get(inbox_counts))
        .route("/api/inbox/preferences", get(inbox_preferences))
        .route("/api/inbox/{id}", post(inbox_change))
        // An unknown API path is an error, never the web client's page.
        .route("/api/{*rest}", axum::routing::any(|| async { ApiError(StatusCode::NOT_FOUND, "no such API path".into()) }))
        .layer(middleware::from_fn_with_state(app.clone(), auth))
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(app.clone());
    // Unknown paths fall through to the web client (a single-page app), when one is configured.
    let router = match web_dir() {
        Some(dir) => {
            let index = dir.join("index.html");
            router.fallback_service(tower_http::services::ServeDir::new(dir).fallback(tower_http::services::ServeFile::new(index)))
        }
        None => router,
    };
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
    crate::plugins::spawn_scheduler(app.plugins.clone());
    crate::triage::spawn_worker(app.triage.clone());
    crate::push::spawn_worker(app.store.clone(), app.config.data_dir.clone(), app.triage.events.clone());
    let names = app.clone();
    crate::service::spawn_outbound(app.plugins.clone(), app.store.clone(), app.triage.events.clone(), move || names.store.workspace_name());
    eprintln!("almightyd listening on {} (data: {})", app.config.url(), app.config.data_dir.display());
    if app.store.user_count() == 0 {
        eprintln!("No users yet. Create the owner with `almighty user add <name> --role owner`,");
        eprintln!("or from a client with this one-time setup code: {}", app.setup_code);
    }
    axum::serve(listener, router).await?;
    Ok(())
}

const PUBLIC: &[&str] = &[
    "/api/health",
    "/api/workspace",
    "/api/setup",
    "/api/auth/login",
    "/api/auth/oidc/start",
    "/api/auth/oidc/callback",
    "/api/auth/oidc/exchange",
    "/.well-known/jwks.json",
];

async fn auth(State(app): State<App>, mut req: Request, next: Next) -> Response {
    // Hooks authenticate with a signature over the body, checked in the handler.
    let path = req.uri().path();
    // The web client's files are public; everything under /api/ and the listed paths is not.
    let is_api = path.starts_with("/api/") || path.starts_with("/hooks/") || path.starts_with("/.well-known/");
    if req.method() == Method::OPTIONS || PUBLIC.contains(&path) || path.starts_with("/hooks/") || !is_api {
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
        // Whether "sign in with your organization" is available, and through whom.
        "sso": crate::oidc::Config::load(&app.config.data_dir).map(|c| c.issuer),
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

// ---- sign-in through an identity provider ----

fn oidc_config(app: &App) -> Result<crate::oidc::Config, ApiError> {
    crate::oidc::Config::load(&app.config.data_dir)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "this workspace has no identity provider configured".into()))
}

/// Sends the browser to the provider. `redirect` is where the client wants
/// to be sent back with a one-time code.
async fn oidc_start(State(app): State<App>, Query(q): Query<HashMap<String, String>>) -> Result<Response, ApiError> {
    let config = oidc_config(&app)?;
    let redirect = q.get("redirect").cloned().unwrap_or_default();
    let device = q.get("device").cloned().unwrap_or_else(|| "client".into());
    let (app2, url) = (app.clone(), app.config.url());
    let target = tokio::task::spawn_blocking(move || app2.oidc.start(&config, &url, &redirect, &device))
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(bad)?;
    Ok(axum::response::Redirect::to(&target).into_response())
}

/// The provider sends the browser back here.
async fn oidc_callback(State(app): State<App>, Query(q): Query<HashMap<String, String>>) -> Result<Response, ApiError> {
    let config = oidc_config(&app)?;
    if let Some(error) = q.get("error") {
        let detail: String = q.get("error_description").unwrap_or(error).chars().filter(|c| !c.is_control()).take(200).collect();
        return Err(forbidden(format!("the identity provider refused the sign-in: {detail}")));
    }
    let (Some(state), Some(code)) = (q.get("state").cloned(), q.get("code").cloned()) else {
        return Err(bad("missing code or state"));
    };
    let (app2, url, config2) = (app.clone(), app.config.url(), config.clone());
    let (identity, redirect, device) = tokio::task::spawn_blocking(move || app2.oidc.finish(&config2, &url, &state, &code))
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(forbidden)?;
    let user = app.store.oidc_user(&config, &identity).map_err(forbidden)?;
    let (_, token) = app.store.create_user_token(user, "login", &device, Some(LOGIN_TTL)).map_err(|e| bad(e.to_string()))?;
    app.store.audit(&format!("user:{}", identity.email), None, "login.sso", &device);
    // The session token never travels in a URL: the client trades this code for it.
    let code = app.oidc.hand_over(token);
    let separator = if redirect.contains('?') { '&' } else { '?' };
    Ok(axum::response::Redirect::to(&format!("{redirect}{separator}almighty_code={code}")).into_response())
}

#[derive(Deserialize)]
struct ExchangeReq {
    code: String,
}

async fn oidc_exchange(State(app): State<App>, Json(req): Json<ExchangeReq>) -> ApiResult {
    let token = app.oidc.redeem(&req.code).ok_or_else(|| ApiError(StatusCode::UNAUTHORIZED, "this sign-in code is not valid any more".into()))?;
    match app.store.principal_for(&token) {
        Some(Principal::User { name, role, .. }) => Ok(Json(json!({ "token": token, "name": name, "role": role.as_str() }))),
        _ => Err(ApiError(StatusCode::UNAUTHORIZED, "this sign-in code is not valid any more".into())),
    }
}

#[derive(Deserialize)]
struct RoleReq {
    role: Role,
}

/// Admins manage members and guests; only owners touch admins and owners.
fn may_manage(who: &Principal, target: Option<Role>, new_role: Option<Role>) -> Result<String, ApiError> {
    let (_, name) = need_role(who, Role::Admin)?;
    let highest = target.into_iter().chain(new_role).max().unwrap_or(Role::Guest);
    if highest >= Role::Admin {
        need_role(who, Role::Owner)?;
    }
    Ok(name.to_string())
}

async fn set_user_role(State(app): State<App>, Extension(who): Who, Path(name): Path<String>, Json(req): Json<RoleReq>) -> ApiResult {
    let target = app.store.role_of(&name).ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no user {name}")))?;
    let by = may_manage(&who, Some(target), Some(req.role))?;
    app.store.set_role(&name, req.role).map_err(bad)?;
    app.store.audit(&format!("user:{by}"), None, "user.role", &format!("{name}: {} -> {}", target.as_str(), req.role.as_str()));
    Ok(Json(json!({ "name": name, "role": req.role })))
}

async fn delete_user(State(app): State<App>, Extension(who): Who, Path(name): Path<String>) -> ApiResult {
    let target = app.store.role_of(&name).ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no user {name}")))?;
    let by = may_manage(&who, Some(target), None)?;
    if by.eq_ignore_ascii_case(&name) {
        return Err(bad("you cannot remove your own account"));
    }
    app.store.delete_user(&name).map_err(bad)?;
    app.store.audit(&format!("user:{by}"), None, "user.delete", &name);
    Ok(Json(json!({ "ok": true })))
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
/// Events addressed to one person (`"user": name`) reach only that person.
fn event_is_for(text: &str, who: &Principal) -> bool {
    if !text.contains("\"user\"") {
        return true;
    }
    let to = serde_json::from_str::<Value>(text).ok().and_then(|v| v["user"].as_str().map(str::to_string));
    match (to, who) {
        (None, _) => true,
        (Some(to), Principal::User { name, .. }) => to.eq_ignore_ascii_case(name),
        (Some(_), Principal::Agent { .. }) => false,
    }
}

async fn events_ws(State(app): State<App>, Extension(who): Who, ws: WebSocketUpgrade) -> Response {
    let mut rx = app.events.subscribe();
    ws.on_upgrade(move |socket| async move {
        let (mut tx, mut incoming) = socket.split();
        loop {
            tokio::select! {
                event = rx.recv() => match event {
                    Ok(text) if !event_is_for(&text, &who) => {}
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

/// Sessions sent to the background are left out unless `?all=1`.
async fn list_sessions(State(app): State<App>, Extension(who): Who, Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    let all = q.get("all").is_some_and(|v| v == "1" || v == "true");
    let mut list: Vec<Value> = app
        .sessions
        .read()
        .unwrap()
        .values()
        .filter(|s| all || !s.background())
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

/// `?scrollback=N` adds up to N lines of history above the screen.
async fn screen(
    State(app): State<App>,
    Extension(who): Who,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult {
    let session = need(&app, &who, &id, Scope::Read)?;
    let mut text = session.screen_text();
    if let Some(n) = q.get("scrollback").and_then(|n| n.parse::<usize>().ok()).filter(|n| *n > 0) {
        text.scrollback = Some(session.scrollback_text(n.min(10_000)));
    }
    Ok(Json(json!(text)))
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

#[derive(Deserialize)]
struct BackgroundReq {
    background: bool,
}

/// Keeps a session running but out of the default list (or brings it back).
async fn background(State(app): State<App>, Extension(who): Who, Path(id): Path<String>, Json(req): Json<BackgroundReq>) -> ApiResult {
    let session = need(&app, &who, &id, Scope::Write)?;
    session.set_background(req.background);
    app.store.audit(&who.label(), Some(&id), if req.background { "background" } else { "foreground" }, "");
    app.emit(json!({ "type": "sessions" }));
    Ok(Json(json!({ "background": req.background })))
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
    // How far up the history this client is looking; 0 follows the output.
    let mut offset: usize = 0;
    loop {
        let grid = session.grid_at(offset);
        offset = grid.offset;
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
                    "offset": grid.offset, "scrollback_len": grid.scrollback_len,
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
                    Some(Ok(Message::Text(text))) => {
                        // Scrolling is a view change for this client only.
                        if let Some(next) = scroll_request(&text, offset) {
                            if next != offset {
                                offset = next;
                                break;
                            }
                            continue;
                        }
                        // Typing returns to the live screen.
                        if offset != 0 && text.contains("\"input\"") {
                            offset = 0;
                            handle_control(&app, &session, &rights, &text);
                            break;
                        }
                        handle_control(&app, &session, &rights, &text)
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                    Some(Ok(_)) => {}
                },
            }
        }
    }
}

/// `{"type":"scroll","delta":n}` (positive = further back) or
/// `{"type":"scroll","offset":n}`. Returns the requested offset; the grid
/// clamps it to the history that exists.
fn scroll_request(text: &str, current: usize) -> Option<usize> {
    let v: Value = serde_json::from_str(text).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("scroll") {
        return None;
    }
    if let Some(offset) = v.get("offset").and_then(Value::as_u64) {
        return Some(offset as usize);
    }
    let delta = v.get("delta").and_then(Value::as_i64).unwrap_or(0);
    Some((current as i64 + delta).max(0) as usize)
}

// ---- plugins ----

#[derive(Deserialize)]
struct InstallReq {
    manifest: String,
    /// The component, base64. Absent for service plugins.
    #[serde(default)]
    component: Option<String>,
    /// The permissions the admin saw and approved.
    granted: Permissions,
}

fn plugin_json(app: &App, plugin: &crate::plugins::Loaded) -> Value {
    plugin.describe(&app.store.secret_names(&plugin.manifest.id))
}

fn need_plugin(app: &App, id: &str) -> Result<Arc<crate::plugins::Loaded>, ApiError> {
    app.plugins.get(id).ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no plugin {id}")))
}

/// Runs a plugin call off the async threads. A failure inside the plugin is
/// the plugin's answer (502), not a server error.
async fn plugin_call(app: &App, id: String, call: Call) -> Result<Value, ApiError> {
    need_plugin(app, &id)?;
    let plugins = app.plugins.clone();
    tokio::task::spawn_blocking(move || plugins.call(&id, call))
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| ApiError(StatusCode::BAD_GATEWAY, e))
}

async fn list_plugins(State(app): State<App>, Extension(who): Who) -> ApiResult {
    need_role(&who, Role::Guest)?;
    let list: Vec<Value> = app.plugins.list().iter().map(|p| plugin_json(&app, p)).collect();
    Ok(Json(json!(list)))
}

async fn get_plugin(State(app): State<App>, Extension(who): Who, Path(id): Path<String>) -> ApiResult {
    need_role(&who, Role::Guest)?;
    Ok(Json(plugin_json(&app, &*need_plugin(&app, &id)?)))
}

async fn install_plugin(State(app): State<App>, Extension(who): Who, Json(req): Json<InstallReq>) -> ApiResult {
    let (_, name) = need_role(&who, Role::Admin)?;
    use base64::Engine;
    let component = match &req.component {
        Some(text) => Some(base64::engine::general_purpose::STANDARD.decode(text.as_bytes()).map_err(|_| bad("component must be base64"))?),
        None => None,
    };
    let by = name.to_string();
    let plugins = app.plugins.clone();
    // Compiling the component takes a moment.
    let loaded = tokio::task::spawn_blocking(move || plugins.install(&req.manifest, component.as_deref(), &req.granted, &by))
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(bad)?;
    let detail = format!("{} {} {}", loaded.manifest.id, loaded.manifest.version, json!(loaded.manifest.permissions));
    app.store.audit(&who.label(), None, "plugin.install", &detail);
    let mut out = plugin_json(&app, &loaded);
    if loaded.manifest.kind == "service" {
        // Shown once, at first install: the service needs it to sign and check events.
        if let (secret, true) = app.store.events_secret(&loaded.manifest.id) {
            out["events_secret"] = json!(secret);
        }
    }
    Ok(Json(out))
}

async fn remove_plugin(State(app): State<App>, Extension(who): Who, Path(id): Path<String>) -> ApiResult {
    need_role(&who, Role::Admin)?;
    if !app.plugins.remove(&id) {
        return Err(ApiError(StatusCode::NOT_FOUND, format!("no plugin {id}")));
    }
    app.store.audit(&who.label(), None, "plugin.remove", &id);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct EnabledReq {
    enabled: bool,
}

async fn enable_plugin(State(app): State<App>, Extension(who): Who, Path(id): Path<String>, Json(req): Json<EnabledReq>) -> ApiResult {
    need_role(&who, Role::Admin)?;
    if !app.plugins.set_enabled(&id, req.enabled) {
        return Err(ApiError(StatusCode::NOT_FOUND, format!("no plugin {id}")));
    }
    app.store.audit(&who.label(), None, if req.enabled { "plugin.enable" } else { "plugin.disable" }, &id);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct SecretReq {
    value: String,
}

async fn set_secret(
    State(app): State<App>,
    Extension(who): Who,
    Path((id, name)): Path<(String, String)>,
    Json(req): Json<SecretReq>,
) -> ApiResult {
    need_role(&who, Role::Admin)?;
    let plugin = need_plugin(&app, &id)?;
    if !plugin.manifest.permissions.secrets.contains(&name) {
        return Err(bad(format!("the plugin does not declare a secret named {name}")));
    }
    if req.value.is_empty() || req.value.len() > 8192 {
        return Err(bad("value must have 1 to 8192 bytes"));
    }
    app.store.secret_set(&id, &name, &req.value).map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // The value is never written to the audit log.
    app.store.audit(&who.label(), None, "plugin.secret.set", &format!("{id} {name}"));
    Ok(Json(json!({ "ok": true })))
}

async fn delete_secret(State(app): State<App>, Extension(who): Who, Path((id, name)): Path<(String, String)>) -> ApiResult {
    need_role(&who, Role::Admin)?;
    need_plugin(&app, &id)?;
    let removed = app.store.secret_delete(&id, &name);
    app.store.audit(&who.label(), None, "plugin.secret.delete", &format!("{id} {name}"));
    Ok(Json(json!({ "ok": removed })))
}

async fn plugin_logs(State(app): State<App>, Extension(who): Who, Path(id): Path<String>) -> ApiResult {
    need_role(&who, Role::Admin)?;
    Ok(Json(json!(need_plugin(&app, &id)?.logs())))
}

async fn poll_plugin(State(app): State<App>, Extension(who): Who, Path(id): Path<String>) -> ApiResult {
    need_role(&who, Role::Member)?;
    plugin_call(&app, id, Call::Poll).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn render_panel(State(app): State<App>, Extension(who): Who, Path((id, panel)): Path<(String, String)>) -> ApiResult {
    need_role(&who, Role::Guest)?;
    Ok(Json(json!({ "tree": plugin_call(&app, id, Call::Render { panel }).await? })))
}

#[derive(Deserialize)]
struct ActionReq {
    action: String,
    #[serde(default)]
    payload: Value,
}

async fn panel_action(
    State(app): State<App>,
    Extension(who): Who,
    Path((id, panel)): Path<(String, String)>,
    Json(req): Json<ActionReq>,
) -> ApiResult {
    need_role(&who, Role::Member)?;
    let call = Call::Action { panel, action: req.action, payload: req.payload };
    Ok(Json(json!({ "tree": plugin_call(&app, id, call).await? })))
}

async fn run_command(
    State(app): State<App>,
    Extension(who): Who,
    Path((id, command)): Path<(String, String)>,
    body: Option<Json<Value>>,
) -> ApiResult {
    // People run any command. An agent only runs the ones the plugin offers
    // to agents, which the admin approved at install; each call is logged.
    if let Principal::Agent { .. } = &who {
        let plugin = need_plugin(&app, &id)?;
        if !plugin.enabled || !plugin.manifest.permissions.agent_tools.contains(&command) {
            return Err(forbidden("this command is not offered to agents"));
        }
        app.store.audit(&who.label(), None, "plugin.command", &format!("{id} {command}"));
    } else {
        need_role(&who, Role::Member)?;
    }
    let args = body.map(|Json(v)| v).unwrap_or(Value::Null);
    Ok(Json(json!({ "result": plugin_call(&app, id, Call::Command { id: command, args }).await? })))
}

/// Plugin commands that agents may call, as MCP tool definitions. Guests
/// get an empty list: they cannot run commands.
async fn agent_tools(State(app): State<App>, Extension(who): Who) -> ApiResult {
    if matches!(&who, Principal::User { role, .. } if *role < Role::Member) {
        return Ok(Json(json!({ "tools": [] })));
    }
    let mut tools = Vec::new();
    for plugin in app.plugins.list().iter().filter(|p| p.enabled) {
        for command in plugin.manifest.agent_tools() {
            tools.push(json!({
                "name": crate::plugins::manifest::tool_name(&plugin.manifest.id, &command.id),
                "plugin": plugin.manifest.id,
                "command": command.id,
                "title": command.title,
                "description": format!("{} (plugin {})", command.description.trim(), plugin.manifest.name),
                "input_schema": command.input.clone().unwrap_or_else(|| json!({ "type": "object" })),
            }));
        }
    }
    Ok(Json(json!({ "tools": tools })))
}

async fn list_items(State(app): State<App>, Extension(who): Who, Query(q): Query<ItemQuery>) -> ApiResult {
    need_role(&who, Role::Guest)?;
    Ok(Json(json!(app.store.items(&q))))
}

/// The caller's `layout.toml` as JSON; an empty object when there is none.
async fn layout(State(app): State<App>, Extension(who): Who) -> ApiResult {
    let (_, name) = need_role(&who, Role::Guest)?;
    let path = app.config.data_dir.join("users").join(name.to_ascii_lowercase()).join("layout.toml");
    let value = match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str::<toml::Table>(&text).map_err(|e| bad(format!("layout.toml: {e}")))?,
        Err(_) => toml::Table::new(),
    };
    Ok(Json(json!(value)))
}

// ---- service plugins ----

async fn jwks(State(app): State<App>) -> Json<Value> {
    Json(app.signer.jwks())
}

fn need_service(app: &App, id: &str) -> Result<Arc<crate::plugins::Loaded>, ApiError> {
    let plugin = need_plugin(app, id)?;
    if plugin.manifest.service.is_none() {
        return Err(bad(format!("{id} is not a service plugin")));
    }
    if !plugin.enabled {
        return Err(bad(format!("plugin {id} is disabled")));
    }
    Ok(plugin)
}

/// A short-lived token that tells one service plugin who is looking.
async fn plugin_token(State(app): State<App>, Extension(who): Who, Path(id): Path<String>) -> ApiResult {
    let (_, name) = need_role(&who, Role::Guest)?;
    need_service(&app, &id)?;
    let role = match &who {
        Principal::User { role, .. } => role.as_str(),
        Principal::Agent { .. } => "agent",
    };
    let token = app.signer.token(&app.config.url(), &id, name, role, &app.store.workspace_name());
    Ok(Json(json!({ "token": token, "expires_in": crate::service::TOKEN_TTL })))
}

/// Replaces the shared event secret; the new one is shown once.
async fn rotate_events_secret(State(app): State<App>, Extension(who): Who, Path(id): Path<String>) -> ApiResult {
    need_role(&who, Role::Admin)?;
    need_service(&app, &id)?;
    app.store.secret_delete(&id, crate::service::EVENTS_SECRET);
    let (secret, _) = app.store.events_secret(&id);
    app.store.audit(&who.label(), None, "plugin.events-secret.rotate", &id);
    Ok(Json(json!({ "events_secret": secret })))
}

/// Events from a service plugin, signed with the shared secret:
/// `X-Almighty-Signature: sha256=<hmac of the body>` and
/// `X-Almighty-Event-Id` (repeats are acknowledged and ignored). The
/// `X-3m4-*` names of the 3m4 platform contract are accepted too.
async fn plugin_event(State(app): State<App>, Path(id): Path<String>, headers: axum::http::HeaderMap, body: Bytes) -> Result<Response, ApiError> {
    let unknown = || ApiError(StatusCode::UNAUTHORIZED, "unknown plugin or bad signature".into());
    let header = |a: &str, b: &str| headers.get(a).or_else(|| headers.get(b)).and_then(|v| v.to_str().ok()).map(str::to_string);
    // The same answer for a missing plugin and a wrong signature.
    let plugin = app.plugins.get(&id).filter(|p| p.enabled && p.manifest.service.is_some()).ok_or_else(unknown)?;
    let secret = app.store.secret_get(&id, crate::service::EVENTS_SECRET).ok_or_else(unknown)?;
    let signature = header("x-almighty-signature", "x-3m4-signature").ok_or_else(unknown)?;
    if !crate::service::verify(&secret, &body, &signature) {
        return Err(unknown());
    }
    let event: Value = serde_json::from_slice(&body).map_err(|_| bad("the event is not JSON"))?;
    let kind = event["type"].as_str().filter(|t| !t.is_empty() && t.len() <= 100).ok_or_else(|| bad("the event has no type"))?;
    let event_id = header("x-almighty-event-id", "x-3m4-event-id").filter(|e| !e.is_empty() && e.len() <= 200).ok_or_else(|| bad("the event has no id header"))?;
    let accepted = |extra: Value| Ok((StatusCode::ACCEPTED, Json(extra)).into_response());
    if !app.store.event_is_new(&id, &event_id) {
        return accepted(json!({ "ok": true, "duplicate": true }));
    }
    let mut out = json!({ "ok": true });
    if !event["item"].is_null() {
        if !plugin.manifest.permissions.items.iter().any(|p| p == "write") {
            return Err(forbidden("the plugin has no permission to write items"));
        }
        let item: crate::plugins::db::NewItem = serde_json::from_value(event["item"].clone()).map_err(|e| bad(format!("item: {e}")))?;
        let (item_id, created) = app.store.item_upsert(&id, &item).map_err(bad)?;
        app.emit(json!({ "type": "item", "id": item_id, "created": created, "plugin": id }));
        out["item"] = json!(item_id);
    }
    // Lets open panels of this plugin refresh.
    app.emit(json!({ "type": "plugin", "id": id, "event": kind, "uri": event["uri"] }));
    accepted(out)
}

/// Finds the service plugin that opens a `3m4://<type>/<id>[@version]` resource.
async fn resolve_uri(State(app): State<App>, Extension(who): Who, Query(q): Query<HashMap<String, String>>) -> ApiResult {
    need_role(&who, Role::Guest)?;
    let uri = q.get("uri").map(String::as_str).unwrap_or("");
    let parsed = uri.strip_prefix("3m4://").and_then(|rest| rest.split_once('/')).map(|(kind, rest)| match rest.split_once('@') {
        Some((id, version)) => (kind, id, Some(version)),
        None => (kind, rest, None),
    });
    let plain = |s: &str| !s.is_empty() && s.len() <= 120 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    let Some((kind, id, version)) = parsed.filter(|(k, i, v)| plain(k) && plain(i) && v.is_none_or(plain)) else {
        return Err(bad("expected 3m4://<type>/<id>[@version]"));
    };
    for plugin in app.plugins.list() {
        let (Some(service), true) = (&plugin.manifest.service, plugin.enabled) else { continue };
        if let Some(resource) = plugin.manifest.resources.iter().find(|r| r.kind == kind) {
            let mut url = format!("{}{}", service.base_url, resource.embed.replace("{id}", id));
            if let Some(version) = version {
                url.push_str(&format!("{}version={version}", if url.contains('?') { '&' } else { '?' }));
            }
            return Ok(Json(json!({ "plugin": plugin.manifest.id, "name": plugin.manifest.name, "url": url })));
        }
    }
    Err(ApiError(StatusCode::NOT_FOUND, format!("no installed plugin opens {kind} resources")))
}

// ---- rich terminal, review, explainer ----

async fn session_media(State(app): State<App>, Extension(who): Who, Path(id): Path<String>) -> ApiResult {
    let session = need(&app, &who, &id, Scope::Read)?;
    Ok(Json(json!({ "media": session.media_list() })))
}

async fn session_media_file(State(app): State<App>, Extension(who): Who, Path((id, media)): Path<(String, u64)>) -> Result<Response, ApiError> {
    let session = need(&app, &who, &id, Scope::Read)?;
    let media = session.media_get(media).ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "no such image".into()))?;
    // The type was taken from the bytes; `nosniff` keeps a browser from second-guessing it.
    let headers = [
        (header::CONTENT_TYPE, media.mime),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::CACHE_CONTROL, "private, max-age=3600"),
    ];
    Ok((headers, media.data.as_ref().clone()).into_response())
}

/// Asks the plugin that brought an item in for the files of the change.
async fn change_of(app: &App, item_id: &str) -> Result<crate::review::Change, ApiError> {
    let item = app.store.item(item_id).ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no item {item_id}")))?;
    let plugin = need_plugin(app, &item.plugin)?;
    let review = plugin.manifest.review.clone().filter(|r| r.kinds.is_empty() || r.kinds.contains(&item.kind));
    let Some(review) = review else {
        return Err(bad(format!("items of kind {} from {} cannot be opened for review", item.kind, item.plugin)));
    };
    let args = json!({ "external_id": item.external_id, "url": item.url, "thread": item.thread });
    let value = plugin_call(app, item.plugin.clone(), Call::Command { id: review.command, args }).await?;
    serde_json::from_value(value).map_err(|e| ApiError(StatusCode::BAD_GATEWAY, format!("the plugin's review data is malformed: {e}")))
}

async fn review(State(app): State<App>, Extension(who): Who, Path(item): Path<String>) -> ApiResult {
    need_role(&who, Role::Member)?;
    Ok(Json(crate::review::report(&change_of(&app, &item).await?)))
}

/// Runs the operator's model once; `503` when none is configured.
async fn ask_model(app: &App, system: &'static str, input: String, effort: &'static str) -> Result<String, ApiError> {
    let data_dir = app.config.data_dir.clone();
    tokio::task::spawn_blocking(move || {
        let provider = crate::llm::Provider::load(&data_dir).ok_or_else(|| {
            ApiError(StatusCode::SERVICE_UNAVAILABLE, "No model is configured. Add an [llm] section to workspace.toml to use this.".into())
        })?;
        provider.complete(system, &input, effort).map_err(|e| ApiError(StatusCode::BAD_GATEWAY, e))
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
}

async fn review_summary(State(app): State<App>, Extension(who): Who, Path(item): Path<String>) -> ApiResult {
    let (_, name) = need_role(&who, Role::Member)?;
    let language = crate::triage::Prefs::load(&app.config.data_dir, name).unwrap_or_default().input_language;
    let change = change_of(&app, &item).await?;
    let (input, left_out) = crate::review::summary_input(&change, &language);
    let answer = ask_model(&app, crate::review::SUMMARY_INSTRUCTIONS, input, "medium").await?;
    let mut summary = crate::review::parse_summary(&answer).map_err(|e| ApiError(StatusCode::BAD_GATEWAY, e))?;
    // The reader must know what the model did not see.
    summary["files_left_out"] = json!(left_out);
    Ok(Json(summary))
}

#[derive(Deserialize)]
struct ExplainReq {
    text: String,
    /// Where the selection comes from, e.g. "terminal output" or a file path.
    #[serde(default)]
    context: String,
}

const MAX_EXPLAIN_CHARS: usize = 20_000;

async fn explain(State(app): State<App>, Extension(who): Who, Json(req): Json<ExplainReq>) -> ApiResult {
    let (_, name) = need_role(&who, Role::Member)?;
    if req.text.trim().is_empty() {
        return Err(bad("select some text first"));
    }
    // Refused, not cut: an explanation of half a selection would mislead.
    if req.text.chars().count() > MAX_EXPLAIN_CHARS {
        return Err(bad(format!("the selection is too long to explain at once (limit {MAX_EXPLAIN_CHARS} characters)")));
    }
    let language = crate::triage::Prefs::load(&app.config.data_dir, name).unwrap_or_default().input_language;
    let input = json!({
        "language": if language.is_empty() { "the language of the selection, or English for code" } else { language.as_str() },
        "context": req.context.chars().take(300).collect::<String>(),
        "selection": req.text,
    })
    .to_string();
    let answer = ask_model(&app, crate::review::EXPLAIN_INSTRUCTIONS, input, "low").await?;
    Ok(Json(json!({ "explanation": answer.trim() })))
}

// ---- push ----

async fn push_list(State(app): State<App>, Extension(who): Who) -> ApiResult {
    let (id, _) = need_role(&who, Role::Member)?;
    let policy = crate::push::Policy::load(&app.config.data_dir);
    Ok(Json(json!({
        "endpoints": app.store.push_list(id),
        "allowed_hosts": policy.allowed_hosts,
        "content": if policy.summary { "summary" } else { "wake" },
    })))
}

#[derive(Deserialize)]
struct PushReq {
    url: String,
    #[serde(default)]
    label: String,
}

async fn push_add(State(app): State<App>, Extension(who): Who, Json(req): Json<PushReq>) -> ApiResult {
    let (id, _) = need_role(&who, Role::Member)?;
    let url = req.url.trim();
    crate::push::Policy::load(&app.config.data_dir).check(url).map_err(bad)?;
    let endpoint = app.store.push_add(id, url, req.label.trim()).map_err(bad)?;
    Ok(Json(json!({ "id": endpoint })))
}

async fn push_remove(State(app): State<App>, Extension(who): Who, Path(endpoint): Path<i64>) -> ApiResult {
    let (id, _) = need_role(&who, Role::Member)?;
    if !app.store.push_remove(id, endpoint) {
        return Err(ApiError(StatusCode::NOT_FOUND, "no such endpoint".into()));
    }
    Ok(Json(json!({ "ok": true })))
}

/// Sends a test push to the caller's endpoints and reports what happened.
async fn push_test(State(app): State<App>, Extension(who): Who) -> ApiResult {
    let (id, _) = need_role(&who, Role::Member)?;
    let (store, data_dir) = (app.store.clone(), app.config.data_dir.clone());
    let delivered = tokio::task::spawn_blocking(move || crate::push::send(&store, &data_dir, id, "almighty test", "Push works."))
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "delivered": delivered, "endpoints": app.store.push_list(id) })))
}

// ---- planner and digest ----

async fn planner(State(app): State<App>, Extension(who): Who, Query(q): Query<HashMap<String, String>>) -> ApiResult {
    let (id, name) = need_role(&who, Role::Member)?;
    Ok(Json(crate::planner::plan(&app.triage, id, name, q.get("done").is_some_and(|v| v == "1"))))
}

async fn planner_add(State(app): State<App>, Extension(who): Who, Json(new): Json<crate::planner::NewTask>) -> ApiResult {
    let (id, name) = need_role(&who, Role::Member)?;
    let task = app.store.task_add(id, &new).map_err(bad)?;
    app.emit(json!({ "type": "planner", "user": name }));
    Ok(Json(json!({ "task": task })))
}

async fn planner_change(
    State(app): State<App>,
    Extension(who): Who,
    Path(task): Path<i64>,
    Json(change): Json<crate::planner::TaskChange>,
) -> ApiResult {
    let (id, name) = need_role(&who, Role::Member)?;
    let task = app.store.task_change(id, task, &change).map_err(|e| {
        if e == "no such task" {
            ApiError(StatusCode::NOT_FOUND, e)
        } else {
            bad(e)
        }
    })?;
    app.emit(json!({ "type": "planner", "user": name }));
    Ok(Json(json!({ "task": task })))
}

async fn planner_delete(State(app): State<App>, Extension(who): Who, Path(task): Path<i64>) -> ApiResult {
    let (id, name) = need_role(&who, Role::Member)?;
    if !app.store.task_delete(id, task) {
        return Err(ApiError(StatusCode::NOT_FOUND, "no such task".into()));
    }
    app.emit(json!({ "type": "planner", "user": name }));
    Ok(Json(json!({ "ok": true })))
}

fn digest_hours(q: &HashMap<String, String>) -> u64 {
    q.get("hours").and_then(|h| h.parse().ok()).unwrap_or(24)
}

async fn digest(State(app): State<App>, Extension(who): Who, Query(q): Query<HashMap<String, String>>) -> ApiResult {
    let (id, name) = need_role(&who, Role::Member)?;
    Ok(Json(crate::planner::digest(&app.triage, id, name, digest_hours(&q))))
}

/// The digest written out by the operator's model, on request.
async fn digest_summary(State(app): State<App>, Extension(who): Who, Query(q): Query<HashMap<String, String>>) -> ApiResult {
    let (id, name) = need_role(&who, Role::Member)?;
    let digest = crate::planner::digest(&app.triage, id, name, digest_hours(&q));
    if digest["total"] == 0 {
        return Ok(Json(json!({ "summary": "", "total": 0 })));
    }
    let language = crate::triage::Prefs::load(&app.config.data_dir, name).unwrap_or_default().input_language;
    let input = json!({
        "language": if language.is_empty() { "English" } else { language.as_str() },
        "groups": digest["groups"],
    })
    .to_string();
    let answer = ask_model(&app, crate::planner::DIGEST_INSTRUCTIONS, input, "low").await?;
    Ok(Json(json!({ "summary": answer.trim(), "total": digest["total"] })))
}

// ---- inbox ----

async fn inbox(State(app): State<App>, Extension(who): Who, Query(q): Query<crate::triage::InboxQuery>) -> ApiResult {
    let (id, name) = need_role(&who, Role::Member)?;
    Ok(Json(json!({ "items": app.triage.inbox(id, name, &q), "counts": app.triage.counts(id, name) })))
}

async fn inbox_counts(State(app): State<App>, Extension(who): Who) -> ApiResult {
    let (id, name) = need_role(&who, Role::Member)?;
    Ok(Json(app.triage.counts(id, name)))
}

/// The caller's triage preferences as the server reads them, and where the file is.
async fn inbox_preferences(State(app): State<App>, Extension(who): Who) -> ApiResult {
    let (_, name) = need_role(&who, Role::Member)?;
    let prefs = crate::triage::Prefs::load(&app.config.data_dir, name).map_err(bad)?;
    let path = crate::triage::Prefs::path(&app.config.data_dir, name);
    Ok(Json(json!({ "preferences": prefs, "file": path.to_string_lossy() })))
}

async fn inbox_change(
    State(app): State<App>,
    Extension(who): Who,
    Path(item): Path<String>,
    Json(change): Json<crate::triage::Change>,
) -> ApiResult {
    let (id, name) = need_role(&who, Role::Member)?;
    let row = app.triage.change(id, name, &item, &change).map_err(|e| {
        if e.starts_with("no item") {
            ApiError(StatusCode::NOT_FOUND, e)
        } else {
            bad(e)
        }
    })?;
    Ok(Json(json!({ "triage": row })))
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
