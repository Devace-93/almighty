//! tmux adapter: panes of the tmux server running next to almighty are
//! opened as sessions without restarting what runs in them.
//!
//! An adopted pane is a session that runs `almighty tmux-pane <id>`: a small
//! bridge that talks to tmux in control mode, shows that pane's output and
//! types into it. Ending the session only detaches the bridge; the pane and
//! its process stay.

use std::process::Command;

use serde::Serialize;

const FORMAT: &str = "#{pane_id}\t#{session_name}\t#{window_index}\t#{pane_index}\t#{pane_current_command}\t#{pane_width}\t#{pane_height}\t#{window_panes}\t#{pane_current_path}\t#{window_name}";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Pane {
    /// tmux's own id of the pane, e.g. `%3`. It does not change while the pane lives.
    pub id: String,
    pub session: String,
    pub window: u32,
    /// Position of the pane inside its window.
    pub index: u32,
    pub window_name: String,
    /// The program running in the pane.
    pub command: String,
    pub cols: u16,
    pub rows: u16,
    /// How many panes the window has.
    pub panes: u32,
    pub path: String,
}

/// `tmux`, on another socket when `ALMIGHTY_TMUX_SOCKET` names one (`tmux -L`).
pub fn base() -> Vec<String> {
    let mut cmd = vec!["tmux".to_string()];
    if let Some(socket) = std::env::var("ALMIGHTY_TMUX_SOCKET").ok().filter(|s| !s.is_empty()) {
        cmd.extend(["-L".to_string(), socket]);
    }
    cmd
}

pub fn parse(output: &str) -> Vec<Pane> {
    output
        .lines()
        .filter_map(|line| {
            let mut f = line.splitn(10, '\t');
            Some(Pane {
                id: f.next().filter(|id| is_pane_id(id))?.to_string(),
                session: f.next()?.to_string(),
                window: f.next()?.parse().ok()?,
                index: f.next()?.parse().ok()?,
                command: f.next()?.to_string(),
                cols: f.next()?.parse().ok()?,
                rows: f.next()?.parse().ok()?,
                panes: f.next()?.parse().ok()?,
                path: f.next()?.to_string(),
                window_name: f.next()?.to_string(),
            })
        })
        .collect()
}

/// `%` and digits: the only shape of a pane id.
pub fn is_pane_id(id: &str) -> bool {
    id.strip_prefix('%').is_some_and(|n| !n.is_empty() && n.len() <= 9 && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The panes of the tmux server. No tmux server running is an empty list;
/// tmux missing from the machine is an error the client can show.
pub fn list() -> Result<Vec<Pane>, String> {
    let base = base();
    let output = Command::new(&base[0])
        .args(&base[1..])
        .args(["list-panes", "-a", "-F", FORMAT])
        .env_remove("TMUX")
        .output()
        .map_err(|_| "tmux is not installed on the server's machine".to_string())?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    Ok(parse(&String::from_utf8_lossy(&output.stdout)))
}

/// The pane a person named: its id (`%3`), `session:window.pane`, or
/// `session:window` when that window has a single pane.
pub fn find<'a>(panes: &'a [Pane], target: &str) -> Option<&'a Pane> {
    if let Some(pane) = panes.iter().find(|p| p.id == target) {
        return Some(pane);
    }
    let (session, rest) = target.rsplit_once(':')?;
    let (window, index) = match rest.split_once('.') {
        Some((w, i)) => (w.parse::<u32>().ok()?, Some(i.parse::<u32>().ok()?)),
        None => (rest.parse::<u32>().ok()?, None),
    };
    let mut inside = panes.iter().filter(|p| p.session == session && p.window == window);
    match index {
        Some(index) => inside.find(|p| p.index == index),
        None => inside.next().filter(|p| p.panes == 1),
    }
}

/// The running almighty program, to start the bridge with.
pub fn exe() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // Replaced on disk while running (an upgrade): Linux still reaches the
    // running program through /proc.
    if !exe.exists() && std::path::Path::new("/proc/self/exe").exists() {
        return Ok("/proc/self/exe".into());
    }
    Ok(exe.to_string_lossy().into_owned())
}

/// The command of the session that shows `pane`: `exe` is almighty itself.
pub fn adopt_command(exe: &str, pane: &Pane) -> Vec<String> {
    vec![exe.to_string(), "tmux-pane".to_string(), pane.id.clone()]
}

pub fn title(pane: &Pane) -> String {
    if pane.panes > 1 {
        format!("tmux {}:{}.{} {}", pane.session, pane.window, pane.index, pane.command)
    } else {
        format!("tmux {}:{} {}", pane.session, pane.window, pane.window_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUT: &str = "%0\twork\t1\t1\tnvim\t120\t40\t2\t/home/ana/src\teditor\n\
                       %1\twork\t1\t2\tbash\t120\t10\t2\t/home/ana/src\teditor\n\
                       %5\twork\t2\t1\tbash\t80\t24\t1\t/home/ana\tname with\ttab\n\
                       $1\twork\t2\t1\tbash\t80\t24\t1\t/\tnot a pane id\n\
                       broken line\n";

    #[test]
    fn parses_panes() {
        let panes = parse(OUT);
        assert_eq!(panes.len(), 3);
        assert_eq!(panes[0], Pane {
            id: "%0".into(), session: "work".into(), window: 1, index: 1, window_name: "editor".into(),
            command: "nvim".into(), cols: 120, rows: 40, panes: 2, path: "/home/ana/src".into(),
        });
        assert_eq!(panes[2].window_name, "name with\ttab");
        assert_eq!(title(&panes[1]), "tmux work:1.2 bash");
        assert_eq!(title(&panes[2]), "tmux work:2 name with\ttab");
    }

    #[test]
    fn finds_a_pane_by_id_or_position() {
        let panes = parse(OUT);
        assert_eq!(find(&panes, "%1").unwrap().id, "%1");
        assert_eq!(find(&panes, "work:1.2").unwrap().id, "%1");
        assert_eq!(find(&panes, "work:2").unwrap().id, "%5");
        // A window with several panes needs the pane named.
        assert!(find(&panes, "work:1").is_none());
        assert!(find(&panes, "work:9.1").is_none());
        assert!(find(&panes, "%1; kill-server").is_none());
        assert!(!is_pane_id("%"));
        assert!(!is_pane_id("%1 "));
    }

    #[test]
    fn adopt_runs_the_bridge_on_the_pane() {
        let pane = parse(OUT).remove(2);
        assert_eq!(adopt_command("/usr/bin/almighty", &pane), ["/usr/bin/almighty", "tmux-pane", "%5"]);
    }
}
