/// POSIX-quote one argument for safe insertion into a shell command line.
pub fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,@%+".contains(c));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Join an argv into a single shell command string with each arg quoted.
pub fn shell_join(argv: &[String]) -> String {
    argv.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ")
}

use anyhow::{bail, Result};
use std::process::Command;

pub fn is_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn in_tmux() -> bool {
    std::env::var("TMUX").map(|v| !v.is_empty()).unwrap_or(false)
}

pub fn has_session(name: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn new_session_detached(name: &str, cwd: &str) -> Result<()> {
    let status = Command::new("tmux")
        .args(["new-session", "-d", "-s", name, "-c", cwd])
        .status()?;
    if !status.success() {
        bail!("failed to create tmux session '{name}'");
    }
    Ok(())
}

pub fn send_command(name: &str, shell_cmd: &str) -> Result<()> {
    Command::new("tmux")
        .args(["send-keys", "-t", name, "-l", shell_cmd])
        .status()?;
    Command::new("tmux")
        .args(["send-keys", "-t", name, "Enter"])
        .status()?;
    Ok(())
}

/// Send a bare Enter keypress to a session's active pane.
pub fn send_enter(name: &str) -> Result<()> {
    Command::new("tmux")
        .args(["send-keys", "-t", name, "Enter"])
        .status()?;
    Ok(())
}

/// Names of all sessions; empty vec if no server is running.
pub fn list_session_names() -> Result<Vec<String>> {
    let out = Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()?;
    if !out.status.success() {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|s| s.to_string())
        .collect())
}

/// Get the current working directory of a tmux session's active pane.
pub fn session_cwd(name: &str) -> Result<String> {
    let out = Command::new("tmux")
        .args(["display-message", "-p", "-t", name, "#{pane_current_path}"])
        .output()?;
    if !out.status.success() {
        bail!("failed to get cwd for tmux session '{name}'");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn kill_session(name: &str) -> Result<()> {
    let status = Command::new("tmux")
        .args(["kill-session", "-t", name])
        .status()?;
    if !status.success() {
        bail!("failed to kill tmux session '{name}'");
    }
    Ok(())
}

/// Attach (outside tmux, via exec) or switch-client (inside tmux).
pub fn attach_or_switch(name: &str) -> Result<()> {
    if in_tmux() {
        Command::new("tmux")
            .args(["switch-client", "-t", name])
            .status()?;
        Ok(())
    } else {
        use std::os::unix::process::CommandExt;
        let err = Command::new("tmux")
            .args(["attach-session", "-t", name])
            .exec();
        bail!("failed to exec tmux attach-session: {err}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_args_unquoted() {
        assert_eq!(shell_join(&["claude".into(), "--yolo".into()]), "claude --yolo");
    }

    #[test]
    fn spaces_and_quotes_are_escaped() {
        let joined = shell_join(&["echo".into(), "a b".into()]);
        assert_eq!(joined, "echo 'a b'");
        let joined = shell_join(&["echo".into(), "it's".into()]);
        assert_eq!(joined, r#"echo 'it'\''s'"#);
    }

    #[test]
    fn create_detect_and_kill_session() {
        if !is_available() {
            eprintln!("skipping: tmux not installed");
            return;
        }
        let name = format!("amuxtest_{}", std::process::id());
        assert!(!has_session(&name));
        new_session_detached(&name, "/tmp").unwrap();
        assert!(has_session(&name));
        assert!(list_session_names().unwrap().contains(&name));
        kill_session(&name).unwrap();
        assert!(!has_session(&name));
    }
}
