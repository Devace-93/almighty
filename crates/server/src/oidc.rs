//! Sign-in through an OpenID Connect provider (Okta, Google Workspace,
//! Entra, Keycloak…): authorization code flow with PKCE. The provider says
//! who the person is; roles stay in almighty.
//!
//! ```toml
//! [oidc]
//! issuer = "https://id.example.com"
//! client_id = "almighty"
//! client_secret_env = "ALMIGHTY_OIDC_SECRET"   # name of the variable; omit for a public client
//! public_url = "https://almighty.example.com"  # how browsers reach this server
//! redirect_urls = ["https://almighty.example.com/"]  # where clients may be sent back
//! allowed_domains = ["example.com"]            # optional
//! admins = ["lead@example.com"]                # get the admin role
//! default_role = "member"
//! ```

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::session::now_secs;
use crate::store::{valid_user_name, Role, Store};

const PENDING_TTL: Duration = Duration::from_secs(600);
const CODE_TTL: Duration = Duration::from_secs(60);
/// Asymmetric algorithms only: a shared-secret or "none" token is never accepted.
const ALGORITHMS: [Algorithm; 7] =
    [Algorithm::RS256, Algorithm::RS384, Algorithm::RS512, Algorithm::PS256, Algorithm::ES256, Algorithm::ES384, Algorithm::EdDSA];

#[derive(Debug, Clone)]
pub struct Config {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub public_url: Option<String>,
    pub redirect_urls: Vec<String>,
    pub scopes: Vec<String>,
    pub allowed_domains: Vec<String>,
    pub admins: Vec<String>,
    pub default_role: Role,
    pub link_existing: bool,
}

fn loopback(url: &str) -> bool {
    ["http://127.0.0.1:", "http://localhost:", "http://127.0.0.1/", "http://localhost/"].iter().any(|p| url.starts_with(p))
}

/// Providers are reached over https; plain HTTP only on this machine (tests, local Keycloak).
fn safe_endpoint(url: &str) -> bool {
    url.starts_with("https://") || loopback(url)
}

impl Config {
    pub fn load(data_dir: &Path) -> Option<Config> {
        let text = std::fs::read_to_string(data_dir.join("workspace.toml")).ok()?;
        let root: toml::Table = toml::from_str(&text).ok()?;
        let t = root.get("oidc")?.as_table()?;
        let string = |key: &str| t.get(key).and_then(|v| v.as_str()).map(str::to_string);
        let list = |key: &str| -> Vec<String> {
            t.get(key).and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_ascii_lowercase())).collect()).unwrap_or_default()
        };
        let issuer = string("issuer")?.trim_end_matches('/').to_string();
        if !safe_endpoint(&issuer) {
            return None;
        }
        let scopes = list("scopes");
        Some(Config {
            issuer,
            client_id: string("client_id")?,
            client_secret: string("client_secret_env").and_then(|name| std::env::var(name).ok()).filter(|s| !s.is_empty()),
            public_url: string("public_url").map(|u| u.trim_end_matches('/').to_string()),
            redirect_urls: t.get("redirect_urls").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default(),
            scopes: if scopes.is_empty() { vec!["openid".into(), "email".into(), "profile".into()] } else { scopes },
            allowed_domains: list("allowed_domains"),
            admins: list("admins"),
            default_role: string("default_role").and_then(|r| Role::parse(&r)).filter(|r| *r <= Role::Member).unwrap_or(Role::Member),
            link_existing: t.get("link_existing").and_then(|v| v.as_bool()).unwrap_or(false),
        })
    }

    /// A client may be sent back to its own loopback listener (CLI, desktop)
    /// or to a URL the operator listed. Anything else would hand a sign-in
    /// code to a stranger.
    pub fn redirect_allowed(&self, url: &str) -> bool {
        if url.contains(['\r', '\n', ' ', '#']) || url.len() > 500 {
            return false;
        }
        loopback(url) || self.redirect_urls.iter().any(|allowed| url == allowed || url.starts_with(&format!("{}/", allowed.trim_end_matches('/'))))
    }
}

struct Pending {
    verifier: String,
    nonce: String,
    redirect: String,
    device: String,
    at: Instant,
}

/// Sign-ins in flight and the one-time codes handed to clients.
#[derive(Default)]
pub struct Oidc {
    pending: Mutex<HashMap<String, Pending>>,
    codes: Mutex<HashMap<String, (String, Instant)>>,
}

fn random(bytes: usize) -> String {
    let raw: Vec<u8> = (0..bytes).map(|_| rand::random::<u8>()).collect();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
}

fn enc(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().max_redirects(0).http_status_as_error(false).timeout_global(Some(Duration::from_secs(15))).build().new_agent()
}

