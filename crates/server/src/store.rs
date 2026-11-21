//! Persistent state: users, tokens, audit log. One SQLite file.
//! Plugin and item tables live in `plugins::db`.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use base64::Engine;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::session::now_secs;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Guest,
    Member,
    Admin,
    Owner,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Guest => "guest",
            Role::Member => "member",
            Role::Admin => "admin",
            Role::Owner => "owner",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        Some(match s {
            "guest" => Role::Guest,
            "member" => Role::Member,
            "admin" => Role::Admin,
            "owner" => Role::Owner,
            _ => return None,
        })
    }
}

/// Who is making a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    User { id: i64, name: String, role: Role, token_id: i64 },
    /// The agent running inside terminal session `session`.
    Agent { session: String, token_id: i64 },
}

impl Principal {
    pub fn label(&self) -> String {
        match self {
            Principal::User { name, .. } => format!("user:{name}"),
            Principal::Agent { session, .. } => format!("agent:{session}"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenInfo {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub created_at: u64,
    pub last_used_at: Option<u64>,
    pub expires_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UserInfo {
    pub id: i64,
    pub name: String,
    pub role: Role,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry {
    pub at: u64,
    pub actor: String,
    pub session: Option<String>,
    pub action: String,
    pub detail: String,
}

pub struct Store {
    pub(crate) conn: Mutex<Connection>,
    /// Encrypts plugin secrets at rest; kept in a file next to the database.
    pub(crate) secret_key: [u8; 32],
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS users (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE COLLATE NOCASE,
    password_hash TEXT NOT NULL,
    role TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS tokens (
    id INTEGER PRIMARY KEY,
    hash TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL,            -- login | api | agent
    user_id INTEGER REFERENCES users(id) ON DELETE CASCADE,
    session TEXT,                  -- terminal session of an agent token
    name TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_used_at INTEGER,
    expires_at INTEGER
);
CREATE TABLE IF NOT EXISTS audit (
    id INTEGER PRIMARY KEY,
    at INTEGER NOT NULL,
    actor TEXT NOT NULL,
    session TEXT,
    action TEXT NOT NULL,
    detail TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS audit_session ON audit(session, id);
";

/// Tokens are random and long, so a fast hash is enough to store them.
fn token_hash(token: &str) -> String {
    Sha256::digest(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn new_token(kind: &str) -> String {
    let bytes: [u8; 32] = rand::random();
    format!("alm_{kind}_{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// Reads the key that encrypts secrets, creating it on first use. Without
/// this file the stored secrets cannot be read: back it up apart from the
/// database, or set the secrets again after restoring.
fn load_secret_key(path: &Path) -> Result<[u8; 32]> {
    if let Ok(bytes) = std::fs::read(path) {
        return <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| anyhow::anyhow!("{} is not a 32-byte key", path.display()));
    }
    let key: [u8; 32] = rand::random();
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    options.open(path).and_then(|mut f| f.write_all(&key)).with_context(|| format!("create {}", path.display()))?;
    Ok(key)
}

pub fn valid_user_name(name: &str) -> bool {
    (1..=64).contains(&name.len()) && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'))
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Self::init(conn, load_secret_key(&path.with_file_name("secret.key"))?)
    }

    pub fn in_memory() -> Result<Store> {
        Self::init(Connection::open_in_memory()?, rand::random())
    }

    fn init(conn: Connection, secret_key: [u8; 32]) -> Result<Store> {
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch(crate::plugins::db::SCHEMA)?;
        // Columns added after the first plugin release; an error means they exist.
        for column in ["direct INTEGER NOT NULL DEFAULT 0", "mentions TEXT NOT NULL DEFAULT '[]'", "due INTEGER", "rev INTEGER NOT NULL DEFAULT 0"] {
            let _ = conn.execute(&format!("ALTER TABLE items ADD COLUMN {column}"), []);
        }
        conn.execute_batch(crate::triage::SCHEMA)?;
        // Terminal sessions do not survive a restart, so neither do their tokens.
        conn.execute("DELETE FROM tokens WHERE kind = 'agent'", [])?;
        Ok(Store { conn: Mutex::new(conn), secret_key })
    }

    pub fn workspace_name(&self) -> String {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT value FROM meta WHERE key = 'workspace'", [], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
            .unwrap_or_else(|| "almighty".to_string())
    }

    pub fn set_workspace_name(&self, name: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO meta(key, value) VALUES('workspace', ?1) ON CONFLICT(key) DO UPDATE SET value = ?1",
            [name],
        )?;
        Ok(())
    }

    pub fn user_count(&self) -> i64 {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0)).unwrap_or(0)
    }

    pub fn create_user(&self, name: &str, password: &str, role: Role) -> Result<i64> {
        anyhow::ensure!(valid_user_name(name), "user name: 1-64 letters, digits or . _ - @");
        anyhow::ensure!(password.chars().count() >= 10, "password must be at least 10 characters");
        let salt_bytes: [u8; 16] = rand::random();
        let salt = SaltString::encode_b64(&salt_bytes).map_err(|e| anyhow::anyhow!("salt: {e}"))?;
        let hash = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map_err(|e| anyhow::anyhow!("hash password: {e}"))?
            .to_string();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO users(name, password_hash, role, created_at) VALUES(?1, ?2, ?3, ?4)",
            params![name, hash, role.as_str(), now_secs() as i64],
        )
        .map_err(|e| match e {
            rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::ConstraintViolation => {
                anyhow::anyhow!("user {name} already exists")
            }
            e => e.into(),
        })?;
        Ok(conn.last_insert_rowid())
    }

    /// Creates the first user as owner, only if there are no users at all.
    /// The check and the insert share one lock, so two setups cannot both win.
    pub fn create_first_owner(&self, name: &str, password: &str) -> Result<Option<i64>> {
        if self.user_count() > 0 {
            return Ok(None);
        }
        match self.create_user(name, password, Role::Owner) {
            Ok(id) => {
                let conn = self.conn.lock().unwrap();
                // Lost a race: keep the earliest row only.
                let first: i64 = conn.query_row("SELECT MIN(id) FROM users", [], |r| r.get(0))?;
                if first != id {
                    conn.execute("DELETE FROM users WHERE id = ?1", [id])?;
                    return Ok(None);
                }
                Ok(Some(id))
            }
            Err(e) => Err(e),
        }
    }

    pub fn list_users(&self) -> Vec<UserInfo> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare("SELECT id, name, role, created_at FROM users ORDER BY id") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map([], |r| {
            Ok(UserInfo {
                id: r.get(0)?,
                name: r.get(1)?,
                role: Role::parse(&r.get::<_, String>(2)?).unwrap_or(Role::Guest),
                created_at: r.get::<_, i64>(3)? as u64,
            })
        });
        rows.map(|it| it.flatten().collect()).unwrap_or_default()
    }

    /// Returns the user id when the password matches.
    pub fn verify_login(&self, name: &str, password: &str) -> Option<i64> {
        let row: Option<(i64, String)> = {
            let conn = self.conn.lock().unwrap();
            conn.query_row("SELECT id, password_hash FROM users WHERE name = ?1", [name], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()
                .ok()
                .flatten()
        };
        // Hash even for unknown names so timing does not reveal which exist.
        const DUMMY: &str = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzb21lc2FsdA$Zm9vYmFyZm9vYmFyZm9vYmFyZm9vYmFyZm9vYmFyZm8";
        let (id, hash) = match row {
            Some((id, hash)) => (Some(id), hash),
            None => (None, DUMMY.to_string()),
        };
        let ok = PasswordHash::new(&hash)
            .map(|parsed| Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok())
            .unwrap_or(false);
        if ok { id } else { None }
    }

    /// Issues a token for a user. `ttl_secs = None` never expires.
    pub fn create_user_token(&self, user_id: i64, kind: &str, name: &str, ttl_secs: Option<u64>) -> Result<(i64, String)> {
        let token = new_token(kind);
        let now = now_secs();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO tokens(hash, kind, user_id, name, created_at, expires_at) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![token_hash(&token), kind, user_id, name, now as i64, ttl_secs.map(|t| (now + t) as i64)],
        )?;
        Ok((conn.last_insert_rowid(), token))
    }

    /// Issues the token handed to the process of a terminal session.
    pub fn create_agent_token(&self, session: &str) -> Result<(i64, String)> {
        let token = new_token("agent");
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO tokens(hash, kind, session, name, created_at) VALUES(?1, 'agent', ?2, ?3, ?4)",
            params![token_hash(&token), session, format!("session {session}"), now_secs() as i64],
        )?;
        Ok((conn.last_insert_rowid(), token))
    }

    pub fn delete_agent_tokens(&self, session: &str) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute("DELETE FROM tokens WHERE kind = 'agent' AND session = ?1", [session]);
    }

    pub fn principal_for(&self, token: &str) -> Option<Principal> {
        let now = now_secs() as i64;
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT t.id, t.kind, t.session, t.expires_at, t.last_used_at, u.id, u.name, u.role
                 FROM tokens t LEFT JOIN users u ON u.id = t.user_id WHERE t.hash = ?1",
                [token_hash(token)],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                        r.get::<_, Option<i64>>(4)?,
                        r.get::<_, Option<i64>>(5)?,
                        r.get::<_, Option<String>>(6)?,
                        r.get::<_, Option<String>>(7)?,
                    ))
                },
            )
            .optional()
            .ok()
            .flatten()?;
        let (token_id, kind, session, expires_at, last_used, user_id, user_name, role) = row;
        if expires_at.is_some_and(|e| e <= now) {
            let _ = conn.execute("DELETE FROM tokens WHERE id = ?1", [token_id]);
            return None;
        }
        // One write per minute per token is plenty for "last used".
        if last_used.is_none_or(|l| now - l >= 60) {
            let _ = conn.execute("UPDATE tokens SET last_used_at = ?1 WHERE id = ?2", params![now, token_id]);
        }
        if kind == "agent" {
            return Some(Principal::Agent { session: session?, token_id });
        }
        Some(Principal::User { id: user_id?, name: user_name?, role: Role::parse(&role?)?, token_id })
    }

