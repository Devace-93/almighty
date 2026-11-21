//! tmux adapter: windows of the tmux server running next to almighty are
//! opened as sessions without restarting what runs in them.
//!
//! An adopted window is a tmux client inside an almighty session. The client
//! joins the window's tmux session through a grouped session of its own, so
//! it can show one window while the person's own tmux client shows another.
//! The grouped session goes away when the almighty session ends; the windows
//! and their processes stay.

use std::process::Command;

use serde::Serialize;

/// Grouped sessions made by `adopt_command`; never listed.
const PREFIX: &str = "almighty-";

const FORMAT: &str = "#{session_id}\t#{session_name}\t#{window_index}\t#{pane_current_command}\t#{window_panes}\t#{pane_current_path}\t#{window_name}";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Window {
    /// tmux's own id of the session, e.g. `$3`.
    pub session_id: String,
    pub session: String,
    pub index: u32,
    pub name: String,
    /// The program in the window's active pane.
    pub command: String,
    pub panes: u32,
    pub path: String,
}

/// `tmux`, on another socket when `ALMIGHTY_TMUX_SOCKET` names one (`tmux -L`).
fn base() -> Vec<String> {
    let mut cmd = vec!["tmux".to_string()];
    if let Some(socket) = std::env::var("ALMIGHTY_TMUX_SOCKET").ok().filter(|s| !s.is_empty()) {
        cmd.extend(["-L".to_string(), socket]);
    }
    cmd
}

pub fn parse(output: &str) -> Vec<Window> {
    output
        .lines()
        .filter_map(|line| {
            let mut f = line.splitn(7, '\t');
            let window = Window {
                session_id: f.next()?.to_string(),
                session: f.next()?.to_string(),
                index: f.next()?.parse().ok()?,
                command: f.next()?.to_string(),
                panes: f.next()?.parse().ok()?,
                path: f.next()?.to_string(),
                name: f.next()?.to_string(),
            };
            (!window.session.starts_with(PREFIX)).then_some(window)
        })
        .collect()
}

/// The windows of the tmux server. No tmux server running is an empty list;
/// tmux missing from the machine is an error the client can show.
pub fn list() -> Result<Vec<Window>, String> {
    let base = base();
    let output = Command::new(&base[0])
        .args(&base[1..])
        .args(["list-windows", "-a", "-F", FORMAT])
        .env_remove("TMUX")
        .output()
        .map_err(|_| "tmux is not installed on the server's machine".to_string())?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    Ok(parse(&String::from_utf8_lossy(&output.stdout)))
}

/// The command of the session that shows `window`. `unique` tells grouped
/// sessions apart; letters and digits only.
pub fn adopt_command(window: &Window, unique: &str) -> Vec<String> {
    let group = format!("{PREFIX}{unique}");
    // `env -u TMUX`: the server itself may have been started inside tmux,
    // and tmux refuses to start a client from inside another.
    let mut cmd: Vec<String> = ["env", "-u", "TMUX"].map(String::from).to_vec();
    cmd.extend(base());
    cmd.extend(["new-session", "-t", &window.session_id, "-s", &group].map(String::from));
    cmd.extend([";", "set-option", "destroy-unattached", "on"].map(String::from));
    cmd.extend([";".to_string(), "select-window".to_string(), "-t".to_string(), format!("{group}:{}", window.index)]);
    cmd
}

pub fn title(window: &Window) -> String {
    format!("tmux {}:{} {}", window.session, window.index, window.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_windows_and_hides_its_own_groups() {
        let out = "$0\twork\t1\tnvim\t2\t/home/ana/src\teditor\n\
                   $0\twork\t2\tbash\t1\t/home/ana\tname with\ttab\n\
                   $4\talmighty-ab12\t1\tnvim\t2\t/home/ana/src\teditor\n\
                   broken line\n";
        let windows = parse(out);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0], Window {
            session_id: "$0".into(), session: "work".into(), index: 1, name: "editor".into(),
            command: "nvim".into(), panes: 2, path: "/home/ana/src".into(),
        });
        assert_eq!(windows[1].name, "name with\ttab");
        assert_eq!(title(&windows[0]), "tmux work:1 editor");
    }

    #[test]
    fn adopt_joins_the_session_and_selects_the_window() {
        let window = parse("$3\twork\t2\tbash\t1\t/\tlogs\n").remove(0);
        let cmd = adopt_command(&window, "ab12");
        let tail: Vec<&str> = cmd.iter().skip_while(|a| *a != "new-session").map(String::as_str).collect();
        assert_eq!(tail, [
            "new-session", "-t", "$3", "-s", "almighty-ab12", ";", "set-option", "destroy-unattached", "on", ";",
            "select-window", "-t", "almighty-ab12:2",
        ]);
        assert_eq!(&cmd[..3], ["env", "-u", "TMUX"]);
    }
}
