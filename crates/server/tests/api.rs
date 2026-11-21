//! API behaviour: who can do what. Runs the router in-process.

use almighty_server::store::{Role, Store};
use almighty_server::{build, App, Config};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const PASSWORD: &str = "correct-horse-battery";

fn app() -> (Router, App) {
    let config = Config { addr: ([127, 0, 0, 1], 0).into(), data_dir: std::env::temp_dir() };
    build(config, Store::in_memory().unwrap())
}

async fn call(router: &Router, method: &str, path: &str, token: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(path);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let req = match body {
        Some(b) => req.header("content-type", "application/json").body(Body::from(b.to_string())).unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let res = router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn login(router: &Router, name: &str) -> String {
    let (status, body) = call(router, "POST", "/api/auth/login", None, Some(json!({ "name": name, "password": PASSWORD }))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["token"].as_str().unwrap().to_string()
}

/// A shell that stays alive and prints its agent token on request.
fn shell() -> Value {
    if cfg!(windows) {
        json!({ "command": ["cmd.exe"] })
    } else {
        json!({ "command": ["sh"] })
    }
}

#[tokio::test]
async fn setup_login_and_tokens() {
    let (router, app) = app();

    let (status, ws) = call(&router, "GET", "/api/workspace", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ws["setup_needed"], true);

    // Nothing works without a token.
    assert_eq!(call(&router, "GET", "/api/sessions", None, None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(call(&router, "GET", "/api/sessions", Some("alm_login_bogus"), None).await.0, StatusCode::UNAUTHORIZED);

    // Setup needs the code printed by the server.
    let wrong = json!({ "code": "nope", "name": "ana", "password": PASSWORD });
    assert_eq!(call(&router, "POST", "/api/setup", None, Some(wrong)).await.0, StatusCode::FORBIDDEN);
    let short = json!({ "code": app.setup_code, "name": "ana", "password": "short" });
    assert_eq!(call(&router, "POST", "/api/setup", None, Some(short)).await.0, StatusCode::BAD_REQUEST);
    let good = json!({ "code": app.setup_code, "name": "ana", "password": PASSWORD, "workspace": "home" });
    let (status, body) = call(&router, "POST", "/api/setup", None, Some(good.clone())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let owner = body["token"].as_str().unwrap().to_string();
    // Only once.
    assert_eq!(call(&router, "POST", "/api/setup", None, Some(good)).await.0, StatusCode::CONFLICT);

    let (_, me) = call(&router, "GET", "/api/auth/me", Some(&owner), None).await;
    assert_eq!(me, json!({ "kind": "user", "name": "ana", "role": "owner", "workspace": "home" }));

    // Wrong password, then lockout after repeated failures.
    for _ in 0..5 {
        let bad = json!({ "name": "ana", "password": "wrong-password-xx" });
        assert_eq!(call(&router, "POST", "/api/auth/login", None, Some(bad)).await.0, StatusCode::UNAUTHORIZED);
    }
    let ok = json!({ "name": "ana", "password": PASSWORD });
    assert_eq!(call(&router, "POST", "/api/auth/login", None, Some(ok)).await.0, StatusCode::TOO_MANY_REQUESTS);

    // API token: shown once, works, can be revoked.
    let (status, created) = call(&router, "POST", "/api/tokens", Some(&owner), Some(json!({ "name": "script" }))).await;
    assert_eq!(status, StatusCode::OK);
    let api = created["token"].as_str().unwrap().to_string();
    assert!(api.starts_with("alm_api_"));
    assert_eq!(call(&router, "GET", "/api/sessions", Some(&api), None).await.0, StatusCode::OK);
    let (_, listed) = call(&router, "GET", "/api/tokens", Some(&owner), None).await;
    assert!(listed.to_string().find(&api).is_none(), "token values are never listed");
    let path = format!("/api/tokens/{}", created["id"]);
    assert_eq!(call(&router, "DELETE", &path, Some(&owner), None).await.0, StatusCode::OK);
    assert_eq!(call(&router, "GET", "/api/sessions", Some(&api), None).await.0, StatusCode::UNAUTHORIZED);

    // Logout ends the login token.
    assert_eq!(call(&router, "POST", "/api/auth/logout", Some(&owner), None).await.0, StatusCode::OK);
    assert_eq!(call(&router, "GET", "/api/auth/me", Some(&owner), None).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn roles() {
    let (router, app) = app();
    app.store.create_user("ana", PASSWORD, Role::Owner).unwrap();
    app.store.create_user("adm", PASSWORD, Role::Admin).unwrap();
    app.store.create_user("mem", PASSWORD, Role::Member).unwrap();
    app.store.create_user("gus", PASSWORD, Role::Guest).unwrap();
    let (admin, member, guest) = (login(&router, "adm").await, login(&router, "mem").await, login(&router, "gus").await);

    // Users and audit are for admins.
    assert_eq!(call(&router, "GET", "/api/users", Some(&member), None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(call(&router, "GET", "/api/audit", Some(&member), None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(call(&router, "GET", "/api/users", Some(&admin), None).await.0, StatusCode::OK);
    // An admin cannot mint an owner.
    let new_owner = json!({ "name": "eve", "password": PASSWORD, "role": "owner" });
    assert_eq!(call(&router, "POST", "/api/users", Some(&admin), Some(new_owner)).await.0, StatusCode::FORBIDDEN);
    let new_member = json!({ "name": "bob", "password": PASSWORD, "role": "member" });
    assert_eq!(call(&router, "POST", "/api/users", Some(&admin), Some(new_member)).await.0, StatusCode::OK);

    // Guests look, members act.
    assert_eq!(call(&router, "POST", "/api/sessions", Some(&guest), Some(shell())).await.0, StatusCode::FORBIDDEN);
    let (status, s) = call(&router, "POST", "/api/sessions", Some(&member), Some(shell())).await;
    assert_eq!(status, StatusCode::OK, "{s}");
    let id = s["id"].as_str().unwrap();
    assert_eq!(s["created_by"], "user:mem");
    assert_eq!(call(&router, "GET", &format!("/api/sessions/{id}/screen"), Some(&guest), None).await.0, StatusCode::OK);
    let typed = json!({ "text": "x" });
    assert_eq!(call(&router, "POST", &format!("/api/sessions/{id}/input"), Some(&guest), Some(typed.clone())).await.0, StatusCode::FORBIDDEN);
    assert_eq!(call(&router, "POST", &format!("/api/sessions/{id}/input"), Some(&member), Some(typed)).await.0, StatusCode::OK);
    assert_eq!(call(&router, "POST", &format!("/api/sessions/{id}/lock"), Some(&guest), Some(json!({ "locked": true }))).await.0, StatusCode::FORBIDDEN);
    assert_eq!(call(&router, "DELETE", &format!("/api/sessions/{id}"), Some(&guest), None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(call(&router, "DELETE", &format!("/api/sessions/{id}"), Some(&member), None).await.0, StatusCode::OK);
}

#[tokio::test]
async fn agents_only_reach_what_they_own_or_were_granted() {
    let (router, app) = app();
    app.store.create_user("ana", PASSWORD, Role::Owner).unwrap();
    let user = login(&router, "ana").await;

    // Two sessions started by a person: A hosts an agent, B is unrelated.
    let (_, a) = call(&router, "POST", "/api/sessions", Some(&user), Some(shell())).await;
    let (_, b) = call(&router, "POST", "/api/sessions", Some(&user), Some(shell())).await;
    let (a, b) = (a["id"].as_str().unwrap().to_string(), b["id"].as_str().unwrap().to_string());
    // The token the process inside A would find in its environment.
    let (_, agent) = app.store.create_agent_token(&a).unwrap();

    let (_, me) = call(&router, "GET", "/api/auth/me", Some(&agent), None).await;
    assert_eq!(me["kind"], "agent");
    assert_eq!(me["session"], a);

    // It sees itself, read-only; B does not exist for it.
    let (_, list) = call(&router, "GET", "/api/sessions", Some(&agent), None).await;
    let ids: Vec<&str> = list["sessions"].as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec![a.as_str()]);
    let typed = json!({ "keys": ["Enter"] });
    assert_eq!(call(&router, "POST", &format!("/api/sessions/{a}/input"), Some(&agent), Some(typed.clone())).await.0, StatusCode::FORBIDDEN);
    assert_eq!(call(&router, "GET", &format!("/api/sessions/{b}/screen"), Some(&agent), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&router, "POST", &format!("/api/sessions/{b}/input"), Some(&agent), Some(typed.clone())).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&router, "DELETE", &format!("/api/sessions/{b}"), Some(&agent), None).await.0, StatusCode::NOT_FOUND);

    // Agents cannot lock, grant, manage accounts or mint tokens.
    assert_eq!(call(&router, "POST", &format!("/api/sessions/{a}/lock"), Some(&agent), Some(json!({ "locked": false }))).await.0, StatusCode::FORBIDDEN);
    assert_eq!(call(&router, "POST", &format!("/api/sessions/{a}/grants"), Some(&agent), Some(json!({ "agent": a, "scope": "write" }))).await.0, StatusCode::FORBIDDEN);
    assert_eq!(call(&router, "POST", "/api/tokens", Some(&agent), Some(json!({ "name": "x" }))).await.0, StatusCode::FORBIDDEN);
    assert_eq!(call(&router, "GET", "/api/users", Some(&agent), None).await.0, StatusCode::FORBIDDEN);

    // Hooks: only for its own session.
    let hook = json!({ "hook_event_name": "SubagentStart", "agent_id": "x1", "agent_type": "Explore" });
    assert_eq!(call(&router, "POST", &format!("/api/hooks/claude?session={a}"), Some(&agent), Some(hook.clone())).await.0, StatusCode::OK);
    assert_eq!(call(&router, "POST", &format!("/api/hooks/claude?session={b}"), Some(&agent), Some(hook)).await.0, StatusCode::FORBIDDEN);
    let (_, tree) = call(&router, "GET", &format!("/api/sessions/{a}/agents"), Some(&user), None).await;
    assert_eq!(tree["agents"][0]["agent_id"], "x1");

    // A session the agent starts is its own: full control.
    let (status, child) = call(&router, "POST", "/api/sessions", Some(&agent), Some(shell())).await;
    assert_eq!(status, StatusCode::OK, "{child}");
    let c = child["id"].as_str().unwrap().to_string();
    assert_eq!(child["created_by"], format!("agent:{a}"));
    assert_eq!(call(&router, "POST", &format!("/api/sessions/{c}/input"), Some(&agent), Some(json!({ "text": "echo hi" }))).await.0, StatusCode::OK);

    // A person grants read, then write, on B; and takes it away.
    let grant = |scope: Value| json!({ "agent": a, "scope": scope });
    assert_eq!(call(&router, "POST", &format!("/api/sessions/{b}/grants"), Some(&user), Some(grant(json!("read")))).await.0, StatusCode::OK);
    assert_eq!(call(&router, "GET", &format!("/api/sessions/{b}/screen"), Some(&agent), None).await.0, StatusCode::OK);
    assert_eq!(call(&router, "POST", &format!("/api/sessions/{b}/input"), Some(&agent), Some(typed.clone())).await.0, StatusCode::FORBIDDEN);
    assert_eq!(call(&router, "POST", &format!("/api/sessions/{b}/grants"), Some(&user), Some(grant(json!("write")))).await.0, StatusCode::OK);
    assert_eq!(call(&router, "POST", &format!("/api/sessions/{b}/input"), Some(&agent), Some(typed.clone())).await.0, StatusCode::OK);

    // A locked session refuses the agent even with a write grant.
    assert_eq!(call(&router, "POST", &format!("/api/sessions/{b}/lock"), Some(&user), Some(json!({ "locked": true }))).await.0, StatusCode::OK);
    assert_eq!(call(&router, "POST", &format!("/api/sessions/{b}/input"), Some(&agent), Some(typed.clone())).await.0, StatusCode::CONFLICT);
    assert_eq!(call(&router, "POST", &format!("/api/sessions/{b}/lock"), Some(&user), Some(json!({ "locked": false }))).await.0, StatusCode::OK);
    assert_eq!(call(&router, "POST", &format!("/api/sessions/{b}/grants"), Some(&user), Some(grant(Value::Null))).await.0, StatusCode::OK);
    assert_eq!(call(&router, "GET", &format!("/api/sessions/{b}/screen"), Some(&agent), None).await.0, StatusCode::NOT_FOUND);

    // What the agent typed is in the audit log; what the person typed is not.
    call(&router, "POST", &format!("/api/sessions/{b}/input"), Some(&user), Some(json!({ "text": "human" }))).await;
    let (_, log) = call(&router, "GET", "/api/audit", Some(&user), None).await;
    let inputs: Vec<&Value> = log["entries"].as_array().unwrap().iter().filter(|e| e["action"] == "input").collect();
    assert_eq!(inputs.len(), 2, "{log}");
    assert!(inputs.iter().all(|e| e["actor"] == format!("agent:{a}")));
    assert_eq!(inputs[0]["detail"], "text: \"echo hi\"");

    // Closing A ends its token.
    assert_eq!(call(&router, "DELETE", &format!("/api/sessions/{a}"), Some(&user), None).await.0, StatusCode::OK);
    let (_, token_of_a) = app.store.create_agent_token("gone").unwrap();
    app.store.delete_agent_tokens("gone");
    assert_eq!(call(&router, "GET", "/api/auth/me", Some(&token_of_a), None).await.0, StatusCode::UNAUTHORIZED);
    for id in [b, c] {
        call(&router, "DELETE", &format!("/api/sessions/{id}"), Some(&user), None).await;
    }
}

#[tokio::test]
async fn a_session_gets_its_own_token_and_cannot_be_given_another() {
    if cfg!(windows) {
        return; // uses a POSIX shell to read the environment
    }
    let (router, app) = app();
    app.store.create_user("ana", PASSWORD, Role::Owner).unwrap();
    let user = login(&router, "ana").await;
    // The caller tries to plant its own (user) token in the session.
    let req = json!({ "command": ["sh", "-c", "echo tok=$ALMIGHTY_TOKEN; sleep 5"], "env": { "ALMIGHTY_TOKEN": user } });
    let (_, s) = call(&router, "POST", "/api/sessions", Some(&user), Some(req)).await;
    let id = s["id"].as_str().unwrap();
    let wait = json!({ "pattern": "^tok=alm_", "timeout_ms": 5000 });
    let (_, out) = call(&router, "POST", &format!("/api/sessions/{id}/wait"), Some(&user), Some(wait)).await;
    let text = out["screen"]["text"].as_str().unwrap().replace('\n', "");
    let token = text.split("tok=").nth(1).unwrap().trim().to_string();
    assert!(token.starts_with("alm_agent_"), "{text}");
    let (_, me) = call(&router, "GET", "/api/auth/me", Some(&token), None).await;
    assert_eq!(me["session"], *id);
    call(&router, "DELETE", &format!("/api/sessions/{id}"), Some(&user), None).await;
    assert_eq!(call(&router, "GET", "/api/auth/me", Some(&token), None).await.0, StatusCode::UNAUTHORIZED);
}
