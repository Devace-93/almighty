//! Triage: every item gets, per person, a bucket, a priority and a short
//! summary. Rules run first and always; an LLM configured by the operator
//! may refine the result. What a person changes by hand is kept and feeds
//! later decisions.

pub mod llm;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::plugins::db::{Item, ItemQuery};
use crate::session::now_secs;
use crate::store::{Role, Store};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS triage (
    item_id TEXT NOT NULL,
    user_id INTEGER NOT NULL,
    bucket TEXT NOT NULL,
    priority INTEGER NOT NULL,
    summary TEXT NOT NULL,
    draft TEXT NOT NULL DEFAULT '',
    reason TEXT NOT NULL,
    origin TEXT NOT NULL,              -- rules | llm | user
    done INTEGER NOT NULL DEFAULT 0,
    snoozed_until INTEGER,
    notified INTEGER NOT NULL DEFAULT 0,
    item_rev INTEGER NOT NULL,         -- revision of the item this was decided for
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (item_id, user_id)
);
CREATE TABLE IF NOT EXISTS corrections (
    id INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL,
    item_id TEXT NOT NULL,
    at INTEGER NOT NULL,
    source TEXT NOT NULL,
    kind TEXT NOT NULL,
    actor TEXT NOT NULL,
    title TEXT NOT NULL,
    bucket TEXT,                       -- where the person moved it, if they did
    priority INTEGER                   -- the priority they set, if they did
);
CREATE INDEX IF NOT EXISTS corrections_user ON corrections(user_id, id);
";

pub const BUCKETS: [&str; 5] = ["reply", "act", "prepare", "fyi", "noise"];

