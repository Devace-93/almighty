use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
    pub cols: u16,
    pub rows: u16,
    pub exited: bool,
    pub locked: bool,
    pub agents: usize,
    #[serde(default)]
    pub exit_code: Option<u32>,
    /// `user:<name>` or `agent:<session id>`.
    #[serde(default)]
    pub created_by: String,
    /// What the signed-in user may do: `read` or `write`.
    #[serde(default)]
    pub access: String,
    /// Still running, but taken out of the main list.
    #[serde(default)]
    pub background: bool,
}

/// Sessions in display order with their depth: a terminal opened by the
/// agent of another session is listed under that session.
pub fn session_tree(sessions: &[SessionInfo]) -> Vec<(SessionInfo, usize)> {
    fn add(parent: Option<&str>, depth: usize, all: &[SessionInfo], out: &mut Vec<(SessionInfo, usize)>) {
        for s in all {
            let owner = s.created_by.strip_prefix("agent:").filter(|p| all.iter().any(|o| o.id == *p && o.id != s.id));
            if owner == parent {
                out.push((s.clone(), depth));
                // Depth is capped so a malformed cycle cannot recurse forever.
                if depth < 8 {
                    add(Some(&s.id), depth + 1, all, out);
                }
            }
        }
    }
    let mut out = Vec::with_capacity(sessions.len());
    add(None, 0, sessions, &mut out);
    out
}

