//! Past agent conversations for a project directory, and resuming one into a
//! separate multiplexer session.
//!
//! The listing side is a thin wrapper over `commands::session_ids`, which reads
//! the agents' own stores — so conversations started outside amux show up too.
//!
//! The resume side deliberately does *not* reuse the plain project session
//! name. `amux run` maps one directory to one session; resuming an old
//! conversation there would either collide with the agent already running or
//! silently replace it. Instead the caller supplies a suffix and we launch
//! `<normal-session-name>-<suffix>`, which leaves the primary session alone.

use crate::commands::session_ids::{self, PastSession};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Conversations for one agent in one directory.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessions {
    /// The agent's configured name, e.g. "claude", "codex" or "pi".
    pub agent: String,
    pub sessions: Vec<PastSession>,
}

/// How many conversations per agent to return when the caller doesn't say.
pub const DEFAULT_LIMIT: usize = 20;

/// Cap on `limit`, so a client can't ask us to parse thousands of transcripts.
const MAX_LIMIT: usize = 100;

/// Every configured agent whose conversations can be listed and resumed.
///
/// Driven by the agent list rather than a hardcoded pair, so an agent added to
/// config.toml (or shipped as a new builtin) shows up here too. Agents without
/// a session store are skipped — an always-empty group is noise to the client.
fn listable_agents() -> Vec<String> {
    let agents =
        crate::config::resolve_agents().unwrap_or_else(|_| crate::config::builtin_agents());
    agents
        .into_iter()
        .filter(|a| session_ids::supports_sessions(&a.name))
        .map(|a| a.name)
        .collect()
}

/// List recent conversations for `dir`, newest first, for every listable agent.
///
/// Blocking: reads and parses transcript files. Call inside `spawn_blocking`.
pub fn list(dir: &str, limit: Option<usize>) -> Result<Vec<AgentSessions>, String> {
    let cwd = canonical_project_dir(dir)?;
    let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    Ok(listable_agents()
        .into_iter()
        .map(|agent| AgentSessions {
            sessions: session_ids::recent_sessions(&agent, &cwd, limit),
            agent,
        })
        .collect())
}

/// Resume `session_id` in a session suffixed with `suffix`, leaving whatever
/// runs in the project's primary session untouched.
///
/// Returns the multiplexer session name. Idempotent: if that suffixed session
/// already exists it is returned as-is rather than relaunched, so tapping twice
/// reattaches instead of spawning a duplicate agent.
pub fn resume(
    dir: &str,
    agent: &str,
    session_id: &str,
    suffix: &str,
) -> Result<String, String> {
    let cwd = canonical_project_dir(dir)?;
    let agent = agent.trim().to_ascii_lowercase();
    if !session_ids::supports_sessions(&agent) {
        return Err(format!("unsupported agent: {agent}"));
    }
    if session_id.trim().is_empty() {
        return Err("sessionId is required".to_string());
    }
    if !session_ids::looks_like_session_id(session_id) {
        return Err(format!("not a session id: {session_id}"));
    }
    let suffix = sanitize_suffix(suffix)?;

    // The alias comes from the agent list, so the name matches what the CLI
    // would build. Deriving it as "cx for codex, cc for everything else" put
    // every other agent's session under a `cc_` name that pointed at the wrong
    // agent entirely.
    let agents =
        crate::config::resolve_agents().unwrap_or_else(|_| crate::config::builtin_agents());
    let alias = crate::config::find(&agents, &agent)
        .map(|a| a.alias.clone())
        .ok_or_else(|| format!("unsupported agent: {agent}"))?;
    let name = format!(
        "{}-{}",
        crate::session::session_name(&alias, &cwd),
        suffix
    );

    if super::server::mux_has_session(&name) {
        return Ok(name);
    }

    let mut argv = vec![base_command(&agent)?];
    argv.extend(session_ids::resume_args(&agent, session_id));

    super::server::mux_new_session(&name, &cwd.to_string_lossy(), &argv.join(" "))?;

    // Remember the id, so a later plain `cc`/`cx` in this suffixed session
    // resumes the same conversation rather than the directory's newest.
    session_ids::store_id(&name, session_id);
    crate::commands::sessions::auto_save(&agents);

    Ok(name)
}

