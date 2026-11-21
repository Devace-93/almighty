//! `almighty plugin …`: install and operate light plugins.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use almighty_server::plugins::manifest::{Manifest, Permissions, MANIFEST_FILE};
use anyhow::{bail, Context, Result};
use base64::Engine;
use clap::Subcommand;
use serde_json::{json, Value};

use crate::Client;

#[derive(Subcommand)]
pub enum PluginCmd {
    /// Install or update a plugin from a directory with an almighty-plugin.toml.
    Add {
        path: PathBuf,
        /// Approve the permissions without asking.
        #[arg(long, short)]
        yes: bool,
    },
    /// List installed plugins.
    List,
    /// Show one plugin: permissions, panels, commands, last poll.
    Show { id: String },
    Remove { id: String },
    Enable { id: String },
    Disable { id: String },
    /// Store a secret the plugin declared (value from the terminal or stdin).
    Secret {
        id: String,
        name: String,
        /// Remove the secret instead.
        #[arg(long)]
        delete: bool,
    },
    /// Replace the secret a service plugin signs events with (shown once).
    RotateEventsSecret { id: String },
    /// Run the plugin's connector now.
    Poll { id: String },
    /// Print the tree of a panel as JSON.
    Panel { id: String, panel: String },
    /// Run a plugin command; arguments are a JSON value.
    Run { id: String, command: String, args: Option<String> },
    /// Recent log lines of a plugin.
    Logs { id: String },
}

fn list(values: &[String]) -> String {
    if values.is_empty() {
        "none".into()
    } else {
        values.join(", ")
    }
}

fn describe(p: &Permissions) -> Vec<(&'static str, String)> {
    vec![
        ("items", list(&p.items)),
        ("network (outbound hosts)", list(&p.network)),
        ("secrets it may read", list(&p.secrets)),
        ("events", list(&p.events)),
        ("commands agents may call", list(&p.agent_tools)),
    ]
}

