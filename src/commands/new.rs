use crate::config::Agent;
use crate::{session, tmux};
use anyhow::{Context, Result};

/// Open an *additional* agent session for the current directory, alongside
/// whatever is already running there.
///
/// `amux run` deliberately maps one directory to one session, so re-running it
/// reattaches instead of starting a second agent. That's the right default, but
/// it leaves no way to run two agents side by side on the same project. This
/// gives the extra session its own name — `<normal-name>-<suffix>` — so the
/// primary one is untouched.
///
/// Unlike a resume, this always starts a fresh conversation: the point is a
/// second, independent workspace.
pub fn new_session(agent: &Agent, suffix: &str, agents: &[Agent]) -> Result<()> {
    let cwd = std::env::current_dir()
        .context("cannot read current directory")?
        .canonicalize()
        .context("cannot canonicalize current directory")?;

    // Shared with the serve endpoint so a name typed here and one sent from the
    // app are sanitized identically — the suffix reaches a multiplexer target
    // spec, where `.` and `:` are separators.
    let suffix = super::super::serve::sessions::sanitize_suffix(suffix)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let base = session::session_name(&agent.alias, &cwd);
    let name = format!("{base}-{suffix}");

    let tmux_ok = tmux::is_available();
    let exists = tmux_ok && tmux::has_session(&name);
    if exists {
        println!("attaching to existing session {name}");
    } else {
        println!("new {} session {name}", agent.name);
    }

    super::run::launch_new(agent, &cwd, &name, exists, tmux_ok, agents)
}

/// Pick a suffix that isn't taken yet: `2`, `3`, `4`, …
///
/// Unused by the CLI, which requires an explicit name, but kept for callers
/// that want the "just give me another one" behaviour.
#[allow(dead_code)]
pub fn next_free_suffix(base: &str) -> String {
    for n in 2..100 {
        if !tmux::has_session(&format!("{base}-{n}")) {
            return n.to_string();
        }
    }
    "new".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffixed_name_is_distinct_from_the_primary_one() {
        let cwd = std::path::Path::new("/tmp/some-project");
        let base = session::session_name("cc", cwd);
        let suffixed = format!("{base}-debug");
        assert_ne!(base, suffixed);
        // The primary name must remain a prefix, so `amux ls` still groups the
        // two under the same project.
        assert!(suffixed.starts_with(&base));
    }

    #[test]
    fn rejects_a_suffix_that_would_change_how_the_name_parses() {
        // Delegates to the serve-side sanitizer; this asserts the wiring, not
        // the rules (those are tested there).
        assert!(crate::serve::sessions::sanitize_suffix("a:b").unwrap() == "a-b");
        assert!(crate::serve::sessions::sanitize_suffix("").is_err());
    }
}
