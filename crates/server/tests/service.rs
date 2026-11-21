//! Service plugins: install, identity tokens, signed events in both
//! directions, and resource links. A local HTTP listener stands in for the
//! service.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use almighty_server::service::{sign, verify};
use almighty_server::store::{Role, Store};
use almighty_server::{build, App, Config};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use base64::Engine;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const ID: &str = "net.ema.flows";

fn data_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("almighty-service-{:016x}", rand::random::<u64>()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn app_in(dir: &Path) -> (Router, App) {
    build(Config { addr: ([127, 0, 0, 1], 0).into(), data_dir: dir.to_path_buf() }, Store::in_memory().unwrap())
}

async fn send(router: &Router, method: &str, path: &str, headers: &[(&str, &str)], body: Option<String>) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    let req = match body {
        Some(b) => req.header("content-type", "application/json").body(Body::from(b)).unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let res = router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn call(router: &Router, method: &str, path: &str, token: &str, body: Option<Value>) -> (StatusCode, Value) {
    send(router, method, path, &[("authorization", &format!("Bearer {token}"))], body.map(|b| b.to_string())).await
}

fn token(app: &App, name: &str, role: Role) -> String {
    let id = app.store.create_user(name, "correct-horse-battery", role).unwrap();
    app.store.create_user_token(id, "api", "test", None).unwrap().1
}

fn manifest(port: u16, permissions: &str) -> String {
    format!(
        "id = \"{ID}\"\nname = \"flows\"\nversion = \"0.1.0\"\nkind = \"service\"\n\n[service]\nbase_url = \"http://127.0.0.1:{port}\"\nmcp = \"/mcp\"\n\n\
         [permissions]\n{permissions}\n\n[[panels]]\nid = \"runs\"\ntitle = \"Runs\"\ntype = \"web\"\nurl = \"/embed/runs\"\n\n\
         [[resources]]\ntype = \"workflows\"\nembed = \"/embed/workflows/{{id}}\"\n"
    )
}

async fn install(router: &Router, admin: &str, text: &str) -> Value {
    let granted = toml::from_str::<toml::Table>(text).unwrap()["permissions"].clone();
    let (status, out) = call(router, "POST", "/api/plugins", admin, Some(json!({ "manifest": text, "granted": granted }))).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    out
}

/// Records every request and answers 202.
fn service() -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut request = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&request).to_string();
                if let Some(head_end) = text.find("\r\n\r\n") {
                    let length = text[..head_end]
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                        .unwrap_or(0);
                    if request.len() >= head_end + 4 + length {
                        break;
                    }
                }
            }
            seen2.lock().unwrap().push(String::from_utf8_lossy(&request).to_string());
            let _ = stream.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    });
    (port, seen)
}

