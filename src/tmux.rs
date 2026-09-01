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

/// The terminal-multiplexer binary to drive. Defaults to `rmux` (tmux-compatible
/// CLI, own daemon, cross-platform). Override with `AMUX_MUX` (or the legacy
/// `AGENT_MONITOR_TMUX_PATH`) — e.g. `AMUX_MUX=tmux` to fall back to tmux.
pub fn mux_bin() -> String {
    std::env::var("AMUX_MUX")
        .or_else(|_| std::env::var("AGENT_MONITOR_TMUX_PATH"))
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "rmux".to_string())
}

pub fn is_available() -> bool {
    Command::new(mux_bin())
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn in_tmux() -> bool {
    std::env::var("TMUX").map(|v| !v.is_empty()).unwrap_or(false)
}

pub fn has_session(name: &str) -> bool {
    // `has-session -t name` matches by prefix, so a suffixed session
    // (`cx_myapp_1a2b3c4d-debug`) would make the bare name
    // (`cx_myapp_1a2b3c4d`) appear to exist. `amux <id>` / `amux run` would
    // then skip creating the real session and fail on attach. Compare against
    // the full session list instead, which is exact.
    Command::new(mux_bin())
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| l.trim() == name)
        })
        .unwrap_or(false)
}

pub fn new_session_detached(name: &str, cwd: &str) -> Result<()> {
    let status = Command::new(mux_bin())
        .args(["new-session", "-d", "-s", name, "-c", cwd])
        .status()?;
    if !status.success() {
        bail!("failed to create session '{name}'");
    }
    pin_window_size(name);
    Ok(())
}

pub fn send_command(name: &str, shell_cmd: &str) -> Result<()> {
    Command::new(mux_bin())
        .args(["send-keys", "-t", name, "-l", shell_cmd])
        .status()?;
    Command::new(mux_bin())
        .args(["send-keys", "-t", name, "Enter"])
        .status()?;
    Ok(())
}