    pub fn list_user_tokens(&self, user_id: i64) -> Vec<TokenInfo> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn
            .prepare("SELECT id, name, kind, created_at, last_used_at, expires_at FROM tokens WHERE user_id = ?1 ORDER BY id")
        {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map([user_id], |r| {
            Ok(TokenInfo {
                id: r.get(0)?,
                name: r.get(1)?,
                kind: r.get(2)?,
                created_at: r.get::<_, i64>(3)? as u64,
                last_used_at: r.get::<_, Option<i64>>(4)?.map(|v| v as u64),
                expires_at: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
            })
        });
        rows.map(|it| it.flatten().collect()).unwrap_or_default()
    }

    /// Deletes a token that belongs to `user_id`. Returns whether it existed.
    pub fn delete_user_token(&self, user_id: i64, token_id: i64) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM tokens WHERE id = ?1 AND user_id = ?2", params![token_id, user_id]).unwrap_or(0) > 0
    }

    pub fn audit(&self, actor: &str, session: Option<&str>, action: &str, detail: &str) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO audit(at, actor, session, action, detail) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![now_secs() as i64, actor, session, action, detail],
        );
    }

    pub fn audit_tail(&self, session: Option<&str>, limit: u32) -> Vec<AuditEntry> {
        let conn = self.conn.lock().unwrap();
        let sql = "SELECT at, actor, session, action, detail FROM audit
                   WHERE (?1 IS NULL OR session = ?1) ORDER BY id DESC LIMIT ?2";
        let mut stmt = match conn.prepare(sql) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![session, limit], |r| {
            Ok(AuditEntry {
                at: r.get::<_, i64>(0)? as u64,
                actor: r.get(1)?,
                session: r.get(2)?,
                action: r.get(3)?,
                detail: r.get(4)?,
            })
        });
        let mut out: Vec<AuditEntry> = rows.map(|it| it.flatten().collect()).unwrap_or_default();
        out.reverse();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn users_logins_and_tokens() {
        let store = Store::in_memory().unwrap();
        assert_eq!(store.user_count(), 0);
        assert!(store.create_user("ana", "short", Role::Owner).is_err());
        assert!(store.create_user("bad name", "long-enough-password", Role::Owner).is_err());
        let id = store.create_first_owner("ana", "long-enough-password").unwrap().unwrap();
        assert!(store.create_first_owner("eve", "long-enough-password").unwrap().is_none());
        assert!(store.create_user("ANA", "long-enough-password", Role::Member).is_err());

        assert_eq!(store.verify_login("ana", "long-enough-password"), Some(id));
        assert_eq!(store.verify_login("Ana", "long-enough-password"), Some(id));
        assert_eq!(store.verify_login("ana", "wrong-password-here"), None);
        assert_eq!(store.verify_login("nobody", "long-enough-password"), None);

        let (token_id, token) = store.create_user_token(id, "login", "laptop", Some(3600)).unwrap();
        assert!(token.starts_with("alm_login_"));
        match store.principal_for(&token) {
            Some(Principal::User { name, role, .. }) => {
                assert_eq!(name, "ana");
                assert_eq!(role, Role::Owner);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(store.principal_for("alm_login_nope").is_none());
        assert_eq!(store.list_user_tokens(id).len(), 1);
        assert!(!store.delete_user_token(id + 1, token_id));
        assert!(store.delete_user_token(id, token_id));
        assert!(store.principal_for(&token).is_none());

        let (_, expired) = store.create_user_token(id, "login", "old", Some(0)).unwrap();
        assert!(store.principal_for(&expired).is_none());
    }

    #[test]
    fn agent_tokens_and_audit() {
        let store = Store::in_memory().unwrap();
        let (_, token) = store.create_agent_token("s7").unwrap();
        assert_eq!(
            store.principal_for(&token).map(|p| p.label()),
            Some("agent:s7".to_string())
        );
        store.delete_agent_tokens("s7");
        assert!(store.principal_for(&token).is_none());

        store.audit("agent:s7", Some("s8"), "input", "keys: Enter");
        store.audit("user:ana", None, "login", "");
        assert_eq!(store.audit_tail(Some("s8"), 10).len(), 1);
        assert_eq!(store.audit_tail(None, 10).len(), 2);
    }

    #[test]
    fn roles_order() {
        assert!(Role::Owner > Role::Admin && Role::Admin > Role::Member && Role::Member > Role::Guest);
        assert_eq!(Role::parse("admin"), Some(Role::Admin));
        assert_eq!(Role::parse("root"), None);
    }
}