#[tokio::test(flavor = "multi_thread")]
async fn install_tokens_and_links() {
    let dir = data_dir();
    let (router, app) = app_in(&dir);
    let admin = token(&app, "ana", Role::Admin);
    let guest = token(&app, "gus", Role::Guest);

    let out = install(&router, &admin, &manifest(8087, "items = [\"write\"]")).await;
    assert_eq!((out["kind"].as_str(), out["base_url"].as_str()), (Some("service"), Some("http://127.0.0.1:8087")));
    assert_eq!(out["mcp_url"], "http://127.0.0.1:8087/mcp");
    assert_eq!(out["panels"][0]["url"], "/embed/runs");
    let secret = out["events_secret"].as_str().unwrap().to_string();
    assert!(secret.starts_with("alm_evt_"));
    // The secret is shown once and is not listed with the plugin's secrets.
    let again = install(&router, &admin, &manifest(8087, "items = [\"write\"]")).await;
    assert!(again.get("events_secret").is_none());
    assert_eq!(again["secrets_set"], json!([]));
    // A component makes no sense for a service; calls are refused clearly.
    let bad = json!({ "manifest": manifest(8087, ""), "component": "AAAA", "granted": {} });
    assert_eq!(call(&router, "POST", "/api/plugins", &admin, Some(bad)).await.0, StatusCode::BAD_REQUEST);
    let (status, err) = call(&router, "POST", &format!("/api/plugins/{ID}/poll"), &admin, None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(err["error"].as_str().unwrap().contains("service plugin"), "{err}");

    // The public key is public; tokens name the viewer and only this plugin.
    let (status, jwks) = send(&router, "GET", "/.well-known/jwks.json", &[], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(jwks["keys"][0]["crv"], "Ed25519");
    let (status, out) = call(&router, "POST", &format!("/api/plugins/{ID}/token"), &guest, None).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let jwt = out["token"].as_str().unwrap();
    let claims: Value = serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(jwt.split('.').nth(1).unwrap()).unwrap()).unwrap();
    assert_eq!((claims["aud"].as_str(), claims["sub"].as_str(), claims["role"].as_str()), (Some(ID), Some("gus"), Some("guest")));
    {
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};
        let decode = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).unwrap();
        let key = VerifyingKey::from_bytes(&<[u8; 32]>::try_from(decode(jwks["keys"][0]["x"].as_str().unwrap()).as_slice()).unwrap()).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert!(key.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &Signature::from_slice(&decode(parts[2])).unwrap()).is_ok());
    }
    assert_eq!(call(&router, "POST", "/api/plugins/net.nope.x/token", &guest, None).await.0, StatusCode::NOT_FOUND);

    // Resource links.
    let (status, out) = call(&router, "GET", "/api/resolve?uri=3m4://workflows/nightly@3", &guest, None).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["url"], "http://127.0.0.1:8087/embed/workflows/nightly?version=3");
    assert_eq!(call(&router, "GET", "/api/resolve?uri=3m4://skills/x", &guest, None).await.0, StatusCode::NOT_FOUND);
    for bad in ["3m4://workflows/../admin", "3m4://workflows/a%2Fb", "https://example.com", "3m4://workflows/"] {
        assert_eq!(call(&router, "GET", &format!("/api/resolve?uri={bad}"), &guest, None).await.0, StatusCode::BAD_REQUEST, "{bad}");
    }

    // Rotating the secret needs an admin and invalidates the old one.
    assert_eq!(call(&router, "POST", &format!("/api/plugins/{ID}/events-secret"), &guest, None).await.0, StatusCode::FORBIDDEN);
    let (_, rotated) = call(&router, "POST", &format!("/api/plugins/{ID}/events-secret"), &admin, None).await;
    assert_ne!(rotated["events_secret"].as_str().unwrap(), secret);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn inbound_events_are_signed_and_deduplicated() {
    let dir = data_dir();
    let (router, app) = app_in(&dir);
    let admin = token(&app, "ana", Role::Admin);
    let secret = install(&router, &admin, &manifest(8087, "items = [\"write\"]")).await["events_secret"].as_str().unwrap().to_string();
    let hook = format!("/hooks/{ID}");
    let body = json!({
        "type": "run.failed", "uri": "3m4://runs/01ABC",
        "item": { "external_id": "run-01ABC", "title": "Nightly build failed", "kind": "task", "direct": true, "url": "http://127.0.0.1:8087/runs/01ABC" },
    })
    .to_string();
    let signature = sign(&secret, body.as_bytes());
    assert!(verify(&secret, body.as_bytes(), &signature));

    // No signature, a wrong one, a signature of another body, an unknown plugin: all the same answer.
    assert_eq!(send(&router, "POST", &hook, &[("x-almighty-event-id", "e1")], Some(body.clone())).await.0, StatusCode::UNAUTHORIZED);
    let wrong = sign("other", body.as_bytes());
    assert_eq!(send(&router, "POST", &hook, &[("x-almighty-signature", &wrong), ("x-almighty-event-id", "e1")], Some(body.clone())).await.0, StatusCode::UNAUTHORIZED);
    let tampered = body.replace("failed", "passed");
    assert_eq!(send(&router, "POST", &hook, &[("x-almighty-signature", &signature), ("x-almighty-event-id", "e1")], Some(tampered)).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(send(&router, "POST", "/hooks/net.nope.x", &[("x-almighty-signature", &signature), ("x-almighty-event-id", "e1")], Some(body.clone())).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(send(&router, "POST", &hook, &[("x-almighty-signature", &signature)], Some(body.clone())).await.0, StatusCode::BAD_REQUEST, "an id is required");

    let (status, out) = send(&router, "POST", &hook, &[("x-almighty-signature", &signature), ("x-almighty-event-id", "e1")], Some(body.clone())).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{out}");
    assert!(out["item"].as_str().unwrap().starts_with("it_"));
    // A retry of the same event is acknowledged and changes nothing; the 3m4 header names work too.
    let (status, out) = send(&router, "POST", &hook, &[("x-3m4-signature", &signature), ("x-3m4-event-id", "e1")], Some(body.clone())).await;
    assert_eq!((status, out["duplicate"].as_bool()), (StatusCode::ACCEPTED, Some(true)));

    // The item reaches the inbox of the people of the workspace.
    let (_, inbox) = call(&router, "GET", "/api/inbox?bucket=act", &admin, None).await;
    assert_eq!(inbox["items"][0]["item"]["title"], "Nightly build failed", "{inbox}");
    assert_eq!(inbox["items"][0]["item"]["plugin"], ID);

    // Without `items = ["write"]` a service can announce things but not create items.
    install(&router, &admin, &manifest(8087, "")).await;
    let (status, _) = send(&router, "POST", &hook, &[("x-almighty-signature", &signature), ("x-almighty-event-id", "e2")], Some(body.clone())).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let plain = json!({ "type": "workflow.updated", "uri": "3m4://workflows/nightly@4" }).to_string();
    let (status, _) = send(&router, "POST", &hook, &[("x-almighty-signature", &sign(&secret, plain.as_bytes())), ("x-almighty-event-id", "e3")], Some(plain)).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // A disabled plugin is as good as absent.
    call(&router, "POST", &format!("/api/plugins/{ID}/enabled"), &admin, Some(json!({ "enabled": false }))).await;
    assert_eq!(send(&router, "POST", &hook, &[("x-almighty-signature", &signature), ("x-almighty-event-id", "e4")], Some(body)).await.0, StatusCode::UNAUTHORIZED);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn outbound_events_reach_subscribed_services() {
    let dir = data_dir();
    let (router, app) = app_in(&dir);
    let admin = token(&app, "ana", Role::Admin);
    let (port, seen) = service();
    let secret = install(&router, &admin, &manifest(port, "items = [\"write\"]\nevents = [\"item.created\"]")).await["events_secret"].as_str().unwrap().to_string();
    almighty_server::service::spawn_outbound(app.plugins.clone(), app.store.clone(), app.triage.events.clone(), || "home".to_string());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let emit = |plugin: &str, external: &str, title: &str| {
        let (id, created) = app.store.item_upsert(plugin, &serde_json::from_value(json!({ "external_id": external, "title": title, "body": "long private text" })).unwrap()).unwrap();
        let _ = app.triage.events.send(json!({ "type": "item", "id": id, "created": created, "plugin": plugin }).to_string());
    };
    emit("net.ema.github", "pr1", "Fix login");
    emit("net.ema.github", "pr1", "Fix login (updated)"); // an update: not subscribed
    emit(ID, "own", "From the service itself"); // its own item: not echoed

    for _ in 0..50 {
        if !seen.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    let request = &seen[0];
    assert!(request.starts_with("POST /api/v1/events"), "{request}");
    let body = &request[request.find("\r\n\r\n").unwrap() + 4..];
    let header = |name: &str| request.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix(&format!("{name}: ")).map(str::to_string)).unwrap();
    assert!(verify(&secret, body.as_bytes(), &header("x-almighty-signature")), "the service can check where it came from");
    assert!(!header("x-almighty-event-id").is_empty());
    let event: Value = serde_json::from_str(body).unwrap();
    assert_eq!((event["type"].as_str(), event["workspace"].as_str()), (Some("item.created"), Some("home")));
    // The event carries the item as it is when sent, which may already include the update.
    assert!(event["data"]["title"].as_str().unwrap().starts_with("Fix login"), "{event}");
    assert!(!body.contains("long private text"), "only the light version leaves");
    let _ = std::fs::remove_dir_all(dir);
}