fn confirm(question: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("not a terminal: pass --yes to approve the permissions");
    }
    eprint!("{question} [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

async fn add(client: &Client, path: &Path, yes: bool) -> Result<()> {
    let manifest_path = if path.is_dir() { path.join(MANIFEST_FILE) } else { path.to_path_buf() };
    let dir = manifest_path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let text = std::fs::read_to_string(&manifest_path).with_context(|| format!("read {}", manifest_path.display()))?;
    let manifest = Manifest::parse(&text).map_err(anyhow::Error::msg)?;
    // A service plugin is only its manifest; a light plugin also uploads its component.
    let component = match &manifest.wasm {
        Some(wasm) if manifest.kind == "wasm" => {
            let path = dir.join(&wasm.component);
            Some(std::fs::read(&path).with_context(|| format!("read {} (build the plugin first)", path.display()))?)
        }
        _ => None,
    };

    // What is installed now, to point out what a new version adds.
    let before: Option<Permissions> = client
        .get(&format!("/api/plugins/{}", manifest.id))
        .await
        .ok()
        .and_then(|v| serde_json::from_value(v["permissions"].clone()).ok());
    eprintln!("{} {} ({})", manifest.name, manifest.version, manifest.id);
    if let Some(service) = &manifest.service {
        eprintln!("Service at {} (runs on its own; almighty embeds its pages and exchanges signed events)", service.base_url);
    }
    eprintln!("Permissions it asks for:");
    for (label, value) in describe(&manifest.permissions) {
        eprintln!("  {label}: {value}");
    }
    match &before {
        Some(old) if *old == manifest.permissions => eprintln!("Same permissions as the installed version."),
        Some(_) => eprintln!("These differ from the installed version."),
        None => {}
    }
    if !yes && !confirm("Install with these permissions?")? {
        bail!("not installed");
    }
    let mut body = json!({ "manifest": text, "granted": manifest.permissions });
    if let Some(bytes) = &component {
        body["component"] = json!(base64::engine::general_purpose::STANDARD.encode(bytes));
    }
    let v = client.post("/api/plugins", body).await?;
    println!("Installed {} {}", v["id"].as_str().unwrap_or(""), v["version"].as_str().unwrap_or(""));
    if let Some(secret) = v["events_secret"].as_str() {
        eprintln!("Event secret (shown once; give it to the service so both sides can sign events):");
        println!("{secret}");
    }
    let missing: Vec<&String> = manifest
        .permissions
        .secrets
        .iter()
        .filter(|s| !v["secrets_set"].as_array().is_some_and(|set| set.iter().any(|x| x.as_str() == Some(s.as_str()))))
        .collect();
    for name in missing {
        eprintln!("Secret {name} is not set: almighty plugin secret {} {name}", manifest.id);
    }
    Ok(())
}

pub async fn run(client: &Client, cmd: PluginCmd) -> Result<()> {
    let pretty = |v: &Value| serde_json::to_string_pretty(v).unwrap_or_default();
    match cmd {
        PluginCmd::Add { path, yes } => add(client, &path, yes).await?,
        PluginCmd::List => {
            for p in client.get("/api/plugins").await?.as_array().into_iter().flatten() {
                println!(
                    "{}\t{}\t{}\t{}",
                    p["id"].as_str().unwrap_or(""),
                    p["version"].as_str().unwrap_or(""),
                    if p["enabled"] == true { "enabled" } else { "disabled" },
                    p["last_error"].as_str().map(|e| format!("error: {e}")).unwrap_or_default()
                );
            }
        }
        PluginCmd::Show { id } => println!("{}", pretty(&client.get(&format!("/api/plugins/{id}")).await?)),
        PluginCmd::Remove { id } => {
            client.call(reqwest::Method::DELETE, &format!("/api/plugins/{id}"), None).await?;
            println!("Removed {id}");
        }
        PluginCmd::Enable { id } => {
            client.post(&format!("/api/plugins/{id}/enabled"), json!({ "enabled": true })).await?;
        }
        PluginCmd::Disable { id } => {
            client.post(&format!("/api/plugins/{id}/enabled"), json!({ "enabled": false })).await?;
        }
        PluginCmd::Secret { id, name, delete } => {
            let path = format!("/api/plugins/{id}/secrets/{name}");
            if delete {
                client.call(reqwest::Method::DELETE, &path, None).await?;
                println!("Secret {name} removed");
            } else {
                // Never taken from the command line, where it would land in shell history.
                let value = if std::io::stdin().is_terminal() {
                    rpassword::prompt_password(format!("Value for {name}: "))?
                } else {
                    let mut line = String::new();
                    std::io::stdin().lock().read_line(&mut line)?;
                    line
                };
                client.post(&path, json!({ "value": value.trim() })).await?;
                println!("Secret {name} stored");
            }
        }
        PluginCmd::RotateEventsSecret { id } => {
            let v = client.post(&format!("/api/plugins/{id}/events-secret"), json!({})).await?;
            println!("{}", v["events_secret"].as_str().unwrap_or(""));
        }
        PluginCmd::Poll { id } => {
            client.post(&format!("/api/plugins/{id}/poll"), json!({})).await?;
            println!("Polled {id}");
        }
        PluginCmd::Panel { id, panel } => {
            println!("{}", pretty(&client.get(&format!("/api/plugins/{id}/panels/{panel}")).await?["tree"]))
        }
        PluginCmd::Run { id, command, args } => {
            let args: Value = match args {
                Some(text) => serde_json::from_str(&text).context("arguments must be JSON")?,
                None => Value::Null,
            };
            let v = client.post(&format!("/api/plugins/{id}/commands/{command}"), args).await?;
            println!("{}", pretty(&v["result"]));
        }
        PluginCmd::Logs { id } => {
            for line in client.get(&format!("/api/plugins/{id}/logs")).await?.as_array().into_iter().flatten() {
                println!("{}", line.as_str().unwrap_or(""));
            }
        }
    }
    Ok(())
}
