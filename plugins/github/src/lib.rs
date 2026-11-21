//! GitHub connector: open pull requests as items, plus a panel that lists them.

wit_bindgen::generate!({ world: "plugin", path: "../../wit" });

use almighty::plugin::host;
use serde_json::{json, Value};

struct Github;

struct Config {
    api: String,
    repos: Vec<String>,
    review_requests: bool,
}

fn config() -> Config {
    let v: Value = serde_json::from_str(&host::config()).unwrap_or(Value::Null);
    Config {
        api: v["api"].as_str().unwrap_or("https://api.github.com").trim_end_matches('/').to_string(),
        repos: v["repos"]
            .as_array()
            .map(|a| a.iter().filter_map(|r| r.as_str()).filter(|r| valid_repo(r)).map(str::to_string).collect())
            .unwrap_or_default(),
        review_requests: v["review_requests"].as_bool().unwrap_or(true),
    }
}

/// `owner/name`, nothing that could change the request path.
fn valid_repo(repo: &str) -> bool {
    let ok = |part: &str| {
        !part.is_empty() && part != "." && part != ".." && part.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    };
    matches!(repo.split_once('/'), Some((owner, name)) if ok(owner) && ok(name))
}

fn get(url: &str, token: Option<&str>) -> Result<Value, String> {
    let mut headers = vec![
        ("accept".to_string(), "application/vnd.github+json".to_string()),
        ("x-github-api-version".to_string(), "2022-11-28".to_string()),
    ];
    if let Some(token) = token {
        headers.push(("authorization".to_string(), format!("Bearer {token}")));
    }
    let response = host::http_fetch(&host::HttpRequest { method: "GET".into(), url: url.into(), headers, body: None })?;
    let body: Value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
    if response.status != 200 {
        let message = body["message"].as_str().unwrap_or("no message");
        return Err(format!("GitHub answered {} for {url}: {message}", response.status));
    }
    Ok(body)
}

/// Seconds since 1970 from `2026-01-31T12:34:56Z`.
fn unix_time(iso: &str) -> Option<u64> {
    let n = |range: std::ops::Range<usize>| iso.get(range)?.parse::<i64>().ok();
    let (y, m, d, h, min, s) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    // Days from civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    u64::try_from(days * 86_400 + h * 3600 + min * 60 + s).ok()
}

/// `owner/name` from an API URL that ends in `/repos/owner/name`.
fn repo_of(repository_url: &str) -> Option<String> {
    let mut parts = repository_url.rsplit('/');
    let (name, owner) = (parts.next()?, parts.next()?);
    Some(format!("{owner}/{name}"))
}

fn item(repo: &str, pr: &Value, kind: &str) -> Option<Value> {
    let number = pr["number"].as_u64()?;
    let body: String = pr["body"].as_str().unwrap_or("").chars().take(4000).collect();
    Some(json!({
        "external_id": format!("{repo}#{number}"),
        "source": "github",
        "kind": kind,
        "state": if pr["draft"] == true { "draft" } else { "open" },
        "title": pr["title"].as_str().unwrap_or("(no title)"),
        "body": body,
        "url": pr["html_url"].as_str().unwrap_or(""),
        "thread": repo,
        "actors": [pr["user"]["login"].as_str().unwrap_or("")],
        "received_at": pr["created_at"].as_str().and_then(unix_time),
        // A review request is addressed to the person whose token this is.
        "direct": kind == "review_request",
    }))
}

/// Reads GitHub and emits items. Pull requests that were open at the last
/// sync and are gone now are marked closed.
fn sync() -> Result<usize, String> {
    let cfg = config();
    let token = host::secret("GITHUB_TOKEN");
    let token = token.as_deref();
    let mut open: Vec<Value> = Vec::new();

    for repo in &cfg.repos {
        let url = format!("{}/repos/{repo}/pulls?state=open&sort=updated&direction=desc&per_page=50", cfg.api);
        for pr in get(&url, token)?.as_array().into_iter().flatten() {
            open.extend(item(repo, pr, "pull_request"));
        }
    }
    if cfg.review_requests && token.is_some() {
        let url = format!("{}/search/issues?q=is:open+is:pr+review-requested:@me&per_page=50", cfg.api);
        for pr in get(&url, token)?["items"].as_array().into_iter().flatten() {
            let Some(repo) = pr["repository_url"].as_str().and_then(repo_of) else { continue };
            let Some(found) = item(&repo, pr, "review_request") else { continue };
            // The same pull request may already be listed through its repository.
            open.retain(|other| other["external_id"] != found["external_id"]);
            open.push(found);
        }
    }

    for found in &open {
        host::emit_item(&found.to_string())?;
    }
    let before: Vec<Value> = host::kv_get("open").and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    for mut gone in before {
        if !open.iter().any(|now| now["external_id"] == gone["external_id"]) {
            gone["state"] = json!("closed");
            host::emit_item(&gone.to_string())?;
        }
    }
    // Bodies are not needed to close an item later.
    let slim: Vec<Value> = open
        .iter()
        .map(|i| {
            let mut i = i.clone();
            i["body"] = json!("");
            i
        })
        .collect();
    host::kv_set("open", &json!(slim).to_string())?;
    host::log("info", &format!("synced {} open pull requests", open.len()));
    Ok(open.len())
}