/// The agent's launch command, honouring the same env overrides the rest of
/// the server uses so a custom `codex --yolo`-style command applies here too.
fn base_command(agent: &str) -> Result<String, String> {
    super::server::agent_launch_command_for(agent)
}

/// A directory must exist and be absolute before we build a session name from
/// it — the name embeds a hash of the canonical path, so a stale or relative
/// path would silently produce a session that never matches the CLI's.
fn canonical_project_dir(dir: &str) -> Result<PathBuf, String> {
    let dir = dir.trim();
    if dir.is_empty() {
        return Err("path is required".to_string());
    }
    let p = Path::new(dir);
    if !p.is_absolute() {
        return Err(format!("path must be absolute: {dir}"));
    }
    std::fs::canonicalize(p).map_err(|e| format!("no such directory: {dir} ({e})"))
}

/// Session names are shell- and multiplexer-visible identifiers, and tmux/rmux
/// treat `.` and `:` as target syntax. Restrict the user's suffix to characters
/// that can't change how a name is parsed.
pub(crate) fn sanitize_suffix(raw: &str) -> Result<String, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("suffix is required".to_string());
    }
    if s.chars().count() > 24 {
        return Err("suffix is too long (max 24 characters)".to_string());
    }
    // Keep alphanumerics — including non-ASCII, since these names are only ever
    // displayed and matched literally — plus the two safe separators. Runs of
    // replaced characters collapse into one dash so `a$(id)` reads as `a-id`.
    let mut cleaned = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_alphanumeric() || c == '-' || c == '_' {
            cleaned.push(c);
        } else if !cleaned.ends_with('-') {
            cleaned.push('-');
        }
    }
    let cleaned = cleaned.trim_matches('-').to_string();
    if cleaned.is_empty() {
        return Err(format!("suffix has no usable characters: {raw}"));
    }
    Ok(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffix_keeps_plain_names() {
        assert_eq!(sanitize_suffix("debug").unwrap(), "debug");
        assert_eq!(sanitize_suffix("  fix-2 ").unwrap(), "fix-2");
        assert_eq!(sanitize_suffix("try_1").unwrap(), "try_1");
        // Non-ASCII is fine: names are matched literally, never parsed.
        assert_eq!(sanitize_suffix("试验").unwrap(), "试验");
    }

    #[test]
    fn suffix_neutralizes_target_syntax() {
        // `:` and `.` are window/pane separators in tmux target specs, and a
        // space would split the name into separate arguments.
        assert_eq!(sanitize_suffix("a:b").unwrap(), "a-b");
        assert_eq!(sanitize_suffix("a.b").unwrap(), "a-b");
        assert_eq!(sanitize_suffix("a b").unwrap(), "a-b");
        assert_eq!(sanitize_suffix("a$(id)").unwrap(), "a-id");
        assert_eq!(sanitize_suffix("../../etc").unwrap(), "etc");
    }

    #[test]
    fn suffix_rejects_unusable_input() {
        assert!(sanitize_suffix("").is_err());
        assert!(sanitize_suffix("   ").is_err());
        assert!(sanitize_suffix(":::").is_err());
        assert!(sanitize_suffix(&"x".repeat(25)).is_err());
        assert!(sanitize_suffix(&"x".repeat(24)).is_ok());
    }

    #[test]
    fn project_dir_must_be_absolute_and_exist() {
        assert!(canonical_project_dir("").is_err());
        assert!(canonical_project_dir("relative/path").is_err());
        assert!(canonical_project_dir("/definitely/not/here/xyzzy").is_err());
        let tmp = std::env::temp_dir();
        assert!(canonical_project_dir(&tmp.to_string_lossy()).is_ok());
    }

    #[test]
    fn resume_rejects_bad_agent_and_id() {
        let tmp = std::env::temp_dir().to_string_lossy().to_string();
        assert!(resume(&tmp, "vim", "019fc770", "x").is_err());
        assert!(resume(&tmp, "codex", "", "x").is_err());
        // A directory name must not be mistaken for a session id.
        assert!(resume(&tmp, "codex", "my-project", "x").is_err());
    }
}

