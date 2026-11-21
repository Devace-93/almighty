//! Plugin host, end to end: the example plugin is compiled to a WebAssembly
//! component and driven through the API. Needs the `wasm32-wasip2` target
//! (`rustup target add wasm32-wasip2`).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::OnceLock;

use almighty_server::store::{Role, Store};
use almighty_server::{build, App, Config};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use base64::Engine;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const PASSWORD: &str = "correct-horse-battery";
const ID: &str = "net.ema.example";

fn example_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/example")
}

/// Builds the example plugin once per test run.
fn component() -> &'static Vec<u8> {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    BYTES.get_or_init(|| build_plugin("example"))
}

/// Compiles `plugins/<name>` to a component and returns its bytes.
fn build_plugin(name: &str) -> Vec<u8> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins").join(name);
    let status = std::process::Command::new(env!("CARGO"))
        .args(["build", "--release", "--target", "wasm32-wasip2"])
        .current_dir(&dir)
        .env_remove("CARGO_TARGET_DIR")
        .status()
        .expect("run cargo");
    assert!(status.success(), "building the {name} plugin failed");
    std::fs::read(dir.join(format!("target/wasm32-wasip2/release/almighty_{name}_plugin.wasm"))).unwrap()
}

fn manifest() -> String {
    std::fs::read_to_string(example_dir().join("almighty-plugin.toml")).unwrap()
}