/// Exit status as people expect to read it: small codes in decimal, Windows
/// NTSTATUS-style codes in hex.
pub fn exit_label(code: Option<u32>) -> String {
    match code {
        None => "exited".to_string(),
        Some(c) if c >= 0x8000_0000 => format!("exited 0x{c:08X}"),
        Some(c) => format!("exited {c}"),
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AgentNode {
    pub agent_id: String,
    pub agent_type: String,
    pub parent: Option<String>,
    pub description: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TranscriptLine {
    pub role: String,
    pub kind: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct Run {
    pub t: String,
    #[serde(default)]
    pub fg: Option<String>,
    #[serde(default)]
    pub bg: Option<String>,
    #[serde(default)]
    pub bold: bool,
    #[serde(default)]
    pub italic: bool,
    #[serde(default)]
    pub underline: bool,
    #[serde(default)]
    pub inverse: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct Grid {
    pub rows: u16,
    pub cols: u16,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub cursor_visible: bool,
    #[serde(default)]
    pub title: String,
    pub exited: bool,
    #[serde(default)]
    pub locked: bool,
    #[serde(default)]
    pub exit_code: Option<u32>,
    /// Lines scrolled up into the history (0 = live).
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub scrollback_len: usize,
    #[serde(default)]
    pub lines: Vec<Vec<Run>>,
}

const BASE16: [&str; 16] = [
    "#1d1f21", "#cc6666", "#b5bd68", "#f0c674", "#81a2be", "#b294bb", "#8abeb7", "#c5c8c6",
    "#666a6d", "#e08080", "#c9d17c", "#f5d48a", "#99b6d0", "#c6aacd", "#a0d0c9", "#ffffff",
];

/// Server colour (`i<index>` or `#rrggbb`) -> CSS colour.
pub fn css_color(c: &str) -> String {
    let Some(idx) = c.strip_prefix('i').and_then(|n| n.parse::<u8>().ok()) else {
        return c.to_string();
    };
    match idx {
        0..=15 => BASE16[idx as usize].to_string(),
        16..=231 => {
            let n = idx - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + 40 * v as u16 };
            format!("rgb({},{},{})", level(n / 36), level((n / 6) % 6), level(n % 6))
        }
        _ => {
            let g = 8 + 10 * (idx as u16 - 232);
            format!("rgb({g},{g},{g})")
        }
    }
}

impl Run {
    pub fn css(&self) -> String {
        let (mut fg, mut bg) = (self.fg.as_deref().map(css_color), self.bg.as_deref().map(css_color));
        if self.inverse {
            std::mem::swap(&mut fg, &mut bg);
            fg.get_or_insert_with(|| "var(--term-bg)".into());
            bg.get_or_insert_with(|| "var(--term-fg)".into());
        }
        let mut s = String::new();
        if let Some(c) = fg {
            s.push_str(&format!("color:{c};"));
        }
        if let Some(c) = bg {
            s.push_str(&format!("background:{c};"));
        }
        if self.bold {
            s.push_str("font-weight:700;");
        }
        if self.italic {
            s.push_str("font-style:italic;");
        }
        if self.underline {
            s.push_str("text-decoration:underline;");
        }
        s
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    Text(String),
    /// Text that already carries a scheme.
    Url(String),
    /// A bare host (optionally with a path) such as `google.com/maps`: only a
    /// link if the host turns out to answer. `host` is the part to probe.
    Host { text: String, host: String },
}

/// File extensions that look like top-level domains (`main.rs`, `notes.md`).
/// A two-label token ending in one of these is treated as a file name.
const FILE_EXTENSIONS: &[&str] = &[
    "rs", "md", "js", "ts", "tsx", "jsx", "py", "json", "toml", "yaml", "yml", "txt", "lock", "html", "css", "sh",
    "png", "jpg", "jpeg", "gif", "svg", "log", "so", "go", "c", "h", "cc", "cpp", "hpp", "java", "rb", "php", "sql",
    "xml", "zip", "gz", "tar", "pdf", "conf", "cfg", "ini", "env", "bak", "tmp", "out", "bin", "exe", "dll", "wasm",
    "map", "mjs", "cjs", "vue", "kt", "swift", "zig", "nix", "db", "csv", "mp4", "mp3", "wav", "webm", "ico", "ttf",
    "woff", "pem", "key", "crt", "pub", "sum", "mod", "diff", "patch", "orig", "swp", "pid", "sock", "service",
    "desktop", "deb", "rpm", "apk", "ipa", "iso", "img", "vim", "lua", "el", "pl", "bat", "cmd", "dart", "ex", "exs",
    "erl", "hs", "ml", "scala", "clj", "plist", "gradle", "properties", "tf", "proto", "graphql", "snap", "test",
    "spec", "rlib", "pdb", "example", "sample", "local", "internal", "lan",
];

/// Returns the host part if `token` looks like `host.tld` or `host.tld/path`.
fn host_candidate(token: &str) -> Option<String> {
    if token.contains('@') {
        return None; // e-mail addresses
    }
    let (host, path) = match token.split_once('/') {
        Some((h, p)) => (h, Some(p)),
        None => (token, None),
    };
    let host = host.to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    let valid = labels.len() >= 2
        && host.len() <= 253
        && labels.iter().all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
    let tld = labels.last()?;
    if !valid || tld.len() < 2 || !tld.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    // `main.rs` is a file; `docs.rs/serde` and `www.example.rs` are hosts.
    if labels.len() == 2 && path.is_none() && FILE_EXTENSIONS.contains(tld) {
        return None;
    }
    Some(host)
}

fn classify(token: &str) -> Piece {
    if token.starts_with("https://") || token.starts_with("http://") {
        return Piece::Url(token.to_string());
    }
    match host_candidate(token) {
        Some(host) => Piece::Host { text: token.to_string(), host },
        None => Piece::Text(token.to_string()),
    }
}

/// Splits terminal text into plain text, URLs and bare-host candidates.
pub fn split_links(text: &str) -> Vec<Piece> {
    fn push_text(out: &mut Vec<Piece>, t: &str) {
        if t.is_empty() {
            return;
        }
        match out.last_mut() {
            Some(Piece::Text(prev)) => prev.push_str(t),
            _ => out.push(Piece::Text(t.to_string())),
        }
    }
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let ws = rest.len() - rest.trim_start().len();
        push_text(&mut out, &rest[..ws]);
        rest = &rest[ws..];
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let token = &rest[..end];
        rest = &rest[end..];
        // Punctuation around a link is not part of it: (google.com), "x.io".
        let core = token.trim_start_matches(['(', '[', '<', '"', '\'', '`']);
        let lead = token.len() - core.len();
        let core = core.trim_end_matches(['.', ',', ';', ':', ')', ']', '>', '"', '\'', '`', '!', '?']);
        push_text(&mut out, &token[..lead]);
        match classify(core) {
            Piece::Text(t) => push_text(&mut out, &t),
            link => out.push(link),
        }
        push_text(&mut out, &token[lead + core.len()..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(text: &str, host: &str) -> Piece {
        Piece::Host { text: text.into(), host: host.into() }
    }

    #[test]
    fn urls_and_hosts() {
        assert_eq!(
            split_links("see https://example.com/a, and google.com."),
            vec![
                Piece::Text("see ".into()),
                Piece::Url("https://example.com/a".into()),
                Piece::Text(", and ".into()),
                host("google.com", "google.com"),
                Piece::Text(".".into()),
            ]
        );
        assert_eq!(split_links("(Docs.RS/serde)")[1], host("Docs.RS/serde", "docs.rs"));
    }

    #[test]
    fn tree_and_exit_labels() {
        let s = |id: &str, by: &str| SessionInfo {
            id: id.into(), title: id.into(), cols: 80, rows: 24, exited: false, locked: false, agents: 0,
            exit_code: None, created_by: by.into(), access: "write".into(), background: false,
        };
        let all = vec![s("s1", "user:ana"), s("s2", "agent:s1"), s("s3", "user:ana"), s("s4", "agent:s2"), s("s5", "agent:s9")];
        let order: Vec<(String, usize)> = session_tree(&all).into_iter().map(|(s, d)| (s.id, d)).collect();
        // s5's parent is gone, so it is shown at the top level.
        assert_eq!(order, vec![("s1".into(), 0), ("s2".into(), 1), ("s4".into(), 2), ("s3".into(), 0), ("s5".into(), 0)]);
        assert_eq!(exit_label(Some(0)), "exited 0");
        assert_eq!(exit_label(Some(3221225786)), "exited 0xC000013A");
        assert_eq!(exit_label(None), "exited");
    }

    #[test]
    fn not_links() {
        for t in ["main.rs", "README.md", "bash-5.2$", "0.7.10", "a@b.com", "foo_bar.com", "1.2.3.4", "e.g", "Cargo.toml", "..."] {
            assert_eq!(split_links(t), vec![Piece::Text(t.into())], "{t}");
        }
        assert_eq!(split_links("  two  words "), vec![Piece::Text("  two  words ".into())]);
    }
}
