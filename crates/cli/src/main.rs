//! `almighty`: server launcher, terminal client and agent control CLI.

mod attach;
mod creds;
mod mcp;

use std::collections::HashMap;
use std::io::Read;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

#[derive(Parser)]
#[command(name = "almighty", version, about = "Self-hosted workbench for agent terminals")]
struct Cli {
    /// Server URL.
    #[arg(long, global = true, env = "ALMIGHTY_URL", default_value = "http://127.0.0.1:8092")]
    url: String,
    /// Token to use instead of the one saved by `almighty login`. Inside a
    /// session the server sets ALMIGHTY_TOKEN to that session's agent token.
    #[arg(long, global = true, env = "ALMIGHTY_TOKEN", hide_env_values = true)]
    token: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the server in the foreground.
    Serve,
    /// Create the first owner of a fresh server (needs the code it printed).
    Setup {
        #[arg(long)]
        code: String,
        #[arg(long)]
        name: String,
        /// Workspace name shown in clients.
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Sign in and remember the token for this server.
    Login {
        #[arg(long)]
        name: String,
    },
    /// Forget the saved token for this server and revoke it.
    Logout,
    /// Show who the current token belongs to.
    Whoami,
    /// Manage accounts directly in the local data directory (no server needed).
    User {
        #[command(subcommand)]
        cmd: UserCmd,
    },
    /// Long-lived tokens for scripts and MCP clients.
    Token {
        #[command(subcommand)]
        cmd: TokenCmd,
    },
    /// Let the agent of one session read or drive another session.
    Grant {
        /// Session to open up.
        session: String,
        /// Session whose agent gets the access.
        #[arg(long)]
        agent: String,
        /// read, write or none.
        #[arg(long, default_value = "write")]
        scope: String,
    },
    /// List sessions.
    Ls {
        /// Include sessions sent to the background.
        #[arg(long, short)]
        all: bool,
    },
    /// Send a session to the background (it keeps running, off the list).
    Bg { session: String },
    /// Bring a background session back to the list.
    Fg { session: String },
    /// Start a command in a new session and attach this terminal to it.
    Run {
        /// Create the session without attaching.
        #[arg(long, short)]
        detach: bool,
        #[arg(long)]
        title: Option<String>,
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// Attach this terminal to an existing session (detach with Ctrl-\).
    Attach { session: String },
    /// Read and drive a session without attaching (what agents use).
    Term {
        #[command(subcommand)]
        cmd: TermCmd,
    },
    /// Show the agent tree of a session.
    Agents { session: String },
    /// Forward a Claude Code hook payload (stdin) to the server.
    #[command(hide = true)]
    Hook,
    /// MCP server over stdio exposing the terminal control tools.
    Mcp,
}

#[derive(Subcommand)]
enum UserCmd {
    /// Add a user to the database in ALMIGHTY_DATA (asks for a password).
    Add {
        name: String,
        #[arg(long, default_value = "member")]
        role: String,
    },
    List,
}

#[derive(Subcommand)]
enum TokenCmd {
    Create { name: String },
    List,
    Revoke { id: i64 },
}

#[derive(Subcommand)]
enum TermCmd {
    /// Print the rendered screen.
    Read {
        session: String,
        #[arg(long)]
        json: bool,
        /// Also print this many lines of history above the screen.
        #[arg(long, default_value_t = 0)]
        scrollback: usize,
    },
    /// Type text (bracketed paste is used when the application enabled it).
    SendText { session: String, text: String },
    /// Press named keys: Enter, Tab, Esc, Up, C-c, M-x, F5…
    SendKeys { session: String, #[arg(required = true)] keys: Vec<String> },
    /// Wait for a regex on screen and/or for the screen to stay idle.
    Wait {
        session: String,
        #[arg(long)]
        pattern: Option<String>,
        #[arg(long)]
        idle_ms: Option<u64>,
        #[arg(long, default_value_t = 30000)]
        timeout_ms: u64,
    },
    Resize { session: String, cols: u16, rows: u16 },
    /// Stop agent input into a session (or allow it again with --off).
    Lock { session: String, #[arg(long)] off: bool },
    Close { session: String },
}

#[derive(Clone)]
pub struct Client {
    pub url: String,
    pub token: Option<String>,
    http: reqwest::Client,
}

impl Client {
    fn new(url: String, token: Option<String>) -> Self {
        Client { url: url.trim_end_matches('/').to_string(), token, http: reqwest::Client::new() }
    }

    pub async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
        let mut req = self.http.request(method, format!("{}{}", self.url, path));
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let res = req.send().await.with_context(|| format!("cannot reach the server at {}", self.url))?;
        let status = res.status();
        let value: Value = res.json().await.unwrap_or(Value::Null);
        if status == reqwest::StatusCode::UNAUTHORIZED && self.token.is_none() {
            bail!("not signed in to {}: run `almighty login --name <you>`", self.url);
        }
        if !status.is_success() {
            bail!("{}", value.get("error").and_then(Value::as_str).unwrap_or(status.as_str()));
        }
        Ok(value)
    }

    pub async fn get(&self, path: &str) -> Result<Value> {
        self.call(reqwest::Method::GET, path, None).await
    }

    pub async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.call(reqwest::Method::POST, path, Some(body)).await
    }
}

/// Reads a new password twice from the terminal, or once from
/// ALMIGHTY_PASSWORD for scripts.
fn read_password(confirm: bool) -> Result<String> {
    if let Ok(p) = std::env::var("ALMIGHTY_PASSWORD") {
        return Ok(p);
    }
    let first = rpassword::prompt_password("Password: ")?;
    if confirm && rpassword::prompt_password("Repeat password: ")? != first {
        bail!("the passwords do not match");
    }
    Ok(first)
}

/// Lets the agent in the session drive terminals through MCP. The server
/// started by this config inherits the session's own token from the PTY.
fn claude_mcp_config() -> Result<String> {
    let exe = std::env::current_exe()?.to_string_lossy().into_owned();
    Ok(json!({ "mcpServers": { "almighty": { "command": exe, "args": ["mcp"] } } }).to_string())
}

/// Claude Code settings that report subagent lifecycle back to the server.
/// The hook command finds its session through the environment of the PTY.
fn claude_hook_settings() -> Result<String> {
    let exe = std::env::current_exe()?.to_string_lossy().into_owned();
    // POSIX shells need single quotes; cmd.exe and PowerShell take double quotes.
    let command = if cfg!(windows) {
        format!("\"{exe}\" hook")
    } else {
        format!("'{}' hook", exe.replace('\'', "'\\''"))
    };
    let hook = json!([{ "type": "command", "command": command }]);
    Ok(json!({
        "hooks": {
            "PreToolUse": [{ "matcher": "Agent|Task", "hooks": hook }],
            "SubagentStart": [{ "hooks": hook }],
            "SubagentStop": [{ "hooks": hook }],
        }
    })
    .to_string())
}

fn is_claude(command: &[String]) -> bool {
    // file_stem, not file_name: on Windows the launcher is claude.exe or claude.cmd.
    std::path::Path::new(&command[0]).file_stem().is_some_and(|n| n.eq_ignore_ascii_case("claude"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let url = cli.url.trim_end_matches('/').to_string();
    let token = cli.token.or_else(|| creds::load(&url));
    let client = Client::new(url.clone(), token);
    match cli.cmd {
        Cmd::Serve => almighty_server::serve(almighty_server::Config::from_env()).await?,
        Cmd::Setup { code, name, workspace } => {
            let password = read_password(true)?;
            let v = client
                .post("/api/setup", json!({ "code": code, "name": name, "password": password, "workspace": workspace }))
                .await?;
            creds::save(&url, v["token"].as_str().context("no token in the reply")?)?;
            println!("Owner {name} created; signed in to {url}");
        }
        Cmd::Login { name } => {
            let password = read_password(false)?;
            let device = format!("cli on {}", std::env::var("HOSTNAME").or_else(|_| std::env::var("COMPUTERNAME")).unwrap_or_else(|_| "this machine".into()));
            let v = client.post("/api/auth/login", json!({ "name": name, "password": password, "device": device })).await?;
            creds::save(&url, v["token"].as_str().context("no token in the reply")?)?;
            println!("Signed in to {url} as {name} ({})", v["role"].as_str().unwrap_or("?"));
        }
        Cmd::Logout => {
            let _ = client.post("/api/auth/logout", json!({})).await;
            creds::forget(&url)?;
            println!("Signed out of {url}");
        }
        Cmd::Whoami => {
            let v = client.get("/api/auth/me").await?;
            match v["kind"].as_str() {
                Some("agent") => println!("agent of session {} in workspace {}", v["session"].as_str().unwrap_or("?"), v["workspace"].as_str().unwrap_or("?")),
                _ => println!("{} ({}) in workspace {}", v["name"].as_str().unwrap_or("?"), v["role"].as_str().unwrap_or("?"), v["workspace"].as_str().unwrap_or("?")),
            }
        }
        Cmd::User { cmd } => {
            use almighty_server::store::{Role, Store};
            let store = Store::open(&almighty_server::Config::from_env().db_path())?;
            match cmd {
                UserCmd::Add { name, role } => {
                    let role = Role::parse(&role).context("role must be owner, admin, member or guest")?;
                    let password = read_password(true)?;
                    store.create_user(&name, &password, role)?;
                    println!("User {name} ({}) created", role.as_str());
                }
                UserCmd::List => {
                    for u in store.list_users() {
                        println!("{}\t{}\t{}", u.id, u.role.as_str(), u.name);
                    }
                }
            }
        }
        Cmd::Token { cmd } => match cmd {
            TokenCmd::Create { name } => {
                let v = client.post("/api/tokens", json!({ "name": name })).await?;
                eprintln!("Token {} created. It is shown only once:", v["id"]);
                println!("{}", v["token"].as_str().unwrap_or(""));
            }
            TokenCmd::List => {
                let v = client.get("/api/tokens").await?;
                for t in v["tokens"].as_array().into_iter().flatten() {
                    println!("{}\t{}\t{}", t["id"], t["kind"].as_str().unwrap_or(""), t["name"].as_str().unwrap_or(""));
                }
            }
            TokenCmd::Revoke { id } => {
                client.call(reqwest::Method::DELETE, &format!("/api/tokens/{id}"), None).await?;
                println!("Token {id} revoked");
            }
        },
        Cmd::Grant { session, agent, scope } => {
            let scope = match scope.as_str() {
                "read" | "write" => json!(scope),
                "none" => Value::Null,
                _ => bail!("scope must be read, write or none"),
            };
            client.post(&format!("/api/sessions/{session}/grants"), json!({ "agent": agent, "scope": scope })).await?;
            println!("agent of {agent} -> {session}: {scope}");
        }
        Cmd::Bg { session } => {
            client.post(&format!("/api/sessions/{session}/background"), json!({ "background": true })).await?;
        }
        Cmd::Fg { session } => {
            client.post(&format!("/api/sessions/{session}/background"), json!({ "background": false })).await?;
        }
        Cmd::Ls { all } => {
            let v = client.get(if all { "/api/sessions?all=1" } else { "/api/sessions" }).await?;
            for s in v["sessions"].as_array().into_iter().flatten() {
                let state = if s["exited"].as_bool() == Some(true) {
                    "exited"
                } else if s["background"].as_bool() == Some(true) {
                    "background"
                } else {
                    "running"
                };
                println!(
                    "{}\t{}\t{}x{}\tagents:{}\t{}",
                    s["id"].as_str().unwrap_or(""),
                    state,
                    s["cols"],
                    s["rows"],
                    s["agents"],
                    s["title"].as_str().unwrap_or("")
                );
            }
        }
        Cmd::Run { detach, title, mut command } => {
            if is_claude(&command) {
                if !command.iter().any(|a| a == "--settings") {
                    command.push("--settings".into());
                    command.push(claude_hook_settings()?);
                }
                if !command.iter().any(|a| a == "--mcp-config") {
                    command.push("--mcp-config".into());
                    command.push(claude_mcp_config()?);
                }
            }
            let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 32));
            let env: HashMap<String, String> = HashMap::new();
            let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
            let s = client
                .post(
                    "/api/sessions",
                    json!({ "command": command, "cwd": cwd, "cols": cols, "rows": rows, "title": title, "env": env }),
                )
                .await?;
            let id = s["id"].as_str().context("no session id")?.to_string();
            if detach {
                println!("{id}");
            } else {
                attach::attach(&client, &id).await?;
            }
        }
        Cmd::Attach { session } => attach::attach(&client, &session).await?,
        Cmd::Agents { session } => {
            let v = client.get(&format!("/api/sessions/{session}/agents")).await?;
            for a in v["agents"].as_array().into_iter().flatten() {
                println!(
                    "{}\t{}\t{}\t{}",
                    a["agent_id"].as_str().unwrap_or(""),
                    a["status"].as_str().unwrap_or(""),
                    a["agent_type"].as_str().unwrap_or(""),
                    a["description"].as_str().unwrap_or("")
                );
            }
        }
        Cmd::Hook => {
            // Never break the agent: any failure here is silent and exits 0.
            let mut body = String::new();
            let _ = std::io::stdin().read_to_string(&mut body);
            if let Ok(session) = std::env::var("ALMIGHTY_SESSION") {
                if let Ok(payload) = serde_json::from_str::<Value>(&body) {
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_secs(3),
                        client.post(&format!("/api/hooks/claude?session={session}"), payload),
                    )
                    .await;
                }
            }
        }
        Cmd::Mcp => mcp::serve(client).await?,
        Cmd::Term { cmd } => match cmd {
            TermCmd::Read { session, json, scrollback } => {
                let v = client.get(&format!("/api/sessions/{session}/screen?scrollback={scrollback}")).await?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&v)?);
                } else {
                    if let Some(history) = v["scrollback"].as_str().filter(|h| !h.is_empty()) {
                        println!("{history}");
                    }
                    println!("{}", v["text"].as_str().unwrap_or(""));
                }
            }
            TermCmd::SendText { session, text } => {
                client.post(&format!("/api/sessions/{session}/input"), json!({ "text": text })).await?;
            }
            TermCmd::SendKeys { session, keys } => {
                client.post(&format!("/api/sessions/{session}/input"), json!({ "keys": keys })).await?;
            }
            TermCmd::Wait { session, pattern, idle_ms, timeout_ms } => {
                let v = client
                    .post(
                        &format!("/api/sessions/{session}/wait"),
                        json!({ "pattern": pattern, "idle_ms": idle_ms, "timeout_ms": timeout_ms }),
                    )
                    .await?;
                println!("{}", v["reason"].as_str().unwrap_or(""));
                if v["matched"].as_bool() != Some(true) {
                    std::process::exit(1);
                }
            }
            TermCmd::Resize { session, cols, rows } => {
                client.post(&format!("/api/sessions/{session}/resize"), json!({ "cols": cols, "rows": rows })).await?;
            }
            TermCmd::Lock { session, off } => {
                client.post(&format!("/api/sessions/{session}/lock"), json!({ "locked": !off })).await?;
            }
            TermCmd::Close { session } => {
                client.call(reqwest::Method::DELETE, &format!("/api/sessions/{session}"), None).await?;
            }
        },
    }
    Ok(())
}