fn pulls_panel(note: Option<String>) -> Result<String, String> {
    let cfg = config();
    let has_token = host::secret("GITHUB_TOKEN").is_some();
    let items: Vec<Value> = serde_json::from_str(&host::query_items("{\"limit\":200}")?).map_err(|e| e.to_string())?;
    let open: Vec<&Value> = items.iter().filter(|i| i["state"] != "closed").collect();

    let mut children = vec![json!({ "type": "row", "children": [
        { "type": "text", "style": "title", "text": "Pull requests" },
        { "type": "badge", "text": format!("{} open", open.len()) },
        { "type": "button", "label": "Refresh", "action": "refresh" },
    ] })];
    if let Some(note) = note {
        children.push(json!({ "type": "text", "style": "error", "text": note }));
    }
    if cfg.repos.is_empty() && !has_token {
        children.push(json!({ "type": "empty", "text":
            "Nothing to watch yet. List repositories under [plugins.\"net.ema.github\"] in workspace.toml (repos = [\"owner/name\"]), or store a token with `almighty plugin secret net.ema.github GITHUB_TOKEN` to see the pull requests waiting for your review." }));
    } else if open.is_empty() {
        children.push(json!({ "type": "empty", "text": "No open pull requests." }));
    }

    let mut repos: Vec<&str> = open.iter().filter_map(|i| i["thread"].as_str()).collect();
    repos.sort_unstable();
    repos.dedup();
    for repo in repos {
        let number_of = |i: &Value| -> u64 {
            i["external_id"].as_str().and_then(|e| e.rsplit_once('#')).and_then(|(_, n)| n.parse().ok()).unwrap_or(0)
        };
        let mut in_repo: Vec<&Value> = open.iter().copied().filter(|i| i["thread"] == repo).collect();
        // Review requests first, then newest.
        in_repo.sort_by_key(|i| (i["kind"] != "review_request", std::cmp::Reverse(number_of(i))));
        let rows: Vec<Value> = in_repo
            .into_iter()
            .map(|i| {
                let number = number_of(i);
                let mut badges = vec![
                    json!({ "type": "badge", "text": format!("#{number}") }),
                    json!({ "type": "text", "style": "muted", "text": i["actors"][0].as_str().unwrap_or("") }),
                ];
                if i["state"] == "draft" {
                    badges.push(json!({ "type": "badge", "text": "draft", "tone": "muted" }));
                }
                if i["kind"] == "review_request" {
                    badges.push(json!({ "type": "badge", "text": "review requested", "tone": "accent" }));
                }
                json!({ "type": "stack", "children": [
                    { "type": "link", "text": i["title"], "url": i["url"] },
                    { "type": "row", "children": badges },
                ] })
            })
            .collect();
        children.push(json!({ "type": "card", "title": repo, "children": [{ "type": "list", "items": rows }] }));
    }
    Ok(json!({ "type": "stack", "children": children }).to_string())
}

impl Guest for Github {
    fn poll() -> Result<(), String> {
        sync().map(|_| ())
    }

    fn render(panel: String) -> Result<String, String> {
        match panel.as_str() {
            "pulls" => pulls_panel(None),
            other => Err(format!("no panel {other}")),
        }
    }

    fn action(panel: String, action: String, _payload: String) -> Result<String, String> {
        match (panel.as_str(), action.as_str()) {
            // A failed sync still shows what is stored, with the reason on top.
            ("pulls", "refresh") => pulls_panel(sync().err()),
            _ => Err(format!("no action {action} in panel {panel}")),
        }
    }

    fn command(id: String, _args: String) -> Result<String, String> {
        match id.as_str() {
            "github.sync" => Ok(json!({ "open": sync()? }).to_string()),
            other => Err(format!("no command {other}")),
        }
    }
}

export!(Github);
