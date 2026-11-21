//! `almighty`: server launcher, terminal client and agent control CLI.

mod acp;
mod attach;
mod creds;
mod mcp;
mod plugin;

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
        /// Account name, for a local account with a password.
        #[arg(long, required_unless_present = "sso")]
        name: Option<String>,
        /// Sign in through the workspace's identity provider, in your browser.
        #[arg(long, conflicts_with = "name")]
        sso: bool,
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
    /// List the windows of the tmux server next to almighty, or open one as a session.
    Tmux {
        /// The window to open, as `session:window` (e.g. `work:2`). Without it, lists the windows.
        target: Option<String>,
    },
    /// Install and operate plugins.
    Plugin {
        #[command(subcommand)]
        cmd: plugin::PluginCmd,
    },
    /// Your triaged inbox: what needs a reply, an action, preparation, or nothing.
    Inbox {
        #[command(subcommand)]
        cmd: Option<InboxCmd>,
        /// Only one bucket: reply, act, prepare, fyi or noise.
        #[arg(long)]
        bucket: Option<String>,
        /// open (default), done or all.
        #[arg(long, default_value = "open")]
        show: String,
    },
    /// Phone notifications through an ntfy topic or a UnifiedPush endpoint.
    Push {
        #[command(subcommand)]
        cmd: PushCmd,
    },
    /// Your plan: tasks and events to prepare for.
    Plan {
        #[command(subcommand)]
        cmd: Option<PlanCmd>,
        /// Also show what is done.
        #[arg(long)]
        done: bool,
    },
    /// What happened that needs nothing from you, grouped by source.
    Digest {
        #[arg(long, default_value_t = 24)]
        hours: u64,
        /// Have the server's model write it out.
        #[arg(long)]
        written: bool,
    },
    /// List items that connectors brought in.
    Items {
        #[arg(long)]
        source: Option<String>,
        #[arg(long, default_value_t = 30)]
        limit: u32,
    },
    /// Forward a Claude Code hook payload (stdin) to the server.
    #[command(hide = true)]
    Hook,
    /// MCP server over stdio exposing the terminal control tools.
    Mcp,
    /// Talk to an agent that speaks the Agent Client Protocol (ACP) from this terminal.
    Acp {
        /// Send one prompt and end, instead of reading prompts from the terminal.
        #[arg(long)]
        prompt: Option<String>,
        /// What to do when the agent asks for permission.
        #[arg(long, value_enum, default_value = "ask")]
        permissions: acp::Policy,
        /// The agent's command, e.g. `-- my-agent --acp`.
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    #[command(hide = true)]
    AcpEchoAgent,
}

#[derive(Subcommand)]
enum PlanCmd {
    /// Add a task, optionally due in some days.
    Add {
        title: String,
        #[arg(long)]
        in_days: Option<f64>,
    },
    /// Mark a task as done (or open again with --undo).
    Done {
        id: i64,
        #[arg(long)]
        undo: bool,
    },
    Rm { id: i64 },
}

#[derive(Subcommand)]
enum PushCmd {
    /// Register an endpoint, e.g. https://ntfy.example.org/my-secret-topic
    Add {
        /// The topic or endpoint address (not the almighty server's URL).
        endpoint: String,
        #[arg(long, default_value = "")]
        label: String,
    },
    /// Your endpoints and what the operator allows.
    List,
    Remove { id: i64 },
    /// Send a test notification to your endpoints.
    Test,
}

