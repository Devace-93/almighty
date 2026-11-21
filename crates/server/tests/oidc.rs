//! Sign-in through an identity provider, against a stand-in provider that
//! signs real RS256 id tokens.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use almighty_server::store::Store;
use almighty_server::{build, App, Config};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use base64::Engine;
use http_body_util::BodyExt;
use rsa::pkcs1::EncodeRsaPrivateKey;
use rsa::traits::PublicKeyParts;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// What the stand-in provider will put in the next id token.
#[derive(Clone)]
struct Next {
    email: String,
    verified: Value,
    sub: String,
    nonce: String,
    challenge: String,
    audience: String,
    expires_in: i64,
    /// Sign with HS256 using the public key as the secret (a classic attack).
    confuse_algorithm: bool,
}

struct Provider {
    port: u16,
    next: Arc<Mutex<Next>>,
}

fn provider() -> Provider {
    let key = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
    let pem = key.to_pkcs1_pem(Default::default()).unwrap().to_string();
    let jwks = json!({ "keys": [{ "kty": "RSA", "alg": "RS256", "use": "sig", "kid": "k1",
        "n": b64(&key.n().to_bytes_be()), "e": b64(&key.e().to_bytes_be()) }] });
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let issuer = format!("http://127.0.0.1:{port}");
    let next = Arc::new(Mutex::new(Next {
        email: String::new(),
        verified: json!(true),
        sub: String::new(),
        nonce: String::new(),
        challenge: String::new(),
        audience: "almighty".into(),
        expires_in: 300,
        confuse_algorithm: false,
    }));
    let shared = next.clone();
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
            let request = String::from_utf8_lossy(&request).to_string();
            let path = request.split_whitespace().nth(1).unwrap_or("");
            let (status, body) = if path == "/.well-known/openid-configuration" {
                (200, json!({ "issuer": issuer, "authorization_endpoint": format!("{issuer}/authorize"),
                    "token_endpoint": format!("{issuer}/token"), "jwks_uri": format!("{issuer}/jwks") }))
            } else if path == "/jwks" {
                (200, jwks.clone())
            } else if path == "/token" {
                let form = request.split("\r\n\r\n").nth(1).unwrap_or("");
                let field = |name: &str| form.split('&').find_map(|kv| kv.strip_prefix(&format!("{name}="))).unwrap_or("").to_string();
                let n = shared.lock().unwrap().clone();
                // PKCE: the verifier must hash to the challenge sent at the start.
                let pkce_ok = b64(&Sha256::digest(field("code_verifier").as_bytes())) == n.challenge;
                if field("code") != "good-code" || !pkce_ok || field("grant_type") != "authorization_code" {
                    (400, json!({ "error": "invalid_grant", "error_description": "bad code or verifier" }))
                } else {
                    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
                    let claims = json!({ "iss": issuer, "aud": n.audience, "sub": n.sub, "nonce": n.nonce, "email": n.email,
                        "email_verified": n.verified, "iat": now, "exp": now + n.expires_in });
                    let token = if n.confuse_algorithm {
                        let header = jsonwebtoken::Header { kid: Some("k1".into()), ..jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256) };
                        jsonwebtoken::encode(&header, &claims, &jsonwebtoken::EncodingKey::from_secret(jwks["keys"][0]["n"].as_str().unwrap().as_bytes())).unwrap()
                    } else {
                        let header = jsonwebtoken::Header { kid: Some("k1".into()), ..jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256) };
                        jsonwebtoken::encode(&header, &claims, &jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap()).unwrap()
                    };
                    (200, json!({ "access_token": "at", "token_type": "Bearer", "id_token": token }))
                }
            } else {
                (404, json!({}))
            };
            let body = body.to_string();
            let _ = stream.write_all(format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes());
        }
    });
    Provider { port, next }
}