// --------------------------------------------------------------- display labels

/// `~/.amux/session-labels.json` — multiplexer session name -> display label.
///
/// A rename here is cosmetic on purpose. Session names encode
/// `<alias>_<dirslug>_<hash8>`, and everything downstream depends on that:
/// `amux run` recomputes the name from the directory to decide whether to
/// re-attach, managed-session detection matches the pattern, and the
/// conversation-id store is keyed by it. Renaming the real session would make
/// a directory's session unfindable from the shell. So the name stays and only
/// the label the client shows changes.
pub fn labels() -> std::collections::BTreeMap<String, String> {
    crate::store::labels()
}

/// Set (or, with an empty label, clear) a session's display label.
pub fn set_label(session: &str, label: &str) -> Result<(), String> {
    let session = session.trim();
    if session.is_empty() {
        return Err("session is required".to_string());
    }
    let label = label.trim();
    // Long enough for a sentence, short enough that it can't be used to stuff
    // the snapshot every client polls.
    if label.chars().count() > 80 {
        return Err("label is too long (max 80 characters)".to_string());
    }

    // One row, rather than rewriting a map of every label that exists: two
    // clients renaming different sessions used to race here.
    crate::store::set_label(session, label);
    Ok(())
}

#[cfg(test)]
mod label_tests {
    use super::*;

    /// Isolated: these write to $HOME, so point it somewhere disposable.
    fn with_temp_home(body: impl FnOnce()) {
        // Takes the lock itself — do not wrap this in another helper that also
        // takes it, because the mutex is not reentrant and the result is a
        // hang rather than an error.
        let _home_guard = crate::test_home::lock();
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("HOME");
        let prev_db = std::env::var_os("AMUX_DB_PATH");
        std::env::set_var("HOME", tmp.path());
        // Labels live in the store now, and `dirs::home_dir()` does not follow
        // HOME on Windows — so redirecting HOME alone left this reading the
        // real database.
        std::env::set_var("AMUX_DB_PATH", tmp.path().join("amux.db"));
        body();
        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match prev_db {
            Some(v) => std::env::set_var("AMUX_DB_PATH", v),
            None => std::env::remove_var("AMUX_DB_PATH"),
        }
    }

    #[test]
    fn set_read_and_clear_a_label() {
        with_temp_home(|| {
            assert!(labels().is_empty());

            set_label("cc_amux_4d8e0883", "重构登录").unwrap();
            assert_eq!(
                labels().get("cc_amux_4d8e0883").map(String::as_str),
                Some("重构登录")
            );

            // Renaming again replaces rather than accumulating.
            set_label("cc_amux_4d8e0883", "改成别的").unwrap();
            assert_eq!(labels().len(), 1);

            // An empty label is how the client clears one.
            set_label("cc_amux_4d8e0883", "  ").unwrap();
            assert!(labels().is_empty());
        });
    }

    #[test]
    fn rejects_an_empty_session_and_an_overlong_label() {
        with_temp_home(|| {
            assert!(set_label("  ", "x").is_err());
            assert!(set_label("s", &"x".repeat(81)).is_err());
            // Multi-byte counts as characters, not bytes, so a Chinese label
            // well under the limit is not rejected for being 3 bytes a glyph.
            assert!(set_label("s", &"名".repeat(80)).is_ok());
        });
    }
}