#[derive(Subcommand)]
enum InboxCmd {
    /// Move an item to another bucket (remembered for similar items).
    Move { item: String, bucket: String },
    /// Set the priority of an item, 0 to 3.
    Priority { item: String, priority: u8 },
    /// Mark an item as done (or open again with --undo).
    Done {
        item: String,
        #[arg(long)]
        undo: bool,
    },
    /// Hide an item for some hours.
    Snooze { item: String, hours: u64 },
    /// Show the triage preferences the server reads for you, and their file.
    Preferences,
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
    /// Change a person's role through the server (owner, admin, member, guest).
    Role { name: String, role: String },
    /// Remove a person through the server; their tokens stop working.
    Remove { name: String },
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

/// Waits on a loopback port for the server to send the browser back with a
/// one-time code, after the person signs in at the identity provider.
async fn sso_code(url: &str) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // Ask first: without a provider the address below would only show an error page.
    let workspace: Value = reqwest::get(format!("{url}/api/workspace")).await.context("the server did not answer")?.json().await?;
    if workspace["sso"].is_null() {
        bail!("this workspace has no identity provider configured: sign in with --name, or add [oidc] to workspace.toml");
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let start = format!("{url}/api/auth/oidc/start?device=cli&redirect=http://127.0.0.1:{port}/");
    eprintln!("Open this address in your browser to sign in:\n{start}");
    let wait = async {
        loop {
            let (mut stream, _) = listener.accept().await?;
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let code = request
                .split_whitespace()
                .nth(1)
                .and_then(|path| path.split(['?', '&']).find_map(|kv| kv.strip_prefix("almighty_code=")))
                .map(str::to_string);
            let page = "Signed in. You can close this tab and go back to the terminal.";
            let _ = stream
                .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}", page.len()).as_bytes())
                .await;
            if let Some(code) = code {
                return anyhow::Ok(code);
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(300), wait).await.context("the sign-in was not finished in five minutes")?
}

#[tokio::main]
async fn main() -> Result<()> {
    // `almighty inbox | head` should end quietly, not with a broken-pipe panic.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
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
        Cmd::Login { sso: true, .. } => {
            let code = sso_code(&url).await?;
            let v = client.post("/api/auth/oidc/exchange", json!({ "code": code })).await?;
            creds::save(&url, v["token"].as_str().context("no token in the reply")?)?;
            println!("Signed in to {url} as {} ({})", v["name"].as_str().unwrap_or("?"), v["role"].as_str().unwrap_or("?"));
        }
        Cmd::Login { name, .. } => {
            let name = name.context("--name is required")?;
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
        Cmd::User { cmd: UserCmd::Role { name, role } } => {
            let v = client.post(&format!("/api/users/{name}/role"), json!({ "role": role })).await?;
            println!("{name} is now {}", v["role"].as_str().unwrap_or(&role));
        }
        Cmd::User { cmd: UserCmd::Remove { name } } => {
            client.call(reqwest::Method::DELETE, &format!("/api/users/{name}"), None).await?;
            println!("{name} removed");
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
                UserCmd::Role { .. } | UserCmd::Remove { .. } => unreachable!("handled above"),
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
        Cmd::Tmux { target: None } => {
            let v = client.get("/api/tmux").await?;
            if v["available"] == false {
                anyhow::bail!("{}", v["reason"].as_str().unwrap_or("tmux is not available"));
            }
            for w in v["windows"].as_array().into_iter().flatten() {
                println!(
                    "{}:{}\t{}\t{}\t{}",
                    w["session"].as_str().unwrap_or(""),
                    w["index"],
                    w["name"].as_str().unwrap_or(""),
                    w["command"].as_str().unwrap_or(""),
                    w["path"].as_str().unwrap_or("")
                );
            }
        }
        Cmd::Tmux { target: Some(target) } => {
            let (session, window) = target
                .rsplit_once(':')
                .and_then(|(s, w)| Some((s, w.parse::<u32>().ok()?)))
                .ok_or_else(|| anyhow::anyhow!("write the window as session:window, e.g. work:2"))?;
            let v = client.post("/api/tmux/adopt", json!({ "session": session, "window": window })).await?;
            println!("{}\t{}", v["id"].as_str().unwrap_or(""), v["title"].as_str().unwrap_or(""));
        }
        Cmd::Acp { prompt, permissions, command } => acp::run(client, command, prompt, permissions).await?,
        Cmd::AcpEchoAgent => acp::echo_agent().await?,
        Cmd::Plugin { cmd } => plugin::run(&client, cmd).await?,
        Cmd::Plan { cmd: None, done } => {
            let v = client.get(if done { "/api/planner?done=1" } else { "/api/planner" }).await?;
            for e in v["entries"].as_array().into_iter().flatten() {
                let id = match e["kind"].as_str() {
                    Some("task") => e["task"]["id"].to_string(),
                    _ => e["item"]["id"].as_str().unwrap_or("").to_string(),
                };
                println!("{}\t{}\t{}", e["when"].as_str().unwrap_or(""), id, e["title"].as_str().unwrap_or(""));
            }
        }
        Cmd::Plan { cmd: Some(cmd), .. } => match cmd {
            PlanCmd::Add { title, in_days } => {
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
                let due = in_days.map(|d| now + (d.max(0.0) * 86400.0) as u64);
                let v = client.post("/api/planner", json!({ "title": title, "due": due })).await?;
                println!("Task {} added", v["task"]["id"]);
            }
            PlanCmd::Done { id, undo } => {
                client.post(&format!("/api/planner/{id}"), json!({ "done": !undo })).await?;
                println!("Task {id} {}", if undo { "reopened" } else { "done" });
            }
            PlanCmd::Rm { id } => {
                client.call(reqwest::Method::DELETE, &format!("/api/planner/{id}"), None).await?;
                println!("Task {id} removed");
            }
        },
        Cmd::Digest { hours, written } => {
            if written {
                let v = client.post(&format!("/api/digest/summary?hours={hours}"), json!({})).await?;
                println!("{}", v["summary"].as_str().unwrap_or(""));
            } else {
                let v = client.get(&format!("/api/digest?hours={hours}")).await?;
                for g in v["groups"].as_array().into_iter().flatten() {
                    println!("{} {} ({})", g["source"].as_str().unwrap_or(""), g["thread"].as_str().unwrap_or(""), g["count"]);
                    for i in g["items"].as_array().into_iter().flatten() {
                        println!("  {}", i["title"].as_str().unwrap_or(""));
                    }
                }
            }
        }
        Cmd::Push { cmd } => {
            let show = |v: &Value| {
                for e in v["endpoints"].as_array().into_iter().flatten() {
                    let state = match (e["last_error"].as_str(), e["last_ok_at"].as_u64()) {
                        (Some(err), _) => format!("error: {err}"),
                        (None, Some(_)) => "ok".to_string(),
                        (None, None) => "not used yet".to_string(),
                    };
                    println!("{}\t{}\t{}\t{}", e["id"], e["label"].as_str().unwrap_or(""), e["url"].as_str().unwrap_or(""), state);
                }
            };
            match cmd {
                PushCmd::Add { endpoint, label } => {
                    let v = client.post("/api/push", json!({ "url": endpoint, "label": label })).await?;
                    println!("Endpoint {} added. Try it with `almighty push test`.", v["id"]);
                }
                PushCmd::List => {
                    let v = client.get("/api/push").await?;
                    let hosts: Vec<&str> = v["allowed_hosts"].as_array().into_iter().flatten().filter_map(|h| h.as_str()).collect();
                    if hosts.is_empty() {
                        eprintln!("Push is off: no host is listed under [push] allowed_hosts in workspace.toml.");
                    } else {
                        eprintln!("Allowed hosts: {}. Content: {}.", hosts.join(", "), v["content"].as_str().unwrap_or(""));
                    }
                    show(&v);
                }
                PushCmd::Remove { id } => {
                    client.call(reqwest::Method::DELETE, &format!("/api/push/{id}"), None).await?;
                    println!("Endpoint {id} removed");
                }
                PushCmd::Test => {
                    let v = client.post("/api/push/test", json!({})).await?;
                    println!("Delivered to {} endpoint(s)", v["delivered"]);
                    show(&v);
                }
            }
        }
        Cmd::Inbox { cmd: None, bucket, show } => {
            let mut path = format!("/api/inbox?show={show}");
            if let Some(b) = bucket {
                path.push_str(&format!("&bucket={b}"));
            }
            let v = client.get(&path).await?;
            for e in v["items"].as_array().into_iter().flatten() {
                let (i, t) = (&e["item"], &e["triage"]);
                println!(
                    "{}\t{}\tp{}\t{}\t{}",
                    i["id"].as_str().unwrap_or(""),
                    t["bucket"].as_str().unwrap_or(""),
                    t["priority"],
                    i["source"].as_str().unwrap_or(""),
                    i["title"].as_str().unwrap_or("")
                );
            }
        }
        Cmd::Inbox { cmd: Some(cmd), .. } => {
            let (item, body) = match cmd {
                InboxCmd::Move { item, bucket } => (item, json!({ "bucket": bucket })),
                InboxCmd::Priority { item, priority } => (item, json!({ "priority": priority })),
                InboxCmd::Done { item, undo } => (item, json!({ "done": !undo })),
                InboxCmd::Snooze { item, hours } => {
                    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
                    (item, json!({ "snooze_until": now + hours * 3600 }))
                }
                InboxCmd::Preferences => {
                    let v = client.get("/api/inbox/preferences").await?;
                    println!("{}", serde_json::to_string_pretty(&v)?);
                    return Ok(());
                }
            };
            let v = client.post(&format!("/api/inbox/{item}"), body).await?;
            let t = &v["triage"];
            println!("{item}: {} p{}{}", t["bucket"].as_str().unwrap_or(""), t["priority"], if t["done"] == true { " (done)" } else { "" });
        }
        Cmd::Items { source, limit } => {
            let mut path = format!("/api/items?limit={limit}");
            if let Some(s) = source {
                path.push_str(&format!("&source={s}"));
            }
            for i in client.get(&path).await?.as_array().into_iter().flatten() {
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    i["source"].as_str().unwrap_or(""),
                    i["kind"].as_str().unwrap_or(""),
                    i["state"].as_str().unwrap_or(""),
                    i["title"].as_str().unwrap_or(""),
                    i["url"].as_str().unwrap_or("")
                );
            }
        }
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