fn get_json(url: &str) -> Result<Value, String> {
    if !safe_endpoint(url) {
        return Err(format!("the provider's endpoint {url} is not https"));
    }
    let mut response = agent().get(url).call().map_err(|e| format!("cannot reach the identity provider: {e}"))?;
    if response.status().as_u16() != 200 {
        return Err(format!("the identity provider answered {} for {url}", response.status()));
    }
    let text = response.body_mut().read_to_string().map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|_| "the identity provider did not answer with JSON".to_string())
}

fn discover(config: &Config) -> Result<Value, String> {
    let doc = get_json(&format!("{}/.well-known/openid-configuration", config.issuer))?;
    if doc["issuer"].as_str().map(|i| i.trim_end_matches('/')) != Some(config.issuer.as_str()) {
        return Err("the provider's discovery document names another issuer".into());
    }
    Ok(doc)
}

#[derive(Debug, Deserialize)]
struct Claims {
    sub: String,
    #[serde(default)]
    nonce: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    email_verified: Option<Value>,
}

#[derive(Debug, PartialEq)]
pub struct Identity {
    pub subject: String,
    pub email: String,
}

impl Oidc {
    /// The provider URL to send the browser to. Blocking (discovery).
    pub fn start(&self, config: &Config, server_url: &str, redirect: &str, device: &str) -> Result<String, String> {
        if !config.redirect_allowed(redirect) {
            return Err("this client address is not in the list of allowed sign-in redirects".into());
        }
        let doc = discover(config)?;
        let authorize = doc["authorization_endpoint"].as_str().filter(|u| safe_endpoint(u)).ok_or("the provider has no usable authorization endpoint")?;
        let (state, nonce, verifier) = (random(24), random(24), random(48));
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let callback = format!("{}/api/auth/oidc/callback", config.public_url.as_deref().unwrap_or(server_url));
        let url = format!(
            "{authorize}{}response_type=code&client_id={}&redirect_uri={}&scope={}&state={state}&nonce={nonce}&code_challenge={challenge}&code_challenge_method=S256",
            if authorize.contains('?') { '&' } else { '?' },
            enc(&config.client_id),
            enc(&callback),
            enc(&config.scopes.join(" ")),
        );
        let mut pending = self.pending.lock().unwrap();
        pending.retain(|_, p| p.at.elapsed() < PENDING_TTL);
        if pending.len() > 1000 {
            return Err("too many sign-ins in progress; try again in a minute".into());
        }
        let device = device.chars().filter(|c| !c.is_control()).take(60).collect();
        pending.insert(state, Pending { verifier, nonce, redirect: redirect.to_string(), device, at: Instant::now() });
        Ok(url)
    }

    /// Finishes a sign-in: trades the provider's code for an id token,
    /// checks it, and returns who it is, where to send the client and the
    /// device label. Blocking.
    pub fn finish(&self, config: &Config, server_url: &str, state: &str, code: &str) -> Result<(Identity, String, String), String> {
        let pending = self.pending.lock().unwrap().remove(state).filter(|p| p.at.elapsed() < PENDING_TTL).ok_or("this sign-in expired or was already used; start again")?;
        let doc = discover(config)?;
        let token_url = doc["token_endpoint"].as_str().filter(|u| safe_endpoint(u)).ok_or("the provider has no usable token endpoint")?;
        let callback = format!("{}/api/auth/oidc/callback", config.public_url.as_deref().unwrap_or(server_url));
        let mut form = format!(
            "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
            enc(code),
            enc(&callback),
            enc(&config.client_id),
            pending.verifier
        );
        if let Some(secret) = &config.client_secret {
            form.push_str(&format!("&client_secret={}", enc(secret)));
        }
        let mut response = agent()
            .post(token_url)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .send(form)
            .map_err(|e| format!("cannot reach the identity provider: {e}"))?;
        let status = response.status().as_u16();
        let body: Value = serde_json::from_str(&response.body_mut().read_to_string().map_err(|e| e.to_string())?).unwrap_or(Value::Null);
        let id_token = body["id_token"].as_str().ok_or_else(|| {
            format!("the identity provider refused the sign-in ({status}): {}", body["error_description"].as_str().or(body["error"].as_str()).unwrap_or("no reason given"))
        })?;
        let jwks_url = doc["jwks_uri"].as_str().ok_or("the provider publishes no keys")?;
        let jwks: JwkSet = serde_json::from_value(get_json(jwks_url)?).map_err(|e| format!("the provider's keys are malformed: {e}"))?;
        let identity = verify_id_token(config, id_token, &jwks, &pending.nonce)?;
        Ok((identity, pending.redirect, pending.device))
    }

    /// Stores a session token behind a one-time code the client redeems.
    pub fn hand_over(&self, token: String) -> String {
        let code = random(32);
        let mut codes = self.codes.lock().unwrap();
        codes.retain(|_, (_, at)| at.elapsed() < CODE_TTL);
        codes.insert(code.clone(), (token, Instant::now()));
        code
    }

