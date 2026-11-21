//! Tokens saved by `almighty login`, one per server URL.
//!
//! Stored in a file only the user can read. The OS keychain replaces this in
//! a later milestone.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};

fn path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("almighty").join("credentials.json"))
}

fn read() -> BTreeMap<String, String> {
    path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write(map: &BTreeMap<String, String>) -> Result<()> {
    let path = path().context("no configuration directory on this system")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let data = serde_json::to_string_pretty(map)?;
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        // Created private from the start, never world-readable in between.
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&path)?;
        f.write_all(data.as_bytes())?;
    }
    #[cfg(not(unix))]
    std::fs::write(&path, data)?;
    Ok(())
}

pub fn load(url: &str) -> Option<String> {
    read().get(url).cloned()
}

pub fn save(url: &str, token: &str) -> Result<()> {
    let mut map = read();
    map.insert(url.to_string(), token.to_string());
    write(&map)
}

pub fn forget(url: &str) -> Result<()> {
    let mut map = read();
    if map.remove(url).is_some() {
        write(&map)?;
    }
    Ok(())
}
