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
    /// "claude" or "codex".
    pub agent: String,
    pub sessions: Vec<PastSession>,
}

/// How many conversations per agent to return when the caller doesn't say.
pub const DEFAULT_LIMIT: usize = 20;

/// Cap on `limit`, so a client can't ask us to parse thousands of transcripts.
const MAX_LIMIT: usize = 100;

/// List recent conversations for `dir`, newest first, for both agents.
///
/// Blocking: reads and parses transcript files. Call inside `spawn_blocking`.
pub fn list(dir: &str, limit: Option<usize>) -> Result<Vec<AgentSessions>, String> {
    let cwd = canonical_project_dir(dir)?;
    let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    Ok(["claude", "codex"]
        .into_iter()
        .map(|agent| AgentSessions {
            agent: agent.to_string(),
            sessions: session_ids::recent_sessions(agent, &cwd, limit),
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
    if agent != "claude" && agent != "codex" {
        return Err(format!("unsupported agent: {agent}"));
    }
    if session_id.trim().is_empty() {
        return Err("sessionId is required".to_string());
    }
    if !session_ids::looks_like_session_id(session_id) {
        return Err(format!("not a session id: {session_id}"));
    }
    let suffix = sanitize_suffix(suffix)?;

    let alias = if agent == "codex" { "cx" } else { "cc" };
    let name = format!(
        "{}-{}",
        crate::session::session_name(alias, &cwd),
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
fn labels_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".amux").join("session-labels.json"))
}

pub fn labels() -> std::collections::BTreeMap<String, String> {
    let Some(p) = labels_path() else {
        return Default::default();
    };
    std::fs::read_to_string(&p)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
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

    let mut map = labels();
    if label.is_empty() {
        map.remove(session);
    } else {
        map.insert(session.to_string(), label.to_string());
    }

    let p = labels_path().ok_or("cannot determine home directory")?;
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let json = serde_json::to_string_pretty(&map).map_err(|e| e.to_string())?;
    std::fs::write(&p, json).map_err(|e| format!("writing {}: {e}", p.display()))
}

#[cfg(test)]
mod label_tests {
    use super::*;

    /// Isolated: these write to $HOME, so point it somewhere disposable.
    fn with_temp_home(body: impl FnOnce()) {
        let _home_guard = crate::test_home::lock();
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());
        body();
        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
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