    pub fn redeem(&self, code: &str) -> Option<String> {
        let (token, at) = self.codes.lock().unwrap().remove(code)?;
        (at.elapsed() < CODE_TTL).then_some(token)
    }
}

/// Signature, issuer, audience, expiry and nonce; then the email rules.
pub fn verify_id_token(config: &Config, id_token: &str, jwks: &JwkSet, nonce: &str) -> Result<Identity, String> {
    let header = decode_header(id_token).map_err(|_| "the id token is malformed".to_string())?;
    if !ALGORITHMS.contains(&header.alg) {
        return Err("the id token uses an algorithm that is not accepted".into());
    }
    let jwk = match &header.kid {
        Some(kid) => jwks.find(kid),
        None if jwks.keys.len() == 1 => jwks.keys.first(),
        None => None,
    }
    .ok_or("the id token was signed with a key the provider does not publish")?;
    let key = DecodingKey::from_jwk(jwk).map_err(|e| format!("unusable provider key: {e}"))?;
    let mut validation = Validation::new(header.alg);
    validation.set_audience(&[&config.client_id]);
    validation.set_issuer(&[&config.issuer, &format!("{}/", config.issuer)]);
    validation.set_required_spec_claims(&["exp", "aud", "iss", "sub"]);
    validation.leeway = 60;
    let claims = decode::<Claims>(id_token, &key, &validation).map_err(|e| format!("the id token is not valid: {e}"))?.claims;
    if nonce.is_empty() || claims.nonce != nonce {
        return Err("the id token does not belong to this sign-in".into());
    }
    let email = claims.email.filter(|e| e.contains('@')).ok_or("the provider did not share an email address")?.to_ascii_lowercase();
    // Some providers send the flag as a string.
    let verified = matches!(&claims.email_verified, Some(Value::Bool(true))) || claims.email_verified.as_ref().and_then(|v| v.as_str()) == Some("true");
    if !verified {
        return Err("the email address is not verified by the provider".into());
    }
    let domain = email.rsplit('@').next().unwrap_or("");
    if !config.allowed_domains.is_empty() && !config.allowed_domains.iter().any(|d| d == domain) {
        return Err(format!("accounts from {domain} are not allowed in this workspace"));
    }
    if !valid_user_name(&email) {
        return Err("the email address cannot be used as an account name".into());
    }
    Ok(Identity { subject: format!("{}|{}", config.issuer, claims.sub), email })
}

impl Store {
    /// Finds or creates the account of a person who signed in through the
    /// provider and returns its id.
    pub fn oidc_user(&self, config: &Config, who: &Identity) -> Result<i64, String> {
        let wants_admin = config.admins.contains(&who.email);
        let conn = self.conn.lock().unwrap();
        let known: Option<(i64, String)> = conn
            .query_row("SELECT id, role FROM users WHERE oidc_sub = ?1", params![who.subject], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some((id, role)) = known {
            // The operator's admin list can promote, never demote: roles are managed in almighty.
            if wants_admin && Role::parse(&role).is_some_and(|r| r < Role::Admin) {
                let _ = conn.execute("UPDATE users SET role = 'admin' WHERE id = ?1", params![id]);
            }
            return Ok(id);
        }
        let users: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0)).unwrap_or(0);
        // An empty workspace is claimed by someone the operator named, not by whoever arrives first.
        if users == 0 && !wants_admin {
            return Err("this workspace has no owner yet; only an address listed under [oidc] admins can create it".into());
        }
        let existing: Option<(i64, Option<String>)> = conn
            .query_row("SELECT id, oidc_sub FROM users WHERE name = ?1", params![who.email], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
            .map_err(|e| e.to_string())?;
        match existing {
            Some((id, None)) if config.link_existing => {
                conn.execute("UPDATE users SET oidc_sub = ?2 WHERE id = ?1", params![id, who.subject]).map_err(|e| e.to_string())?;
                Ok(id)
            }
            Some(_) => Err(format!("an account named {} already exists and is not linked to this provider; ask an admin", who.email)),
            None => {
                let role = if users == 0 { Role::Owner } else if wants_admin { Role::Admin } else { config.default_role };
                // "!" is not a password hash: this account cannot sign in with a password.
                conn.execute(
                    "INSERT INTO users(name, password_hash, role, created_at, oidc_sub) VALUES(?1, '!', ?2, ?3, ?4)",
                    params![who.email, role.as_str(), now_secs() as i64, who.subject],
                )
                .map_err(|e| e.to_string())?;
                Ok(conn.last_insert_rowid())
            }
        }
    }

