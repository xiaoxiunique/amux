//! Records and restores the real agent session id per amux tmux session, so
//! `amux run` can resume the *exact* previous conversation (`resume <id>`)
//! instead of the blunt `resume --last` / `--continue`, which only picks the
//! directory's newest rollout and thus recovers the wrong conversation once a
//! newer one exists (e.g. after switching providers).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// `~/.amux/session-ids.json` — maps tmux session name -> agent session id.
fn store_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".amux").join("session-ids.json"))
}

fn read_store() -> BTreeMap<String, String> {
    let Some(p) = store_path() else {
        return BTreeMap::new();
    };
    let Ok(text) = std::fs::read_to_string(&p) else {
        return BTreeMap::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// The agent session id recorded for a tmux session, if any.
pub fn load_id(session_name: &str) -> Option<String> {
    read_store().get(session_name).cloned()
}

/// Record (or overwrite) the agent session id for a tmux session. Best-effort.
pub fn store_id(session_name: &str, id: &str) {
    let mut map = read_store();
    map.insert(session_name.to_string(), id.to_string());
    if let Some(p) = store_path() {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(&map) {
            let _ = std::fs::write(&p, json);
        }
    }
}

/// Args that make the agent resume a specific session id.
pub fn resume_args(agent_name: &str, id: &str) -> Vec<String> {
    match agent_name {
        "codex" => vec!["resume".into(), id.to_string()],
        "claude" => vec!["--resume".into(), id.to_string()],
        _ => Vec::new(),
    }
}

/// Root directory where an agent stores its per-session files.
fn agent_session_root(agent_name: &str) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    match agent_name {
        "codex" => Some(home.join(".codex").join("sessions")),
        "claude" => Some(home.join(".claude").join("projects")),
        _ => None,
    }
}

/// Claude escapes a cwd into its project dir name by replacing every
/// non-alphanumeric char with `-` (e.g. `/a/b_c.d` -> `-a-b-c-d`).
fn claude_project_dir(root: &Path, cwd: &Path) -> PathBuf {
    let escaped: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    root.join(escaped)
}

/// Whether the recorded session `id` still has a backing file (so resuming it
/// won't error). Best-effort; returns true when we can't tell.
pub fn session_file_exists(agent_name: &str, cwd: &Path, id: &str) -> bool {
    let Some(root) = agent_session_root(agent_name) else {
        return true;
    };
    match agent_name {
        "claude" => claude_project_dir(&root, cwd)
            .join(format!("{id}.jsonl"))
            .exists(),
        // rollout filenames end with `-<id>.jsonl`
        "codex" => codex_rollout_with_id(&root, id).is_some(),
        _ => true,
    }
}

fn mtime_epoch(p: &Path) -> f64 {
    p.metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Collect `*.jsonl` files under `root` (recursively), newest mtime first.
fn jsonl_files_by_mtime(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_jsonl(root, &mut out);
    out.sort_by(|a, b| {
        mtime_epoch(b)
            .partial_cmp(&mtime_epoch(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_jsonl(&p, out);
        } else if p.extension().is_some_and(|x| x == "jsonl") {
            out.push(p);
        }
    }
}

/// First-line JSON of a codex rollout -> (cwd, id) from its `session_meta`.
fn codex_meta(path: &Path) -> Option<(String, String)> {
    let text = std::fs::read_to_string(path).ok()?;
    let first = text.lines().next()?;
    let v: serde_json::Value = serde_json::from_str(first).ok()?;
    let p = v.get("payload")?;
    let cwd = p.get("cwd")?.as_str()?.to_string();
    let id = p.get("id")?.as_str()?.to_string();
    Some((cwd, id))
}

/// Newest codex rollout file whose recorded cwd matches. Scans recent files only.
fn newest_codex_rollout_path(root: &Path, cwd: &Path) -> Option<PathBuf> {
    let target = cwd.to_string_lossy();
    jsonl_files_by_mtime(root)
        .into_iter()
        .take(60)
        .find(|f| codex_meta(f).map(|(rcwd, _)| rcwd == target).unwrap_or(false))
}

/// Newest codex rollout whose recorded cwd matches. Returns its id.
fn newest_codex_rollout_for(root: &Path, cwd: &Path) -> Option<String> {
    let p = newest_codex_rollout_path(root, cwd)?;
    codex_meta(&p).map(|(_, id)| id)
}

fn codex_rollout_with_id(root: &Path, id: &str) -> Option<PathBuf> {
    let needle = format!("-{id}.jsonl");
    jsonl_files_by_mtime(root)
        .into_iter()
        .find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().ends_with(&needle)))
}

/// Newest claude session file for a cwd (its project dir).
fn newest_claude_session_path(root: &Path, cwd: &Path) -> Option<PathBuf> {
    let dir = claude_project_dir(root, cwd);
    let mut best: Option<(PathBuf, f64)> = None;
    for e in std::fs::read_dir(&dir).ok()?.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "jsonl") {
            let m = mtime_epoch(&p);
            if best.as_ref().map(|(_, bm)| m > *bm).unwrap_or(true) {
                best = Some((p, m));
            }
        }
    }
    best.map(|(p, _)| p)
}