fn data_dir() -> PathBuf {
    let bytes: [u8; 8] = rand::random();
    let name: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let dir = std::env::temp_dir().join(format!("almighty-test-{name}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn app_in(dir: &std::path::Path) -> (Router, App) {
    let config = Config { addr: ([127, 0, 0, 1], 0).into(), data_dir: dir.to_path_buf() };
    build(config, Store::open(&dir.join("almighty.db")).unwrap())
}

async fn call(router: &Router, method: &str, path: &str, token: &str, body: Option<Value>) -> (StatusCode, Value) {
    let req = Request::builder().method(method).uri(path).header("authorization", format!("Bearer {token}"));
    let req = match body {
        Some(b) => req.header("content-type", "application/json").body(Body::from(b.to_string())).unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let res = router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn token(app: &App, name: &str, role: Role) -> String {
    let id = app.store.create_user(name, PASSWORD, role).unwrap();
    app.store.create_user_token(id, "api", "test", None).unwrap().1
}

fn install_body() -> Value {
    let m: toml::Table = toml::from_str(&manifest()).unwrap();
    json!({
        "manifest": manifest(),
        "component": base64::engine::general_purpose::STANDARD.encode(component()),
        "granted": m["permissions"],
    })
}

async fn installed(dir: &std::path::Path) -> (Router, App, String) {
    let (router, app) = app_in(dir);
    let admin = token(&app, "ana", Role::Admin);
    let (status, body) = call(&router, "POST", "/api/plugins", &admin, Some(install_body())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    (router, app, admin)
}

/// Answers every request with what `handler` returns for the raw request
/// text (status, body); returns its port.
fn http_server(handler: impl Fn(&str) -> (u16, String) + Send + 'static) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            // Headers, then as many body bytes as Content-Length says: they may arrive apart.
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
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
            let (status, body) = handler(&String::from_utf8_lossy(&request));
            let head = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let _ = stream.write_all(format!("{head}{body}").as_bytes());
        }
    });
    port
}

#[tokio::test(flavor = "multi_thread")]
async fn install_needs_an_admin_and_an_exact_approval() {
    let dir = data_dir();
    let (router, app) = app_in(&dir);
    let admin = token(&app, "ana", Role::Admin);
    let member = token(&app, "bo", Role::Member);

    assert_eq!(call(&router, "POST", "/api/plugins", &member, Some(install_body())).await.0, StatusCode::FORBIDDEN);

    // Approving less (or anything else) than the manifest asks for is refused.
    let mut partial = install_body();
    partial["granted"]["network"] = json!([]);
    let (status, body) = call(&router, "POST", "/api/plugins", &admin, Some(partial)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let mut garbage = install_body();
    garbage["component"] = json!(base64::engine::general_purpose::STANDARD.encode(b"not wasm"));
    assert_eq!(call(&router, "POST", "/api/plugins", &admin, Some(garbage)).await.0, StatusCode::BAD_REQUEST);

    let (status, body) = call(&router, "POST", "/api/plugins", &admin, Some(install_body())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], ID);
    assert_eq!(body["panels"][0]["id"], "counter");
    assert_eq!(body["permissions"]["network"], json!(["127.0.0.1"]));

    let (_, list) = call(&router, "GET", "/api/plugins", &member, None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn panels_actions_and_roles() {
    let dir = data_dir();
    let (router, app, _admin) = installed(&dir).await;
    let member = token(&app, "bo", Role::Member);
    let guest = token(&app, "gus", Role::Guest);
    let panel = format!("/api/plugins/{ID}/panels/counter");
    let action = format!("{panel}/action");
    let badge = |tree: &Value| tree["tree"]["children"][1]["children"][0]["text"].clone();

    let (status, tree) = call(&router, "GET", &panel, &guest, None).await;
    assert_eq!(status, StatusCode::OK, "{tree}");
    assert_eq!(tree["tree"]["type"], "stack");
    assert_eq!(tree["tree"]["children"][0]["text"], "hello");
    assert_eq!(badge(&tree), "count 0");

    // Guests look, members act.
    let add = json!({ "action": "add", "payload": { "by": 2 } });
    assert_eq!(call(&router, "POST", &action, &guest, Some(add.clone())).await.0, StatusCode::FORBIDDEN);
    let (status, tree) = call(&router, "POST", &action, &member, Some(add)).await;
    assert_eq!(status, StatusCode::OK, "{tree}");
    assert_eq!(badge(&tree), "count 2");

    // State lives in the host's storage, not in the instance.
    let set = json!({ "action": "set", "payload": { "value": "40" } });
    assert_eq!(badge(&call(&router, "POST", &action, &member, Some(set)).await.1), "count 40");
    assert_eq!(badge(&call(&router, "GET", &panel, &guest, None).await.1), "count 40");

    // A failure inside the plugin is reported as the plugin's.
    let bad = json!({ "action": "set", "payload": { "value": "x" } });
    let (status, body) = call(&router, "POST", &action, &member, Some(bad)).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["error"], "value must be a number");
    assert_eq!(call(&router, "GET", &format!("/api/plugins/{ID}/panels/nope"), &guest, None).await.0, StatusCode::BAD_GATEWAY);

    // The plugin's table in workspace.toml reaches it as configuration.
    std::fs::write(dir.join("workspace.toml"), format!("[plugins.\"{ID}\"]\ngreeting = \"hola\"\n")).unwrap();
    assert_eq!(call(&router, "GET", &panel, &guest, None).await.1["tree"]["children"][0]["text"], "hola");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn connector_items_secrets_and_network() {
    let dir = data_dir();
    let (router, app, admin) = installed(&dir).await;
    let member = token(&app, "bo", Role::Member);
    let poll = format!("/api/plugins/{ID}/poll");

    assert_eq!(call(&router, "POST", &poll, &member, None).await.0, StatusCode::OK);
    assert_eq!(call(&router, "POST", &poll, &member, None).await.0, StatusCode::OK);
    let (_, items) = call(&router, "GET", "/api/items", &member, None).await;
    let items = items.as_array().unwrap().clone();
    assert_eq!(items.len(), 1, "the same external id updates, it does not duplicate");
    assert_eq!(items[0]["plugin"], ID);
    assert_eq!(items[0]["kind"], "note");
    assert_eq!(items[0]["state"], "open");

    // Only declared secrets can be set; values are never listed.
    let secret = |name: &str| format!("/api/plugins/{ID}/secrets/{name}");
    let value = Some(json!({ "value": "s3cret" }));
    assert_eq!(call(&router, "POST", &secret("OTHER"), &admin, value.clone()).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(call(&router, "POST", &secret("EXAMPLE_TOKEN"), &member, value.clone()).await.0, StatusCode::FORBIDDEN);
    assert_eq!(call(&router, "POST", &secret("EXAMPLE_TOKEN"), &admin, value).await.0, StatusCode::OK);
    let (_, info) = call(&router, "GET", &format!("/api/plugins/{ID}"), &member, None).await;
    assert_eq!(info["secrets_set"], json!(["EXAMPLE_TOKEN"]));
    assert!(!info.to_string().contains("s3cret"));
    call(&router, "POST", &poll, &member, None).await;
    assert_eq!(call(&router, "GET", "/api/items", &member, None).await.1[0]["state"], "with-secret");

    // Without `items = ["read"]` a plugin sees only what it emitted.
    app.store
        .item_upsert("net.other.plugin", &serde_json::from_value(json!({ "external_id": "x", "title": "theirs" })).unwrap())
        .unwrap();
    let command = |id: &str| format!("/api/plugins/{ID}/commands/{id}");
    let (status, seen) = call(&router, "POST", &command("example.items"), &member, Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{seen}");
    assert_eq!(seen["result"].as_array().unwrap().len(), 1);
    assert_eq!(call(&router, "GET", "/api/items", &member, None).await.1.as_array().unwrap().len(), 2);

    // Network: the listed host works, any other is refused before a connection.
    let port = http_server(|_| (200, "hello".into()));
    let fetch_path = command("example.fetch");
    let fetch = |url: String| call(&router, "POST", &fetch_path, &member, Some(json!({ "url": url })));
    let (status, got) = fetch(format!("http://127.0.0.1:{port}/x")).await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert_eq!(got["result"], json!({ "status": 200, "body": "hello" }));
    let (status, refused) = fetch(format!("http://localhost:{port}/x")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(refused["error"].as_str().unwrap().contains("network permissions"), "{refused}");
    let (_, refused) = fetch("https://example.com/".into()).await;
    assert!(refused["error"].as_str().unwrap().contains("network permissions"), "{refused}");

    assert_eq!(call(&router, "POST", &command("example.unknown"), &member, None).await.0, StatusCode::BAD_GATEWAY);
    let (_, logs) = call(&router, "GET", &format!("/api/plugins/{ID}/logs"), &admin, None).await;
    assert!(logs.as_array().unwrap().iter().any(|l| l.as_str().unwrap().ends_with("info poll")), "{logs}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn runaway_plugin_is_stopped_and_lifecycle() {
    let dir = data_dir();
    let (router, app, admin) = installed(&dir).await;
    let panel = format!("/api/plugins/{ID}/panels/counter");

    // An endless loop hits the time limit and later calls still work.
    let started = std::time::Instant::now();
    let (status, body) = call(&router, "POST", &format!("/api/plugins/{ID}/commands/example.spin"), &admin, None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["error"], "the plugin ran out of time");
    assert!(started.elapsed().as_secs() < 30);
    assert_eq!(call(&router, "GET", &panel, &admin, None).await.0, StatusCode::OK);

    // Disabled plugins do not run.
    let enabled = format!("/api/plugins/{ID}/enabled");
    assert_eq!(call(&router, "POST", &enabled, &admin, Some(json!({ "enabled": false }))).await.0, StatusCode::OK);
    assert_eq!(call(&router, "GET", &panel, &admin, None).await.0, StatusCode::BAD_GATEWAY);
    assert_eq!(call(&router, "POST", &enabled, &admin, Some(json!({ "enabled": true }))).await.0, StatusCode::OK);

    // A restart loads the plugin again from the data directory.
    drop((router, app));
    let (router, app) = app_in(&dir);
    let admin = app.store.create_user_token(1, "api", "again", None).unwrap().1;
    assert_eq!(call(&router, "GET", &panel, &admin, None).await.0, StatusCode::OK);

    // A component changed on disk no longer matches its checksum and is not loaded.
    drop((router, app));
    std::fs::write(dir.join("plugins").join(ID).join("plugin.wasm"), b"tampered").unwrap();
    let (router, app) = app_in(&dir);
    let admin = app.store.create_user_token(1, "api", "third", None).unwrap().1;
    assert_eq!(call(&router, "GET", &panel, &admin, None).await.0, StatusCode::NOT_FOUND);

    assert_eq!(call(&router, "DELETE", &format!("/api/plugins/{ID}"), &admin, None).await.0, StatusCode::OK);
    assert!(!dir.join("plugins").join(ID).exists());
    let _ = std::fs::remove_dir_all(dir);
}

/// The GitHub connector against a stand-in for the GitHub API.
#[tokio::test(flavor = "multi_thread")]
async fn github_connector() {
    use std::sync::{Arc, Mutex};
    const GH: &str = "net.ema.github";
    let pr = |n: u64, title: &str, draft: bool| {
        json!({ "number": n, "title": title, "draft": draft, "body": "text", "user": { "login": "octo" },
                "html_url": format!("https://github.com/acme/app/pull/{n}"), "created_at": "2026-01-02T03:04:05Z",
                "repository_url": "https://api.github.com/repos/acme/tools" })
    };
    // What the fake API lists, and the requests it saw.
    let listed = Arc::new(Mutex::new(json!([pr(1, "First", false), pr(2, "Second", true)])));
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let (listed2, seen2) = (listed.clone(), seen.clone());
    let port = http_server(move |request| {
        seen2.lock().unwrap().push(request.to_string());
        let path = request.split_whitespace().nth(1).unwrap_or("");
        if path.starts_with("/repos/acme/app/pulls/1/files") {
            (200, json!([
                { "filename": "src/auth/login.rs", "status": "modified", "additions": 260, "deletions": 10, "patch": "@@ -1 +1 @@\n-old\n+new" },
                { "filename": "README.md", "status": "modified", "additions": 3, "deletions": 0, "patch": "+docs" },
            ]).to_string())
        } else if path.starts_with("/repos/acme/app/pulls/1") {
            let mut one = pr(1, "First", false);
            one["changed_files"] = json!(5);
            one["base"] = json!({ "ref": "main" });
            one["head"] = json!({ "ref": "login" });
            (200, one.to_string())
        } else if path.starts_with("/repos/acme/app/pulls") {
            (200, listed2.lock().unwrap().to_string())
        } else if path.starts_with("/search/issues") {
            (200, json!({ "items": [pr(7, "Please review", false)] }).to_string())
        } else {
            (404, json!({ "message": "Not Found" }).to_string())
        }
    });

    let dir = data_dir();
    let (router, app) = app_in(&dir);
    let admin = token(&app, "ana", Role::Admin);
    // Same plugin, pointed at this machine instead of api.github.com.
    let manifest = std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/github/almighty-plugin.toml"))
        .unwrap()
        .replace("api.github.com", "127.0.0.1");
    let granted = toml::from_str::<toml::Table>(&manifest).unwrap()["permissions"].clone();
    let body = json!({
        "manifest": manifest,
        "component": base64::engine::general_purpose::STANDARD.encode(build_plugin("github")),
        "granted": granted,
    });
    let (status, out) = call(&router, "POST", "/api/plugins", &admin, Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let panel = format!("/api/plugins/{GH}/panels/pulls");
    let poll = format!("/api/plugins/{GH}/poll");

    // Not configured: the panel says what to do.
    let (_, tree) = call(&router, "GET", &panel, &admin, None).await;
    assert_eq!(tree["tree"]["children"][1]["type"], "empty", "{tree}");

    let config = |extra: &str| {
        let text = format!("[plugins.\"{GH}\"]\napi = \"http://127.0.0.1:{port}\"\nrepos = [\"acme/app\", \"../evil\"]\n{extra}");
        std::fs::write(dir.join("workspace.toml"), text).unwrap();
    };
    config("");
    let (status, out) = call(&router, "POST", &poll, &admin, None).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let (_, items) = call(&router, "GET", "/api/items?source=github", &admin, None).await;
    let find = |items: &Value, id: &str| items.as_array().unwrap().iter().find(|i| i["external_id"] == id).cloned().unwrap();
    assert_eq!(items.as_array().unwrap().len(), 2, "{items}");
    let first = find(&items, "acme/app#1");
    assert_eq!(first["title"], "First");
    assert_eq!(first["kind"], "pull_request");
    assert_eq!(first["thread"], "acme/app");
    assert_eq!(first["actors"], json!(["octo"]));
    assert_eq!(first["received_at"], 1767323045);
    assert_eq!(find(&items, "acme/app#2")["state"], "draft");
    // No token stored: no credentials sent, no search, and the bad repo name never became a request.
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(!seen[0].to_ascii_lowercase().contains("authorization"));
    }

    // Review: the plugin lists the files, the server maps the risk.
    let (status, review) = call(&router, "GET", &format!("/api/review/{}", first["id"].as_str().unwrap()), &admin, None).await;
    assert_eq!(status, StatusCode::OK, "{review}");
    assert_eq!(review["change"]["head"], "login");
    assert_eq!(review["risks"][0]["path"], "src/auth/login.rs");
    assert_eq!(review["risks"][0]["level"], "high");
    assert_eq!(review["risks"][1]["level"], "low");
    assert_eq!(review["totals"]["files"], 2);
    let notes = review["notes"].to_string();
    assert!(notes.contains("no test file") && notes.contains("incomplete"), "{notes}");
    assert_eq!(call(&router, "GET", "/api/review/it_nope", &admin, None).await.0, StatusCode::NOT_FOUND);
    // No model configured: the summary says so instead of failing obscurely.
    let (status, out) = call(&router, "POST", &format!("/api/review/{}/summary", first["id"].as_str().unwrap()), &admin, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{out}");
    seen.lock().unwrap().clear();
    call(&router, "POST", &poll, &admin, None).await;

    // A pull request that is no longer listed is closed; with a token, review requests arrive.
    *listed.lock().unwrap() = json!([pr(1, "First, renamed", false)]);
    let secret = format!("/api/plugins/{GH}/secrets/GITHUB_TOKEN");
    assert_eq!(call(&router, "POST", &secret, &admin, Some(json!({ "value": "tok123" }))).await.0, StatusCode::OK);
    let (_, tree) = call(&router, "POST", &format!("{panel}/action"), &admin, Some(json!({ "action": "refresh" }))).await;
    assert!(seen.lock().unwrap().last().unwrap().contains("Bearer tok123") || seen.lock().unwrap().last().unwrap().contains("bearer tok123"));
    let (_, items) = call(&router, "GET", "/api/items?source=github", &admin, None).await;
    assert_eq!(find(&items, "acme/app#1")["title"], "First, renamed");
    assert_eq!(find(&items, "acme/app#2")["state"], "closed");
    assert_eq!(find(&items, "acme/tools#7")["kind"], "review_request");
    let text = tree.to_string();
    assert!(text.contains("First, renamed") && text.contains("review requested") && !text.contains("Second"), "{text}");

    // An API error is reported and the stored items stay.
    std::fs::write(dir.join("workspace.toml"), format!("[plugins.\"{GH}\"]\napi = \"http://127.0.0.1:{port}/nope\"\nrepos = [\"acme/app\"]\n")).unwrap();
    let (status, out) = call(&router, "POST", &poll, &admin, None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(out["error"].as_str().unwrap().contains("GitHub answered 404"), "{out}");
    let (_, info) = call(&router, "GET", &format!("/api/plugins/{GH}"), &admin, None).await;
    assert!(info["last_error"].as_str().unwrap().contains("404"));
    assert_eq!(call(&router, "GET", "/api/items?source=github", &admin, None).await.1.as_array().unwrap().len(), 3);
    let _ = std::fs::remove_dir_all(dir);
}

/// Installs `plugins/<name>` with its network permission pointed at this machine.
async fn install_local(router: &Router, admin: &str, name: &str, hosts: &[&str]) {
    let mut manifest = std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../../plugins/{name}/almighty-plugin.toml"))).unwrap();
    for host in hosts {
        manifest = manifest.replace(&format!("\"{host}\""), "\"127.0.0.1\"");
    }
    let mut table = toml::from_str::<toml::Table>(&manifest).unwrap();
    // Two hosts collapsed into one: keep the list free of duplicates.
    if let Some(network) = table["permissions"].get_mut("network").and_then(|n| n.as_array_mut()) {
        network.dedup();
    }
    let manifest = toml::to_string(&table).unwrap();
    let body = json!({
        "manifest": manifest,
        "component": base64::engine::general_purpose::STANDARD.encode(build_plugin(name)),
        "granted": table["permissions"],
    });
    let (status, out) = call(router, "POST", "/api/plugins", admin, Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{out}");
}

#[tokio::test(flavor = "multi_thread")]
async fn slack_connector() {
    use std::sync::{Arc, Mutex};
    const ID: &str = "net.ema.slack";
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen2 = seen.clone();
    let port = http_server(move |request| {
        let path = request.split_whitespace().nth(1).unwrap_or("").to_string();
        seen2.lock().unwrap().push(path.clone());
        if !request.to_ascii_lowercase().contains("authorization: bearer xoxp-test") {
            return (200, json!({ "ok": false, "error": "invalid_auth" }).to_string());
        }
        let body = if path.starts_with("/auth.test") {
            json!({ "ok": true, "user_id": "U1", "user": "ana", "url": "https://acme.slack.com/" })
        } else if path.starts_with("/users.conversations") {
            json!({ "ok": true, "channels": [{ "id": "C1", "name": "general" }, { "id": "D1", "is_im": true }] })
        } else if path.starts_with("/conversations.history") && path.contains("channel=C1") {
            // The second sync asks only for what is newer than the last message seen.
            if path.contains("oldest=1800000002.000200") {
                json!({ "ok": true, "messages": [] })
            } else {
                json!({ "ok": true, "messages": [
                    { "ts": "1800000002.000200", "user": "U2", "text": "<@U1> can you check the build?\nIt is red." },
                    { "ts": "1800000001.000100", "user": "U1", "text": "my own message" },
                    { "ts": "1800000000.000000", "user": "U2", "subtype": "channel_join", "text": "joined" },
                ] })
            }
        } else if path.starts_with("/conversations.history") {
            json!({ "ok": true, "messages": [{ "ts": "1800000003.000300", "user": "U2", "text": "lunch?" }] })
        } else if path.starts_with("/users.info") {
            json!({ "ok": true, "user": { "name": "bo", "profile": { "display_name": "Bo" } } })
        } else {
            json!({ "ok": false, "error": "unknown_method" })
        };
        (200, body.to_string())
    });

    let dir = data_dir();
    let (router, app) = app_in(&dir);
    let admin = token(&app, "ana", Role::Admin);
    install_local(&router, &admin, "slack", &["slack.com"]).await;
    std::fs::write(dir.join("workspace.toml"), format!("[plugins.\"{ID}\"]\napi = \"http://127.0.0.1:{port}\"\n")).unwrap();
    let poll = format!("/api/plugins/{ID}/poll");

    // No token: the scheduled poll does nothing and does not fail.
    assert_eq!(call(&router, "POST", &poll, &admin, None).await.0, StatusCode::OK);
    assert!(seen.lock().unwrap().is_empty());

    let secret = format!("/api/plugins/{ID}/secrets/SLACK_TOKEN");
    call(&router, "POST", &secret, &admin, Some(json!({ "value": "xoxp-test" }))).await;
    let (status, out) = call(&router, "POST", &poll, &admin, None).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let (_, items) = call(&router, "GET", "/api/items?source=slack", &admin, None).await;
    let items = items.as_array().unwrap().clone();
    assert_eq!(items.len(), 2, "own messages and joins are skipped: {items:?}");
    let find = |id: &str| items.iter().find(|i| i["external_id"] == id).cloned().unwrap();
    let mention = find("C1:1800000002.000200");
    assert_eq!(mention["title"], "<@U1> can you check the build?");
    assert_eq!(mention["thread"], "#general");
    assert_eq!(mention["actors"], json!(["Bo"]));
    assert_eq!(mention["mentions"], json!(["ana", "U1"]));
    assert_eq!(mention["direct"], false);
    assert_eq!(mention["url"], "https://acme.slack.com/archives/C1/p1800000002000200");
    assert_eq!(mention["received_at"], 1800000002);
    let dm = find("D1:1800000003.000300");
    assert_eq!((dm["direct"].as_bool(), dm["thread"].as_str()), (Some(true), Some("direct message")));

    // Triage picks the hints up: both are for ana to answer.
    std::fs::create_dir_all(dir.join("users/ana")).unwrap();
    std::fs::write(dir.join("users/ana/triage.toml"), "me = [\"ana\"]\n").unwrap();
    let (_, inbox) = call(&router, "GET", "/api/inbox?bucket=reply", &admin, None).await;
    assert_eq!(inbox["items"].as_array().unwrap().len(), 2, "{inbox}");

    // The next sync continues after the last message and looks names up only once.
    call(&router, "POST", &poll, &admin, None).await;
    {
        let seen = seen.lock().unwrap();
        assert!(seen.iter().any(|p| p.contains("channel=C1") && p.contains("oldest=1800000002.000200")), "{seen:?}");
        assert_eq!(seen.iter().filter(|p| p.starts_with("/users.info")).count(), 1);
    }

    // A refused token is reported in the panel.
    call(&router, "POST", &secret, &admin, Some(json!({ "value": "xoxp-wrong" }))).await;
    let (_, tree) = call(&router, "POST", &format!("/api/plugins/{ID}/panels/status/action"), &admin, Some(json!({ "action": "sync" }))).await;
    assert!(tree.to_string().contains("invalid_auth"), "{tree}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn gmail_connector() {
    use std::sync::{Arc, Mutex};
    const ID: &str = "net.ema.gmail";
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen2 = seen.clone();
    let message = |id: &str, subject: &str, from: &str, labels: Value, list: bool| {
        let mut headers = vec![json!({ "name": "From", "value": from }), json!({ "name": "Subject", "value": subject })];
        if list {
            headers.push(json!({ "name": "List-Unsubscribe", "value": "<mailto:x@example.com>" }));
        }
        json!({ "id": id, "threadId": format!("t{id}"), "labelIds": labels, "snippet": "Short preview", "internalDate": "1800000000000",
                "payload": { "headers": headers } })
    };
    let port = http_server(move |request| {
        seen2.lock().unwrap().push(request.to_string());
        let path = request.split_whitespace().nth(1).unwrap_or("");
        if path.starts_with("/token") {
            return if request.contains("refresh_token=r%2Ftoken") && request.contains("client_secret=s3") {
                (200, json!({ "access_token": "ya29.test", "expires_in": 3599 }).to_string())
            } else {
                (400, json!({ "error": "invalid_grant", "error_description": "Bad Request" }).to_string())
            };
        }
        if !request.to_ascii_lowercase().contains("authorization: bearer ya29.test") {
            return (401, json!({ "error": { "message": "Invalid Credentials" } }).to_string());
        }
        if path.starts_with("/gmail/v1/users/me/messages?") {
            (200, json!({ "messages": [{ "id": "a1" }, { "id": "b2" }, { "id": "c3" }, { "id": "../x" }] }).to_string())
        } else if path.starts_with("/gmail/v1/users/me/messages/a1") {
            (200, message("a1", "Contract draft", "\"Cy Lee\" <cy@example.com>", json!(["INBOX", "UNREAD"]), false).to_string())
        } else if path.starts_with("/gmail/v1/users/me/messages/b2") {
            (200, message("b2", "Big sale", "shop@example.com", json!(["INBOX", "UNREAD", "CATEGORY_PROMOTIONS"]), true).to_string())
        } else if path.starts_with("/gmail/v1/users/me/messages/c3") {
            (200, message("c3", "", "dee@example.com", json!(["INBOX"]), false).to_string())
        } else {
            (404, json!({ "error": { "message": "Not Found" } }).to_string())
        }
    });

    let dir = data_dir();
    let (router, app) = app_in(&dir);
    let admin = token(&app, "ana", Role::Admin);
    install_local(&router, &admin, "gmail", &["gmail.googleapis.com", "oauth2.googleapis.com"]).await;
    let base = format!("http://127.0.0.1:{port}");
    std::fs::write(dir.join("workspace.toml"), format!("[plugins.\"{ID}\"]\napi = \"{base}\"\ntoken_url = \"{base}/token\"\n")).unwrap();
    let poll = format!("/api/plugins/{ID}/poll");
    let secret = |name: &str| format!("/api/plugins/{ID}/secrets/{name}");

    // Incomplete credentials: nothing is contacted, the panel names what is missing.
    call(&router, "POST", &secret("GMAIL_CLIENT_ID"), &admin, Some(json!({ "value": "id1" }))).await;
    assert_eq!(call(&router, "POST", &poll, &admin, None).await.0, StatusCode::OK);
    assert!(seen.lock().unwrap().is_empty());
    let (_, tree) = call(&router, "GET", &format!("/api/plugins/{ID}/panels/status"), &admin, None).await;
    assert!(tree.to_string().contains("GMAIL_CLIENT_SECRET is not set"), "{tree}");

    call(&router, "POST", &secret("GMAIL_CLIENT_SECRET"), &admin, Some(json!({ "value": "s3" }))).await;
    call(&router, "POST", &secret("GMAIL_REFRESH_TOKEN"), &admin, Some(json!({ "value": "r/token" }))).await;
    let (status, out) = call(&router, "POST", &poll, &admin, None).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let (_, items) = call(&router, "GET", "/api/items?source=gmail", &admin, None).await;
    let items = items.as_array().unwrap().clone();
    assert_eq!(items.len(), 3, "an id that is not plain is never requested: {items:?}");
    let find = |id: &str| items.iter().find(|i| i["external_id"] == id).cloned().unwrap();
    let personal = find("a1");
    assert_eq!((personal["title"].as_str(), personal["actors"][0].as_str()), (Some("Contract draft"), Some("Cy Lee")));
    assert_eq!((personal["direct"].as_bool(), personal["state"].as_str()), (Some(true), Some("open")));
    assert_eq!(personal["body"], "Short preview");
    assert_eq!(personal["received_at"], 1800000000);
    assert_eq!(find("b2")["direct"], false, "promotions and lists are not addressed to you");
    assert_eq!((find("c3")["title"].as_str(), find("c3")["state"].as_str()), (Some("(no subject)"), Some("read")));

    // In the inbox: the personal mail asks for a reply, the sale does not, the read one is done.
    let (_, inbox) = call(&router, "GET", "/api/inbox?show=all", &admin, None).await;
    let triage = |id: &str| {
        inbox["items"].as_array().unwrap().iter().find(|e| e["item"]["external_id"] == id).map(|e| e["triage"].clone()).unwrap()
    };
    assert_eq!(triage("a1")["bucket"], "reply");
    assert_eq!(triage("b2")["bucket"], "fyi");
    assert_eq!(triage("c3")["done"], true);

    // The access token is not kept anywhere: every sync asks for a new one.
    call(&router, "POST", &poll, &admin, None).await;
    assert_eq!(seen.lock().unwrap().iter().filter(|r| r.starts_with("POST /token")).count(), 2);

    // A revoked refresh token is reported with Google's reason.
    call(&router, "POST", &secret("GMAIL_REFRESH_TOKEN"), &admin, Some(json!({ "value": "revoked" }))).await;
    let (status, out) = call(&router, "POST", &poll, &admin, None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(out["error"].as_str().unwrap().contains("refused the refresh token"), "{out}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn agents_call_only_the_commands_offered_to_them() {
    let dir = data_dir();
    let (router, app, admin) = installed(&dir).await;
    let (_, agent) = app.store.create_agent_token("s1").unwrap();
    let guest = token(&app, "gil", Role::Guest);

    // The approved list is part of the permissions the admin saw.
    let (_, plugin) = call(&router, "GET", "/api/plugins/net.ema.example", &admin, None).await;
    assert_eq!(plugin["permissions"]["agent_tools"], json!(["example.items"]));

    let (status, body) = call(&router, "GET", "/api/agent-tools", &agent, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1, "{body}");
    assert_eq!(tools[0]["name"], "net_ema_example__example_items");
    assert_eq!(tools[0]["input_schema"]["type"], "object");
    assert!(tools[0]["description"].as_str().unwrap().contains("plugin example"));
    let (_, body) = call(&router, "GET", "/api/agent-tools", &guest, None).await;
    assert_eq!(body["tools"], json!([]));

    // The offered command runs for the agent and is logged under its name.
    let path = "/api/plugins/net.ema.example/commands";
    let (status, body) = call(&router, "POST", &format!("{path}/example.items"), &agent, Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, audit) = call(&router, "GET", "/api/audit", &admin, None).await;
    assert!(
        audit["entries"].as_array().unwrap().iter().any(|e| e["action"] == "plugin.command" && e["actor"] == "agent:s1"),
        "{audit}"
    );

    // Other commands, and everything else about plugins, stay closed to it.
    let (status, _) = call(&router, "POST", &format!("{path}/example.fetch"), &agent, Some(json!({ "url": "http://127.0.0.1:1/" }))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&router, "GET", "/api/plugins", &agent, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A disabled plugin offers nothing.
    let (status, _) = call(&router, "POST", "/api/plugins/net.ema.example/enabled", &admin, Some(json!({ "enabled": false }))).await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = call(&router, "GET", "/api/agent-tools", &agent, None).await;
    assert_eq!(body["tools"], json!([]));
    let (status, _) = call(&router, "POST", &format!("{path}/example.items"), &agent, Some(json!({}))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A manifest cannot offer a command it does not declare, or one without a description.
    let undeclared = manifest().replace("agent_tools = [\"example.items\"]", "agent_tools = [\"example.nope\"]");
    let mut body = install_body();
    body["manifest"] = json!(undeclared);
    body["granted"]["agent_tools"] = json!(["example.nope"]);
    let (status, answer) = call(&router, "POST", "/api/plugins", &admin, Some(body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    let silent = manifest().replace("agent_tools = [\"example.items\"]", "agent_tools = [\"example.fetch\"]");
    let mut body = install_body();
    body["manifest"] = json!(silent);
    body["granted"]["agent_tools"] = json!(["example.fetch"]);
    let (status, answer) = call(&router, "POST", "/api/plugins", &admin, Some(body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    assert!(answer["error"].as_str().unwrap().contains("description"), "{answer}");
}