/// Per-person preferences, `users/<name>/triage.toml`. Everything is optional.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Prefs {
    /// Language of summaries, e.g. "es". Empty: the item's own language.
    pub input_language: String,
    /// Language of draft replies: "match" (the incoming message) or a language.
    pub output_language: String,
    /// The person's own handles on the connected services.
    pub me: Vec<String>,
    pub people: People,
    pub sources: Sources,
    pub keywords: Keywords,
    pub notify: Notify,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct People {
    pub important: Vec<String>,
    pub muted: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Sources {
    /// Sources or threads (a repository, a channel) that are never worth a look.
    pub muted: Vec<String>,
    /// Sources or threads that are only for staying informed.
    pub fyi: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Keywords {
    pub urgent: Vec<String>,
    pub noise: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Notify {
    /// Notify from this priority up (0–3). 4 turns notifications off.
    pub min_priority: u8,
    /// "22:00-07:00", in the person's local time.
    pub quiet_hours: String,
    /// Minutes east of UTC for quiet hours, e.g. -360.
    pub utc_offset_minutes: i32,
}

impl Default for Notify {
    fn default() -> Self {
        Notify { min_priority: 2, quiet_hours: String::new(), utc_offset_minutes: 0 }
    }
}

impl Prefs {
    pub fn path(data_dir: &Path, user: &str) -> PathBuf {
        data_dir.join("users").join(user.to_ascii_lowercase()).join("triage.toml")
    }

    /// A missing file means defaults; a broken one is an error the person should see.
    pub fn load(data_dir: &Path, user: &str) -> Result<Prefs, String> {
        match std::fs::read_to_string(Self::path(data_dir, user)) {
            Ok(text) => toml::from_str(&text).map_err(|e| format!("triage.toml: {e}")),
            Err(_) => Ok(Prefs::default()),
        }
    }

    fn quiet_now(&self, now: u64) -> bool {
        let Some((from, to)) = self.notify.quiet_hours.split_once('-') else { return false };
        let minutes = |t: &str| -> Option<i64> {
            let (h, m) = t.trim().split_once(':')?;
            Some(h.parse::<i64>().ok()? * 60 + m.parse::<i64>().ok()?)
        };
        let (Some(from), Some(to)) = (minutes(from), minutes(to)) else { return false };
        let local = (now as i64 / 60 + self.notify.utc_offset_minutes as i64).rem_euclid(24 * 60);
        if from <= to {
            (from..to).contains(&local)
        } else {
            local >= from || local < to
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Verdict {
    pub bucket: String,
    pub priority: u8,
    pub reason: String,
}

/// What a person taught by moving items: "things like this go there".
#[derive(Debug, Clone, PartialEq)]
pub struct Learned {
    pub source: String,
    pub kind: String,
    pub actor: String,
    pub bucket: String,
    pub times: u32,
}

fn has(list: &[String], value: &str) -> bool {
    !value.is_empty() && list.iter().any(|x| x.eq_ignore_ascii_case(value))
}

fn mentions_any(text: &str, words: &[String]) -> Option<String> {
    let text = text.to_lowercase();
    words.iter().find(|w| !w.trim().is_empty() && text.contains(&w.to_lowercase())).cloned()
}

/// The deterministic part of triage. Always available, never leaves the server.
pub fn rules(item: &Item, prefs: &Prefs, learned: &[Learned], now: u64) -> Verdict {
    let actor = item.actors.first().map(String::as_str).unwrap_or("");
    let verdict = |bucket: &str, priority: u8, reason: String| Verdict { bucket: bucket.into(), priority, reason };

    if let Some(who) = item.actors.iter().find(|a| has(&prefs.people.muted, a)) {
        return verdict("noise", 0, format!("{who} is muted"));
    }
    if has(&prefs.sources.muted, &item.source) || has(&prefs.sources.muted, &item.thread) {
        return verdict("noise", 0, "this source is muted".into());
    }

    let for_me = item.direct || item.mentions.iter().any(|m| has(&prefs.me, m));
    let mut v = if let Some(l) =
        learned.iter().find(|l| l.times >= 2 && l.source == item.source && l.kind == item.kind && l.actor.eq_ignore_ascii_case(actor))
    {
        let priority = match l.bucket.as_str() {
            "reply" | "act" | "prepare" => 2,
            "noise" => 0,
            _ => 1,
        };
        verdict(&l.bucket, priority, format!("you moved {} similar items there", l.times))
    } else if let Some(word) = mentions_any(&item.title, &prefs.keywords.noise) {
        return verdict("noise", 0, format!("matches the noise keyword \"{word}\""));
    } else if for_me && matches!(item.kind.as_str(), "review_request" | "task" | "assignment") {
        verdict("act", 2, "it asks you to do something".into())
    } else if for_me {
        verdict("reply", 2, "it is addressed to you".into())
    } else if item.due.is_some_and(|due| due > now) {
        let soon = item.due.is_some_and(|due| due < now + 24 * 3600);
        verdict("prepare", if soon { 3 } else { 2 }, "it has a date ahead".into())
    } else if has(&prefs.sources.fyi, &item.source) || has(&prefs.sources.fyi, &item.thread) {
        verdict("fyi", 1, "this source is for staying informed".into())
    } else {
        verdict("fyi", 1, "nothing asks for you by name".into())
    };

    if v.bucket != "noise" {
        if let Some(word) = mentions_any(&format!("{} {}", item.title, item.body), &prefs.keywords.urgent) {
            v.priority = (v.priority + 1).min(3);
            v.reason.push_str(&format!("; urgent keyword \"{word}\""));
        }
        if let Some(who) = item.actors.iter().find(|a| has(&prefs.people.important, a)) {
            v.priority = (v.priority + 1).min(3);
            v.reason.push_str(&format!("; {who} matters to you"));
        }
    }
    v
}

fn closed(state: &str) -> bool {
    matches!(state, "closed" | "merged" | "done" | "resolved" | "read" | "archived")
}

fn short(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &flat[..at]),
        None => flat,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub bucket: String,
    pub priority: u8,
    pub summary: String,
    pub draft: String,
    pub reason: String,
    pub origin: String,
    pub done: bool,
    pub snoozed_until: Option<u64>,
    #[serde(skip)]
    pub notified: bool,
    #[serde(skip)]
    pub item_rev: u64,
}

impl Store {
    pub fn triage_get(&self, item: &str, user: i64) -> Option<Row> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT bucket, priority, summary, draft, reason, origin, done, snoozed_until, notified, item_rev
             FROM triage WHERE item_id = ?1 AND user_id = ?2",
            params![item, user],
            |r| {
                Ok(Row {
                    bucket: r.get(0)?,
                    priority: r.get::<_, i64>(1)? as u8,
                    summary: r.get(2)?,
                    draft: r.get(3)?,
                    reason: r.get(4)?,
                    origin: r.get(5)?,
                    done: r.get::<_, i64>(6)? != 0,
                    snoozed_until: r.get::<_, Option<i64>>(7)?.map(|t| t as u64),
                    notified: r.get::<_, i64>(8)? != 0,
                    item_rev: r.get::<_, i64>(9)? as u64,
                })
            },
        )
        .optional()
        .ok()
        .flatten()
    }

    pub fn triage_put(&self, item: &str, user: i64, row: &Row) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO triage(item_id, user_id, bucket, priority, summary, draft, reason, origin, done, snoozed_until,
                 notified, item_rev, updated_at) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(item_id, user_id) DO UPDATE SET bucket = ?3, priority = ?4, summary = ?5, draft = ?6, reason = ?7,
                 origin = ?8, done = ?9, snoozed_until = ?10, notified = ?11, item_rev = ?12, updated_at = ?13",
            params![
                item,
                user,
                row.bucket,
                row.priority as i64,
                row.summary,
                row.draft,
                row.reason,
                row.origin,
                row.done as i64,
                row.snoozed_until.map(|t| t as i64),
                row.notified as i64,
                row.item_rev as i64,
                now_secs() as i64
            ],
        );
    }

    fn correction_add(&self, user: i64, item: &Item, bucket: Option<&str>, priority: Option<u8>) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO corrections(user_id, item_id, at, source, kind, actor, title, bucket, priority)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                user,
                item.id,
                now_secs() as i64,
                item.source,
                item.kind,
                item.actors.first().cloned().unwrap_or_default(),
                item.title,
                bucket,
                priority.map(|p| p as i64)
            ],
        );
    }

    /// Bucket moves grouped by what the items had in common; the latest move
    /// of each item counts once.
    pub fn learned(&self, user: i64) -> Vec<Learned> {
        let conn = self.conn.lock().unwrap();
        let sql = "SELECT source, kind, actor, bucket, COUNT(*) FROM (
                       SELECT source, kind, actor, bucket, MAX(id) FROM corrections
                       WHERE user_id = ?1 AND bucket IS NOT NULL GROUP BY item_id
                   ) GROUP BY source, kind, actor, bucket ORDER BY COUNT(*) DESC";
        let Ok(mut stmt) = conn.prepare(sql) else { return Vec::new() };
        let rows = stmt.query_map(params![user], |r| {
            Ok(Learned { source: r.get(0)?, kind: r.get(1)?, actor: r.get(2)?, bucket: r.get(3)?, times: r.get::<_, i64>(4)? as u32 })
        });
        rows.map(|it| it.flatten().collect()).unwrap_or_default()
    }

    /// Recent moves as examples for the LLM: (title, source, kind, actor, bucket).
    pub fn correction_examples(&self, user: i64, limit: u32) -> Vec<Value> {
        let conn = self.conn.lock().unwrap();
        let sql = "SELECT title, source, kind, actor, bucket FROM corrections
                   WHERE user_id = ?1 AND bucket IS NOT NULL ORDER BY id DESC LIMIT ?2";
        let Ok(mut stmt) = conn.prepare(sql) else { return Vec::new() };
        let rows = stmt.query_map(params![user, limit], |r| {
            Ok(json!({ "title": r.get::<_, String>(0)?, "source": r.get::<_, String>(1)?, "kind": r.get::<_, String>(2)?,
                       "from": r.get::<_, String>(3)?, "bucket": r.get::<_, String>(4)? }))
        });
        rows.map(|it| it.flatten().collect()).unwrap_or_default()
    }
}

