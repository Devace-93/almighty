//! The ACP client against the built-in stand-in agent.

use std::process::{Command, Stdio};

fn run(permissions: &str) -> (String, String) {
    let exe = env!("CARGO_BIN_EXE_almighty");
    let out = Command::new(exe)
        .args(["acp", "--prompt", "hello there", "--permissions", permissions, "--", exe, "acp-echo-agent"])
        .env_remove("ALMIGHTY_SESSION")
        .env_remove("ALMIGHTY_TOKEN")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    (String::from_utf8_lossy(&out.stdout).to_string(), String::from_utf8_lossy(&out.stderr).to_string())
}

#[test]
fn a_prompt_turn_with_plan_tool_call_and_permission() {
    let (out, _) = run("allow");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines,
        [
            "plan:",
            "  [completed] Read the prompt",
            "tool: Write notes.txt (pending)",
            "permission: Write notes.txt — allowed once",
            "tool: Write notes.txt (completed)",
            // The agent asked for a file; this client offers none and says so.
            "You said: hello there (no file access)",
        ],
        "{out}"
    );
}

#[test]
fn permission_is_refused_when_told_to_or_when_nobody_can_be_asked() {
    let (out, _) = run("deny");
    assert!(out.contains("permission: Write notes.txt — refused") && out.contains("tool: Write notes.txt (failed)"), "{out}");
    // "ask" without a keyboard must not hang and must not allow.
    let (out, err) = run("ask");
    assert!(out.contains("refused") && out.contains("(failed)"), "{out}");
    assert!(err.contains("nobody can be asked"), "{err}");
}

#[test]
fn a_program_that_is_not_an_agent_fails_clearly() {
    let exe = env!("CARGO_BIN_EXE_almighty");
    let out = Command::new(exe).args(["acp", "--prompt", "x", "--", exe, "--version"]).stdin(Stdio::null()).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("closed its output"));
}