    /// Changes a person's role. The last owner cannot be demoted.
    pub fn set_role(&self, name: &str, role: Role) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        let current: Option<String> = conn.query_row("SELECT role FROM users WHERE name = ?1", params![name], |r| r.get(0)).optional().map_err(|e| e.to_string())?;
        let current = current.ok_or_else(|| format!("no user {name}"))?;
        let owners: i64 = conn.query_row("SELECT COUNT(*) FROM users WHERE role = 'owner'", [], |r| r.get(0)).unwrap_or(0);
        if current == "owner" && role != Role::Owner && owners <= 1 {
            return Err("the workspace needs at least one owner".into());
        }
        conn.execute("UPDATE users SET role = ?2 WHERE name = ?1", params![name, role.as_str()]).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Removes a person and, with them, every token they hold.
    pub fn delete_user(&self, name: &str) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        let current: Option<String> = conn.query_row("SELECT role FROM users WHERE name = ?1", params![name], |r| r.get(0)).optional().map_err(|e| e.to_string())?;
        let current = current.ok_or_else(|| format!("no user {name}"))?;
        let owners: i64 = conn.query_row("SELECT COUNT(*) FROM users WHERE role = 'owner'", [], |r| r.get(0)).unwrap_or(0);
        if current == "owner" && owners <= 1 {
            return Err("the workspace needs at least one owner".into());
        }
        conn.execute("DELETE FROM users WHERE name = ?1", params![name]).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn role_of(&self, name: &str) -> Option<Role> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT role FROM users WHERE name = ?1", params![name], |r| r.get::<_, String>(0)).optional().ok().flatten().and_then(|r| Role::parse(&r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            issuer: "https://id.example.com".into(),
            client_id: "almighty".into(),
            client_secret: None,
            public_url: None,
            redirect_urls: vec!["https://almighty.example.com/".into()],
            scopes: vec!["openid".into()],
            allowed_domains: vec![],
            admins: vec!["lead@example.com".into()],
            default_role: Role::Member,
            link_existing: false,
        }
    }

    #[test]
    fn redirects_are_limited() {
        let c = config();
        assert!(c.redirect_allowed("http://127.0.0.1:41234/cb"));
        assert!(c.redirect_allowed("http://localhost:8093/"));
        assert!(c.redirect_allowed("https://almighty.example.com/"));
        assert!(c.redirect_allowed("https://almighty.example.com/app"));
        for bad in [
            "https://almighty.example.com.evil.net/",
            "https://evil.net/?next=https://almighty.example.com/",
            "http://127.0.0.1.evil.net/",
            "http://almighty.example.com/",
            "javascript:alert(1)",
            "https://almighty.example.com/\r\nSet-Cookie: x",
        ] {
            assert!(!c.redirect_allowed(bad), "{bad}");
        }
    }

    #[test]
    fn accounts_are_created_linked_and_promoted() {
        let store = Store::in_memory().unwrap();
        let c = config();
        let who = |email: &str, sub: &str| Identity { subject: format!("https://id.example.com|{sub}"), email: email.into() };

        // An empty workspace is only for someone on the admin list.
        assert!(store.oidc_user(&c, &who("ana@example.com", "1")).unwrap_err().contains("no owner yet"));
        let lead = store.oidc_user(&c, &who("lead@example.com", "2")).unwrap();
        assert_eq!(store.role_of("lead@example.com"), Some(Role::Owner));
        // The same person again is the same account, whatever their email says now.
        assert_eq!(store.oidc_user(&c, &who("renamed@example.com", "2")).unwrap(), lead);

        store.oidc_user(&c, &who("ana@example.com", "1")).unwrap();
        assert_eq!(store.role_of("ana@example.com"), Some(Role::Member));
        // Accounts made this way have no usable password.
        assert_eq!(store.verify_login("ana@example.com", "!"), None);
        assert_eq!(store.verify_login("ana@example.com", ""), None);

        // A local account with the same name is not taken over silently.
        store.create_user("bo@example.com", "correct-horse-battery", Role::Member).unwrap();
        assert!(store.oidc_user(&c, &who("bo@example.com", "3")).unwrap_err().contains("not linked"));
        let linking = Config { link_existing: true, ..config() };
        store.oidc_user(&linking, &who("bo@example.com", "3")).unwrap();
        // …and another provider identity cannot claim it afterwards.
        assert!(store.oidc_user(&linking, &who("bo@example.com", "4")).is_err());

        // Roles: the last owner stays; removal ends the account.
        assert!(store.set_role("lead@example.com", Role::Member).unwrap_err().contains("at least one owner"));
        store.set_role("ana@example.com", Role::Owner).unwrap();
        store.set_role("lead@example.com", Role::Member).unwrap();
        assert!(store.delete_user("ana@example.com").unwrap_err().contains("at least one owner"));
        store.delete_user("bo@example.com").unwrap();
        assert_eq!(store.role_of("bo@example.com"), None);
        assert!(store.set_role("nobody", Role::Guest).is_err());
    }
}