/// What the triage code needs from the server.
#[derive(Clone)]
pub struct Triage {
    pub store: Arc<Store>,
    pub data_dir: PathBuf,
    pub events: broadcast::Sender<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct Change {
    pub bucket: Option<String>,
    pub priority: Option<u8>,
    pub done: Option<bool>,
    /// Unix seconds; 0 clears the snooze.
    pub snooze_until: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
pub struct InboxQuery {
    pub bucket: Option<String>,
    /// "open" (default), "done" or "all".
    pub show: Option<String>,
    pub limit: Option<u32>,
}

impl Triage {
    /// The rule-based decision for one person, stored. Keeps what the person
    /// set by hand; decides again when the item changed.
    pub fn ensure(&self, user: i64, name: &str, item: &Item) -> Row {
        let existing = self.store.triage_get(&item.id, user);
        if let Some(row) = &existing {
            if row.item_rev == item.rev {
                return row.clone();
            }
        }
        let prefs = Prefs::load(&self.data_dir, name).unwrap_or_default();
        let verdict = rules(item, &prefs, &self.store.learned(user), now_secs());
        let mut row = match existing {
            // The person's own choice survives updates of the item.
            Some(old) if old.origin == "user" => old,
            Some(old) => Row {
                // A higher priority than before may notify again.
                notified: old.notified && verdict.priority <= old.priority,
                done: old.done,
                snoozed_until: old.snoozed_until,
                ..self.fresh(item, verdict)
            },
            None => self.fresh(item, verdict),
        };
        row.item_rev = item.rev;
        if closed(&item.state) {
            row.done = true;
        }
        self.store.triage_put(&item.id, user, &row);
        row
    }

    fn fresh(&self, item: &Item, verdict: Verdict) -> Row {
        Row {
            bucket: verdict.bucket,
            priority: verdict.priority,
            summary: short(if item.body.trim().is_empty() { &item.title } else { &item.body }, 220),
            draft: String::new(),
            reason: verdict.reason,
            origin: "rules".into(),
            done: false,
            snoozed_until: None,
            notified: false,
            item_rev: item.rev,
        }
    }

    /// The person's inbox: items with their triage, most pressing first.
    pub fn inbox(&self, user: i64, name: &str, q: &InboxQuery) -> Vec<Value> {
        let now = now_secs();
        let show = q.show.as_deref().unwrap_or("open");
        let mut rows: Vec<(Item, Row)> = self
            .store
            .items(&ItemQuery { limit: Some(500), ..Default::default() })
            .into_iter()
            .map(|item| {
                let row = self.ensure(user, name, &item);
                (item, row)
            })
            .filter(|(_, row)| q.bucket.as_deref().is_none_or(|b| b == row.bucket))
            .filter(|(_, row)| match show {
                "all" => true,
                "done" => row.done,
                _ => !row.done && row.snoozed_until.is_none_or(|until| until <= now),
            })
            .collect();
        rows.sort_by(|a, b| b.1.priority.cmp(&a.1.priority).then(b.0.updated_at.cmp(&a.0.updated_at)));
        rows.truncate(q.limit.unwrap_or(200).min(500) as usize);
        rows.into_iter().map(|(item, row)| json!({ "item": item, "triage": row })).collect()
    }

    /// Open items per bucket, for the sidebar.
    pub fn counts(&self, user: i64, name: &str) -> Value {
        let mut counts = serde_json::Map::new();
        for bucket in BUCKETS {
            counts.insert(bucket.into(), json!(0));
        }
        for entry in self.inbox(user, name, &InboxQuery { limit: Some(500), ..Default::default() }) {
            if let Some(n) = entry["triage"]["bucket"].as_str().and_then(|b| counts.get_mut(b)) {
                *n = json!(n.as_u64().unwrap_or(0) + 1);
            }
        }
        Value::Object(counts)
    }

    /// A person moves, re-prioritises, finishes or snoozes an item. Moves and
    /// priorities are remembered as corrections.
    pub fn change(&self, user: i64, name: &str, item_id: &str, change: &Change) -> Result<Row, String> {
        let item = self.store.item(item_id).ok_or_else(|| format!("no item {item_id}"))?;
        let mut row = self.ensure(user, name, &item);
        if let Some(bucket) = &change.bucket {
            if !BUCKETS.contains(&bucket.as_str()) {
                return Err(format!("bucket must be one of {}", BUCKETS.join(", ")));
            }
        }
        if change.priority.is_some_and(|p| p > 3) {
            return Err("priority goes from 0 to 3".into());
        }
        let moved = change.bucket.as_ref().filter(|b| **b != row.bucket);
        let ranked = change.priority.filter(|p| *p != row.priority);
        if moved.is_some() || ranked.is_some() {
            self.store.correction_add(user, &item, moved.map(String::as_str), ranked);
            if let Some(bucket) = moved {
                row.bucket = bucket.clone();
            }
            if let Some(priority) = ranked {
                row.priority = priority;
            }
            row.origin = "user".into();
            row.reason = "you set this".into();
        }
        if let Some(done) = change.done {
            row.done = done;
        }
        if let Some(until) = change.snooze_until {
            row.snoozed_until = (until > 0).then_some(until);
        }
        self.store.triage_put(item_id, user, &row);
        self.notify_inbox(name);
        Ok(row)
    }

    fn notify_inbox(&self, name: &str) {
        let _ = self.events.send(json!({ "type": "inbox", "user": name }).to_string());
    }

    /// Triage of one item for everyone who has an inbox, with the LLM when
    /// one is configured, and the notification when it is worth one.
    /// Blocking (the LLM call): run it off the async threads.
    pub fn process(&self, item_id: &str) {
        let Some(item) = self.store.item(item_id) else { return };
        let provider = llm::Provider::load(&self.data_dir);
        for user in self.store.list_users().into_iter().filter(|u| u.role >= Role::Member) {
            let before = self.store.triage_get(&item.id, user.id);
            let mut row = self.ensure(user.id, &user.name, &item);
            let prefs = Prefs::load(&self.data_dir, &user.name).unwrap_or_default();
            let decided_now = before.is_none_or(|b| b.item_rev != item.rev);
            // Muted things are not sent anywhere; the person's own choices are final.
            if decided_now && row.origin == "rules" && row.priority > 0 && !closed(&item.state) {
                if let Some(provider) = &provider {
                    let examples = self.store.correction_examples(user.id, 12);
                    match provider.triage(&item, &row, &prefs, &examples) {
                        Ok(refined) => {
                            row = Row { origin: "llm".into(), ..refined };
                            self.store.triage_put(&item.id, user.id, &row);
                        }
                        Err(e) => eprintln!("triage provider failed for {}: {e}", item.id),
                    }
                }
            }
            self.notify_inbox(&user.name);
            let worth = !row.done
                && !row.notified
                && matches!(row.bucket.as_str(), "reply" | "act" | "prepare")
                && row.priority >= prefs.notify.min_priority
                && !prefs.quiet_now(now_secs());
            if worth {
                row.notified = true;
                self.store.triage_put(&item.id, user.id, &row);
                // The light summary only; the client fetches the rest if opened.
                let _ = self.events.send(
                    json!({ "type": "notify", "user": user.name, "item": item.id, "title": item.title,
                            "body": row.summary, "bucket": row.bucket, "priority": row.priority })
                    .to_string(),
                );
            }
        }
    }
}

/// Runs triage whenever a connector emits or updates an item.
pub fn spawn_worker(triage: Triage) {
    let mut events = triage.events.subscribe();
    tokio::spawn(async move {
        loop {
            let text = match events.recv().await {
                Ok(text) => text,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            };
            let Ok(event) = serde_json::from_str::<Value>(&text) else { continue };
            if event["type"] != "item" {
                continue;
            }
            let Some(id) = event["id"].as_str().map(str::to_string) else { continue };
            let triage = triage.clone();
            let _ = tokio::task::spawn_blocking(move || triage.process(&id)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(kind: &str, actor: &str) -> Item {
        Item {
            id: "it_1".into(),
            plugin: "p".into(),
            external_id: "x".into(),
            source: "chat".into(),
            kind: kind.into(),
            state: "open".into(),
            title: "Lunch on Friday?".into(),
            body: String::new(),
            url: String::new(),
            thread: "general".into(),
            actors: vec![actor.into()],
            links: vec![],
            received_at: 0,
            updated_at: 1,
            direct: false,
            mentions: vec![],
            due: None,
            rev: 0,
        }
    }

    #[test]
    fn rules_cover_the_buckets() {
        let mut prefs = Prefs { me: vec!["Ana".into()], ..Default::default() };
        let v = |i: &Item, p: &Prefs| rules(i, p, &[], 1000);
        assert_eq!(v(&item("message", "bo"), &prefs).bucket, "fyi");

        let mut direct = item("message", "bo");
        direct.direct = true;
        assert_eq!((v(&direct, &prefs).bucket.as_str(), v(&direct, &prefs).priority), ("reply", 2));

        let mut mention = item("review_request", "bo");
        mention.mentions = vec!["ana".into()];
        assert_eq!(v(&mention, &prefs).bucket, "act");

        let mut event = item("event", "cal");
        event.due = Some(1000 + 3600);
        assert_eq!((v(&event, &prefs).bucket.as_str(), v(&event, &prefs).priority), ("prepare", 3));
        event.due = Some(500);
        assert_eq!(v(&event, &prefs).bucket, "fyi", "a date in the past asks for nothing");

        prefs.people.important = vec!["bo".into()];
        prefs.keywords.urgent = vec!["friday".into()];
        assert_eq!(v(&direct, &prefs).priority, 3, "capped at 3");
        prefs.keywords.noise = vec!["lunch".into()];
        assert_eq!(v(&item("message", "bo"), &prefs).bucket, "noise");
        prefs.people.muted = vec!["BO".into()];
        assert_eq!((v(&direct, &prefs).bucket.as_str(), v(&direct, &prefs).priority), ("noise", 0), "muted wins");
        prefs.people.muted.clear();
        prefs.sources.muted = vec!["general".into()];
        assert_eq!(v(&direct, &prefs).bucket, "noise", "a muted thread");
    }

    #[test]
    fn learned_moves_apply_after_two() {
        let learned = |times| vec![Learned { source: "chat".into(), kind: "message".into(), actor: "bo".into(), bucket: "noise".into(), times }];
        let prefs = Prefs::default();
        assert_eq!(rules(&item("message", "bo"), &prefs, &learned(1), 0).bucket, "fyi");
        assert_eq!(rules(&item("message", "Bo"), &prefs, &learned(2), 0).bucket, "noise");
        assert_eq!(rules(&item("message", "cy"), &prefs, &learned(2), 0).bucket, "fyi");
    }

    #[test]
    fn quiet_hours_wrap_midnight() {
        let mut prefs = Prefs::default();
        prefs.notify.quiet_hours = "22:00-07:00".into();
        prefs.notify.utc_offset_minutes = -360;
        let at = |h: u64, m: u64| (h * 60 + m) * 60; // UTC time of day on day 0
        assert!(prefs.quiet_now(at(5, 0)), "05:00 UTC is 23:00 local");
        assert!(prefs.quiet_now(at(12, 59)), "06:59 local");
        assert!(!prefs.quiet_now(at(13, 0)), "07:00 local");
        assert!(!prefs.quiet_now(at(20, 0)), "14:00 local");
        prefs.notify.quiet_hours = "nonsense".into();
        assert!(!prefs.quiet_now(at(5, 0)));
    }
}