/// Current visible contents of a session's active pane.
///
/// Used to decide what an agent is asking before answering it — see
/// [`crate::commands::run::dismiss_codex_prompts`]. Empty on any failure, so a
/// caller treats "can't tell" as "nothing to answer".
pub fn capture_pane(name: &str) -> String {
    Command::new(mux_bin())
        .args(["capture-pane", "-p", "-t", name])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// Send literal text (no trailing Enter) to a session's active pane.
pub fn send_text(name: &str, text: &str) -> Result<()> {
    Command::new(mux_bin())
        .args(["send-keys", "-t", name, "-l", text])
        .status()?;
    Ok(())
}

/// Send a bare Enter keypress to a session's active pane.
pub fn send_enter(name: &str) -> Result<()> {
    Command::new(mux_bin())
        .args(["send-keys", "-t", name, "Enter"])
        .status()?;
    Ok(())
}

/// Names of all sessions; empty vec if no server is running.
pub fn list_session_names() -> Result<Vec<String>> {
    let out = Command::new(mux_bin())
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

/// Get the current working directory of a session's active pane.
pub fn session_cwd(name: &str) -> Result<String> {
    let out = Command::new(mux_bin())
        .args(["display-message", "-p", "-t", name, "#{pane_current_path}"])
        .output()?;
    if !out.status.success() {
        bail!("failed to get cwd for session '{name}'");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn kill_session(name: &str) -> Result<()> {
    let status = Command::new(mux_bin())
        .args(["kill-session", "-t", name])
        .status()?;
    if !status.success() {
        bail!("failed to kill session '{name}'");
    }
    Ok(())
}

/// Attach (outside a mux, replacing this process) or switch-client (inside one).
/// Pin a session to follow whichever client last used it.
///
/// `window-size` is a per-session option, snapshotted from the global value
/// when the session is created. A session created before `amux init` wrote the
/// config — or on a server whose global is still the default `smallest` — is
/// stuck at the narrowest client that ever attached, and no later change to the
/// global rescues it: connect from a second machine and the window can't grow
/// past the first one's, even after that client is gone.
///
/// Best-effort: a multiplexer without the option must not fail the launch.
pub fn pin_window_size(name: &str) {
    // Resize first, then set the option — not the other way round.
    //
    // `resize-window` switches a window to *manual* sizing; that is tmux's
    // documented behaviour and rmux adopted it in 0.10.0. Setting `latest`
    // first and resizing second therefore threw the pin away, leaving every
    // new session manually sized and unable to follow its client. Doing the
    // forced recompute first and setting the option last gets both: the size
    // is correct now, and it keeps tracking on future attach/detach/resize.
    let _ = Command::new(mux_bin())
        .args(["resize-window", "-t", name, "-A"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let _ = Command::new(mux_bin())
        .args(["set-option", "-t", name, "window-size", "latest"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Recompute a session's size against its clients, shortly from now.
///
/// `resize-window -A` only sees the clients attached *at the moment it runs*,
/// and `attach-session` replaces this process — so a foreground call would
/// always run before the client we care about exists. Spawning a detached
/// helper that sleeps first lets the resize land just after the attach.
fn resize_after_attach(name: &str) {
    let bin = mux_bin();
    #[cfg(unix)]
    let spawned = Command::new("sh")
        .arg("-c")
        .arg(format!(
            // Re-set the option after resizing: `resize-window` switches the
            // window to *manual* sizing, so resizing alone would undo the pin
            // `pin_window_size` just established — and this runs later, so it
            // wins. That is why sessions kept coming back manually sized.
            "sleep 0.4; {mux} resize-window -t {sess} -A >/dev/null 2>&1; \
             {mux} set-option -t {sess} window-size latest >/dev/null 2>&1",
            mux = shell_quote(&bin),
            sess = shell_quote(name)
        ))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    #[cfg(not(unix))]
    let spawned = Command::new("cmd")
        .args([
            "/C",
            &format!(
                "timeout /t 1 /nobreak >nul & \"{bin}\" resize-window -t \"{name}\" -A \
                 & \"{bin}\" set-option -t \"{name}\" window-size latest"
            ),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    let _ = spawned;
}

pub fn attach_or_switch(name: &str) -> Result<()> {
    // Covers sessions created before the option was set — attaching is exactly
    // when a stale size becomes visible.
    pin_window_size(name);
    if in_tmux() {
        Command::new(mux_bin())
            .args(["switch-client", "-t", name])
            .status()?;
        // The client moved after the pin above ran, so recompute against it.
        let _ = Command::new(mux_bin())
            .args(["resize-window", "-t", name, "-A"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        Ok(())
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Attaching does not itself re-evaluate `window-size latest`, so a
            // session sized to an older, narrower client would stay that way.
            resize_after_attach(name);
            let err = Command::new(mux_bin())
                .args(["attach-session", "-t", name])
                .exec();
            bail!("failed to exec attach-session: {err}")
        }
        #[cfg(not(unix))]
        {
            // Windows has no exec(): run attach to completion, then exit with
            // its status so the shell behaves as if we'd been replaced.
            resize_after_attach(name);
            let status = Command::new(mux_bin())
                .args(["attach-session", "-t", name])
                .status()?;
            std::process::exit(status.code().unwrap_or(0));
        }
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
            eprintln!("skipping: no multiplexer installed");
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

    #[test]
    fn has_session_does_not_match_by_prefix() {
        if !is_available() {
            eprintln!("skipping: no multiplexer installed");
            return;
        }
        let name = format!("amuxpre_{}", std::process::id());
        let _ = kill_session(&name);
        let _ = kill_session(&format!("{name}-debug"));
        new_session_detached(&name, "/tmp").unwrap();
        new_session_detached(&format!("{name}-debug"), "/tmp").unwrap();

        assert!(has_session(&name));
        assert!(has_session(&format!("{name}-debug")));

        // Delete the primary; the suffixed session alone must not make the
        // bare name look present — that was the bug that made `amux <id>`
        // skip creating the real session and fail on attach.
        kill_session(&name).unwrap();
        assert!(!has_session(&name), "bare name must be absent when only the suffixed one exists");
        assert!(has_session(&format!("{name}-debug")));

        kill_session(&format!("{name}-debug")).unwrap();
    }

    #[test]
    fn a_new_session_follows_its_latest_client() {
        if !is_available() {
            eprintln!("skipping: no multiplexer installed");
            return;
        }
        let name = format!("amuxsize_{}", std::process::id());
        let _ = kill_session(&name);
        new_session_detached(&name, "/tmp").unwrap();

        let opt = Command::new(mux_bin())
            .args(["show-options", "-t", &name, "window-size"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        let _ = kill_session(&name);

        // A session that inherits the default `smallest` is the bug: it pins
        // itself to the narrowest client that ever attached, so a second
        // machine can never widen it.
        assert!(
            opt.contains("latest"),
            "new sessions must be pinned to the latest client, got {opt:?}"
        );
    }
}