/// Newest claude session id for a cwd (its project dir).
fn newest_claude_session(root: &Path, cwd: &Path) -> Option<String> {
    newest_claude_session_path(root, cwd)
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()))
}

/// Path to the newest agent session file for `cwd` (codex rollout / claude
/// jsonl). Its mtime is a robust "actively working" signal for the monitor.
pub fn session_file_for(agent_name: &str, cwd: &Path) -> Option<PathBuf> {
    let root = agent_session_root(agent_name)?;
    match agent_name {
        "codex" => newest_codex_rollout_path(&root, cwd),
        "claude" => newest_claude_session_path(&root, cwd),
        _ => None,
    }
}

/// The id of the newest session file for `cwd` right now. Used both to resolve a
/// resume target on relaunch and to opportunistically record the live session's
/// id on re-attach (a running agent writes the newest rollout for its cwd).
pub fn current_id(agent_name: &str, cwd: &Path) -> Option<String> {
    let root = agent_session_root(agent_name)?;
    match agent_name {
        "codex" => newest_codex_rollout_for(&root, cwd),
        "claude" => newest_claude_session(&root, cwd),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn resume_args_by_agent() {
        assert_eq!(resume_args("codex", "abc"), vec!["resume", "abc"]);
        assert_eq!(resume_args("claude", "abc"), vec!["--resume", "abc"]);
        assert!(resume_args("gemini", "abc").is_empty());
    }

    #[test]
    fn claude_project_dir_escapes_nonalnum() {
        let d = claude_project_dir(Path::new("/root"), Path::new("/Users/me/proj_x.y"));
        assert_eq!(d, Path::new("/root/-Users-me-proj-x-y"));
    }

    #[test]
    fn newest_codex_rollout_matches_cwd_and_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let day = root.join("2026/07/09");
        std::fs::create_dir_all(&day).unwrap();

        let write = |name: &str, cwd: &str, id: &str| {
            let p = day.join(name);
            let mut f = std::fs::File::create(&p).unwrap();
            writeln!(
                f,
                r#"{{"type":"session_meta","payload":{{"cwd":"{cwd}","id":"{id}"}}}}"#
            )
            .unwrap();
            p
        };

        let older = write("rollout-a.jsonl", "/work/proj", "id-old");
        // ensure distinct, increasing mtimes
        std::thread::sleep(std::time::Duration::from_millis(20));
        let newer = write("rollout-b.jsonl", "/work/proj", "id-new");
        std::thread::sleep(std::time::Duration::from_millis(20));
        let _other = write("rollout-c.jsonl", "/work/other", "id-other");

        // touch to guarantee order older < newer
        let _ = older.metadata();
        let _ = newer.metadata();

        // newest rollout for the matching cwd wins
        assert_eq!(newest_codex_rollout_for(root, Path::new("/work/proj")).as_deref(), Some("id-new"));
        // unrelated cwd -> none
        assert!(newest_codex_rollout_for(root, Path::new("/work/nope")).is_none());
    }

    #[test]
    fn store_roundtrip_isolated() {
        // Redirect HOME to a temp dir so we don't touch the real store.
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());

        assert_eq!(load_id("cc_x_1234"), None);
        store_id("cc_x_1234", "sess-1");
        assert_eq!(load_id("cc_x_1234").as_deref(), Some("sess-1"));
        store_id("cc_x_1234", "sess-2");
        assert_eq!(load_id("cc_x_1234").as_deref(), Some("sess-2"));

        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }
}