fn data_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("almighty-oidc-{:016x}", rand::random::<u64>()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn app_in(dir: &Path) -> (Router, App) {
    build(Config { addr: ([127, 0, 0, 1], 0).into(), data_dir: dir.to_path_buf() }, Store::in_memory().unwrap())
}

async fn raw(router: &Router, method: &str, path: &str, token: Option<&str>, body: Option<Value>) -> (StatusCode, Option<String>, Value) {
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
    let location = res.headers().get("location").and_then(|v| v.to_str().ok()).map(str::to_string);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, location, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn query(url: &str, name: &str) -> String {
    url.split(['?', '&']).find_map(|kv| kv.strip_prefix(&format!("{name}="))).unwrap_or("").to_string()
}

const CLIENT: &str = "http://127.0.0.1:41000/cb";

/// Runs one sign-in: start, the person "signs in" at the provider, callback.
/// Returns the callback's status, its redirect and its error text.
async fn sign_in(router: &Router, p: &Provider, email: &str, sub: &str, tweak: impl FnOnce(&mut Next)) -> (StatusCode, Option<String>, String) {
    let (status, location, body) = raw(router, "GET", &format!("/api/auth/oidc/start?redirect={CLIENT}&device=test"), None, None).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    let to_provider = location.unwrap();
    assert!(to_provider.starts_with(&format!("http://127.0.0.1:{}/authorize?", p.port)), "{to_provider}");
    assert_eq!(query(&to_provider, "code_challenge_method"), "S256");
    assert_eq!(query(&to_provider, "response_type"), "code");
    {
        let mut next = p.next.lock().unwrap();
        *next = Next {
            email: email.into(),
            sub: sub.into(),
            verified: json!(true),
            nonce: query(&to_provider, "nonce"),
            challenge: query(&to_provider, "code_challenge"),
            audience: "almighty".into(),
            expires_in: 300,
            confuse_algorithm: false,
        };
        tweak(&mut next);
    }
    let state = query(&to_provider, "state");
    let (status, location, body) = raw(router, "GET", &format!("/api/auth/oidc/callback?code=good-code&state={state}"), None, None).await;
    (status, location, body["error"].as_str().unwrap_or("").to_string())
}

#[tokio::test(flavor = "multi_thread")]
async fn sign_in_through_a_provider() {
    let dir = data_dir();
    let (router, _app) = app_in(&dir);

    // Not configured: nothing is offered.
    assert_eq!(raw(&router, "GET", "/api/workspace", None, None).await.2["sso"], Value::Null);
    assert_eq!(raw(&router, "GET", &format!("/api/auth/oidc/start?redirect={CLIENT}"), None, None).await.0, StatusCode::NOT_FOUND);

    let p = provider();
    let issuer = format!("http://127.0.0.1:{}", p.port);
    std::fs::write(
        dir.join("workspace.toml"),
        format!("[oidc]\nissuer = \"{issuer}\"\nclient_id = \"almighty\"\nallowed_domains = [\"example.com\"]\nadmins = [\"lead@example.com\"]\n"),
    )
    .unwrap();
    assert_eq!(raw(&router, "GET", "/api/workspace", None, None).await.2["sso"], issuer);

    // A client can only be sent back to itself or to a listed address.
    let (status, _, body) = raw(&router, "GET", "/api/auth/oidc/start?redirect=https://evil.example/cb", None, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // An empty workspace is claimed by the person the operator named.
    let (status, _, error) = sign_in(&router, &p, "ana@example.com", "u-ana", |_| {}).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(error.contains("no owner yet"), "{error}");
    let (status, location, error) = sign_in(&router, &p, "lead@example.com", "u-lead", |_| {}).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{error}");
    let back = location.unwrap();
    assert!(back.starts_with(&format!("{CLIENT}?almighty_code=")), "{back}");
    assert!(!back.contains("alm_login_"), "the session token never travels in a URL");

    // The client trades the code for its token, once.
    let code = query(&back, "almighty_code");
    let (status, _, out) = raw(&router, "POST", "/api/auth/oidc/exchange", None, Some(json!({ "code": code }))).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!((out["name"].as_str(), out["role"].as_str()), (Some("lead@example.com"), Some("owner")));
    let owner = out["token"].as_str().unwrap().to_string();
    assert_eq!(raw(&router, "POST", "/api/auth/oidc/exchange", None, Some(json!({ "code": code }))).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(raw(&router, "GET", "/api/auth/me", Some(&owner), None).await.2["role"], "owner");
    // No password works for this account.
    let (status, _, _) = raw(&router, "POST", "/api/auth/login", None, Some(json!({ "name": "lead@example.com", "password": "!" }))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Everyone else arrives as a member.
    let (status, location, _) = sign_in(&router, &p, "ana@example.com", "u-ana", |_| {}).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let code = query(&location.unwrap(), "almighty_code");
    let (_, _, out) = raw(&router, "POST", "/api/auth/oidc/exchange", None, Some(json!({ "code": code }))).await;
    assert_eq!(out["role"], "member");
    let member = out["token"].as_str().unwrap().to_string();

    // Tokens that must be refused, each for its own reason.
    type Tweak = fn(&mut Next);
    let refused: [(&str, Tweak, &str); 7] = [
        ("another domain", |n| n.email = "eve@other.org".into(), "not allowed"),
        ("unverified email", |n| n.verified = json!(false), "not verified"),
        ("a token from another sign-in", |n| n.nonce = "stolen".into(), "does not belong"),
        ("a token for another application", |n| n.audience = "other-app".into(), "not valid"),
        ("an expired token", |n| n.expires_in = -3600, "not valid"),
        ("a token signed with the public key as a shared secret", |n| n.confuse_algorithm = true, "not accepted"),
        ("a failed code exchange", |n| n.challenge = "wrong".into(), "refused the sign-in"),
    ];
    for (what, tweak, expected) in refused {
        let (status, _, error) = sign_in(&router, &p, "eve@example.com", "u-eve", tweak).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{what}");
        assert!(error.contains(expected), "{what}: {error}");
    }
    // A state cannot be replayed, and a made-up one is unknown.
    assert_eq!(raw(&router, "GET", "/api/auth/oidc/callback?code=good-code&state=made-up", None, None).await.0, StatusCode::FORBIDDEN);
    let (_, _, users) = raw(&router, "GET", "/api/users", Some(&owner), None).await;
    assert!(!users.to_string().contains("eve@"), "{users}");

    // Roles: admins manage members and guests; only owners make admins; nobody removes themselves or the last owner.
    let role = |name: &str| format!("/api/users/{name}/role");
    assert_eq!(raw(&router, "POST", &role("ana@example.com"), Some(&member), Some(json!({ "role": "guest" }))).await.0, StatusCode::FORBIDDEN);
    assert_eq!(raw(&router, "POST", &role("ana@example.com"), Some(&owner), Some(json!({ "role": "admin" }))).await.0, StatusCode::OK);
    let admin = member; // same token, new role
    assert_eq!(raw(&router, "POST", &role("lead@example.com"), Some(&admin), Some(json!({ "role": "member" }))).await.0, StatusCode::FORBIDDEN);
    assert_eq!(raw(&router, "POST", &role("ana@example.com"), Some(&admin), Some(json!({ "role": "owner" }))).await.0, StatusCode::FORBIDDEN);
    assert_eq!(raw(&router, "POST", &role("lead@example.com"), Some(&owner), Some(json!({ "role": "member" }))).await.0, StatusCode::BAD_REQUEST, "last owner");
    assert_eq!(raw(&router, "POST", &role("nobody"), Some(&owner), Some(json!({ "role": "guest" }))).await.0, StatusCode::NOT_FOUND);
    assert_eq!(raw(&router, "DELETE", "/api/users/lead@example.com", Some(&owner), None).await.0, StatusCode::BAD_REQUEST, "yourself");
    assert_eq!(raw(&router, "DELETE", "/api/users/ana@example.com", Some(&owner), None).await.0, StatusCode::OK);
    // A removed person's token stops working at once.
    assert_eq!(raw(&router, "GET", "/api/auth/me", Some(&admin), None).await.0, StatusCode::UNAUTHORIZED);
    let _ = std::fs::remove_dir_all(dir);
}
