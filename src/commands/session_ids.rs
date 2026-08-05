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
        "codex" => {
            // The provider a session was recorded under may no longer exist in
            // config.toml — cc-switch replaces `model_providers` wholesale on
            // every switch. Codex then refuses to open the session at all, so
            // re-supply the name on the command line when it's missing.
            let mut args = codex_session_provider(id)
                .map(|p| codex_provider_patch(&p))
                .unwrap_or_default();
            args.push("resume".into());
            args.push(id.to_string());
            args
        }
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

/// The `model_provider` a codex rollout was recorded under, if any.
///
/// Codex refuses to resume a session whose provider is missing from
/// config.toml, and cc-switch rewrites `model_providers` wholesale on every
/// switch — so a session recorded under a since-replaced provider becomes
/// unopenable. Reading the name back lets us re-supply it at launch.
pub fn codex_session_provider(id: &str) -> Option<String> {
    let root = agent_session_root("codex")?;
    let path = codex_rollout_with_id(&root, id)?;
    let text = std::fs::read_to_string(&path).ok()?;
    let v: serde_json::Value = serde_json::from_str(text.lines().next()?).ok()?;
    let name = v.get("payload")?.get("model_provider")?.as_str()?;
    (!name.is_empty()).then(|| name.to_string())
}

/// Providers codex ships itself. Overriding one is rejected outright ("reserved
/// built-in provider IDs"), so these must never be re-supplied.
fn is_builtin_provider(name: &str) -> bool {
    matches!(name, "openai" | "oss" | "azure")
}

/// `-c model_providers.<name>={...}` arguments that define `provider` as an
/// alias of the local proxy, or nothing when the provider needs no help.
///
/// Returns empty when the provider is already configured, is a codex built-in,
/// or the config can't be read — in each case codex resolves it on its own and
/// an override would be at best redundant, at worst rejected.
pub fn codex_provider_patch(provider: &str) -> Vec<String> {
    if provider.is_empty() || is_builtin_provider(provider) {
        return Vec::new();
    }
    if codex_config_has_provider(provider) {
        return Vec::new();
    }
    let Some(base_url) = codex_proxy_base_url() else {
        return Vec::new();
    };
    vec![
        "-c".to_string(),
        format!(
            "model_providers.{provider}={{name=\"{provider}\",\
             base_url=\"{base_url}\",wire_api=\"responses\",\
             requires_openai_auth=true,experimental_bearer_token=\"PROXY_MANAGED\"}}"
        ),
    ]
}

/// True when config.toml already declares `[model_providers.<name>]`.
fn codex_config_has_provider(name: &str) -> bool {
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    let Ok(text) = std::fs::read_to_string(home.join(".codex").join("config.toml")) else {
        return false;
    };
    let header = format!("[model_providers.{name}]");
    text.lines().any(|l| l.trim() == header)
}

/// Base URL of whatever provider config.toml currently points at — the local
/// cc-switch proxy in practice. Reused for the alias so a resumed session
/// talks to the same endpoint a fresh one would.
fn codex_proxy_base_url() -> Option<String> {
    let home = dirs::home_dir()?;
    let text = std::fs::read_to_string(home.join(".codex").join("config.toml")).ok()?;
    text.lines()
        .map(str::trim)
        .find(|l| l.starts_with("base_url"))
        .and_then(|l| l.split('=').nth(1))
        .map(|v| v.trim().trim_matches('"').to_string())
        .filter(|v| !v.is_empty())
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

fn codex_rollout_with_id(root: &Path, id: &str) -> Option<PathBuf> {    let needle = format!("-{id}.jsonl");
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

/// One past conversation for a directory, newest first.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PastSession {
    pub id: String,
    /// Local path to the transcript. Skipped when serialized — remote clients
    /// have no use for a host-side path, and it leaks the home directory.
    #[serde(skip)]
    pub path: PathBuf,
    /// Seconds since the epoch of the file's last write.
    pub modified: f64,
    /// Bytes on disk — a rough proxy for how much work is in it.
    pub size: u64,
    /// Short description: Claude's own generated title, or the opening user
    /// prompt for Codex, which records none. `None` when neither is readable.
    pub summary: Option<String>,
}

/// A session located by id prefix, along with everything needed to resume it.
pub struct FoundSession {
    pub agent: &'static str,
    pub id: String,
    pub cwd: PathBuf,
    pub summary: Option<String>,
}

/// True when `s` looks like a session id prefix rather than a directory name.
///
/// Both agents use hex-and-dash ids (UUID for Claude, ULID-ish for Codex),
/// while project names contain letters outside `a-f` or are shorter, so this
/// cleanly separates `amux 019fc770` from `amux mbox`.
pub fn looks_like_session_id(s: &str) -> bool {
    s.len() >= 6
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() || c == '-')
        && s.chars().any(|c| c.is_ascii_digit())
}

/// Find a session by id prefix across every project, newest first.
///
/// Searches both agents so the caller doesn't have to know which one recorded
/// it — that is the whole point of `amux <id>`.
pub fn find_by_id_prefix(prefix: &str) -> Vec<FoundSession> {
    /// Same bound as `recent_sessions`: parsing every rollout ever written to
    /// answer one lookup would be slow, and ids resolve to recent work.
    const SCAN_LIMIT: usize = 800;

    let needle = prefix.to_lowercase();
    let mut out = Vec::new();

    // Claude: id is the filename, and the project dir encodes the cwd.
    if let Some(root) = agent_session_root("claude") {
        if let Ok(projects) = std::fs::read_dir(&root) {
            for project in projects.flatten() {
                let Ok(files) = std::fs::read_dir(project.path()) else {
                    continue;
                };
                for f in files.flatten() {
                    let p = f.path();
                    if p.extension().is_none_or(|x| x != "jsonl") {
                        continue;
                    }
                    let id = p
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if !id.to_lowercase().starts_with(&needle) {
                        continue;
                    }
                    // The transcript records its own cwd; the escaped
                    // directory name can't be reversed (both `/` and `_`
                    // become `-`).
                    if let Some(cwd) = claude_cwd(&p) {
                        out.push(FoundSession {
                            agent: "claude",
                            summary: session_summary(&p, "claude"),
                            id,
                            cwd,
                        });
                    }
                }
            }
        }
    }

    // Codex: id is inside the file, and so is the cwd.
    if let Some(root) = agent_session_root("codex") {
        for p in jsonl_files_by_mtime(&root).into_iter().take(SCAN_LIMIT) {
            // Cheap pre-filter: the id is also in the filename.
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
            if !name
                .map(|n| n.to_lowercase().contains(&needle))
                .unwrap_or(false)
            {
                continue;
            }
            if let Some((cwd, id)) = codex_meta(&p) {
                if id.to_lowercase().starts_with(&needle) {
                    out.push(FoundSession {
                        agent: "codex",
                        summary: session_summary(&p, "codex"),
                        id,
                        cwd: PathBuf::from(cwd),
                    });
                }
            }
        }
    }

    out
}

/// The cwd a Claude transcript recorded for itself.
fn claude_cwd(path: &Path) -> Option<PathBuf> {
    use std::io::{BufRead, BufReader};

    let file = std::fs::File::open(path).ok()?;
    for line in BufReader::new(file).lines().take(60).map_while(Result::ok) {
        if !line.contains("\"cwd\"") {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
            if let Some(cwd) = v.get("cwd").and_then(|c| c.as_str()) {
                return Some(PathBuf::from(cwd));
            }
        }
    }
    None
}

/// A short description of what a session was about.
///
/// Claude Code maintains its own generated title (`ai-title` records, rewritten
/// as the conversation evolves) and that is what its resume picker shows, so we
/// use the last one. Codex records no title at all, so its opening user prompt
/// stands in — the same thing its own picker falls back to.
fn session_summary(path: &Path, agent: &str) -> Option<String> {
    match agent {
        "claude" => last_ai_title(path).or_else(|| first_user_prompt(path, agent)),
        _ => first_user_prompt(path, agent),
    }
}

/// The most recent `ai-title` in a Claude transcript.
///
/// Titles are appended throughout the session and refined as it goes, so the
/// last one is the one Claude Code itself displays. This has to scan the whole
/// file, which is why only the title line is parsed — a substring test first
/// keeps the 90MB transcripts cheap.
fn last_ai_title(path: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader};

    let file = std::fs::File::open(path).ok()?;
    let mut title = None;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if !line.contains("\"ai-title\"") {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
            if let Some(t) = v.get("aiTitle").and_then(|t| t.as_str()) {
                let cleaned = clean_prompt(t);
                if !cleaned.is_empty() {
                    title = Some(cleaned);
                }
            }
        }
    }
    title
}

/// First real user prompt in a session file — what it was actually about.
///
/// Used for Codex, which stores no title of its own. Only the head of the file
/// is read: the opening turn is near the top, and these transcripts run to tens
/// of megabytes.
fn first_user_prompt(path: &Path, agent: &str) -> Option<String> {
    use std::io::{BufRead, BufReader};

    /// Enough lines to clear the session header and any injected preamble
    /// without reading a 90MB transcript.
    const MAX_LINES: usize = 400;

    let file = std::fs::File::open(path).ok()?;
    for line in BufReader::new(file).lines().take(MAX_LINES).map_while(Result::ok) {
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let text = match agent {
            // {"type":"event_msg","payload":{"type":"user_message","message":"…"}}
            "codex" => v
                .get("payload")
                .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("user_message"))
                .and_then(|p| p.get("message"))
                .and_then(|m| m.as_str())
                .map(str::to_string),
            // {"type":"user","message":{"content": "…" | [{"type":"text","text":"…"}]}}
            "claude" => v
                .get("type")
                .filter(|t| t.as_str() == Some("user"))
                .and(v.get("message"))
                .and_then(|m| m.get("content"))
                .and_then(|c| match c {
                    serde_json::Value::String(s) => Some(s.clone()),
                    serde_json::Value::Array(items) => {
                        let joined: String = items
                            .iter()
                            .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join(" ");
                        (!joined.is_empty()).then_some(joined)
                    }
                    _ => None,
                }),
            _ => None,
        };

        let Some(text) = text else { continue };
        let cleaned = clean_prompt(&text);
        if !cleaned.is_empty() {
            return Some(cleaned);
        }
    }
    None
}

/// Collapse whitespace and drop turns that aren't the user talking: tool
/// results, and the harness-injected blocks (`<local-command-caveat>`,
/// `<system-reminder>`, …) that would otherwise be mistaken for the prompt.
fn clean_prompt(raw: &str) -> String {
    let text = raw.trim();
    if text.starts_with('<') || text.contains("tool_result") {
        return String::new();
    }
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    // A handful of characters is a stray fragment, not a description.
    if collapsed.chars().count() < 4 {
        return String::new();
    }
    collapsed
}

/// The most recent `limit` sessions an agent recorded for `cwd`, newest first.
///
/// Codex stores every rollout in one flat tree and records the cwd inside the
/// file, so its scan is bounded (`SCAN_LIMIT`) to avoid parsing thousands of
/// unrelated rollouts. Claude keys sessions by an escaped project directory,
/// so its lookup is a plain directory read.
pub fn recent_sessions(agent_name: &str, cwd: &Path, limit: usize) -> Vec<PastSession> {
    /// How many of the newest codex rollouts to inspect. Each one costs a
    /// file read (only the first line is parsed), and rollouts for other
    /// projects are interleaved, so this trades completeness for speed.
    const SCAN_LIMIT: usize = 400;

    let Some(root) = agent_session_root(agent_name) else {
        return Vec::new();
    };

    let describe = |path: PathBuf, id: String| {
        let size = path.metadata().map(|m| m.len()).unwrap_or(0);
        PastSession {
            modified: mtime_epoch(&path),
            summary: session_summary(&path, agent_name),
            id,
            path,
            size,
        }
    };

    match agent_name {
        "codex" => {
            let target = cwd.to_string_lossy();
            jsonl_files_by_mtime(&root)
                .into_iter()
                .take(SCAN_LIMIT)
                .filter_map(|f| {
                    codex_meta(&f)
                        .filter(|(rcwd, _)| *rcwd == target)
                        .map(|(_, id)| describe(f, id))
                })
                .take(limit)
                .collect()
        }
        "claude" => {
            let dir = claude_project_dir(&root, cwd);
            let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
                .collect();
            files.sort_by(|a, b| {
                mtime_epoch(b)
                    .partial_cmp(&mtime_epoch(a))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            files
                .into_iter()
                .take(limit)
                .map(|p| {
                    // Claude names the file after the session id.
                    let id = p
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    describe(p, id)
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn builtin_providers_are_never_overridden() {
        // Codex rejects these outright: "reserved built-in provider IDs".
        assert!(super::codex_provider_patch("openai").is_empty());
        assert!(super::codex_provider_patch("oss").is_empty());
        assert!(super::codex_provider_patch("").is_empty());
    }

    #[test]
    fn provider_patch_precedes_the_resume_subcommand() {
        // `-c` is a global flag; codex rejects it after the subcommand. Any
        // patch resume_args produces must come first, and `resume <id>` must
        // remain the final pair.
        let args = super::resume_args("codex", "019f9207-b3ec-7b82-8494-e123bdb77987");
        let resume_at = args
            .iter()
            .position(|a| a == "resume")
            .expect("resume subcommand must be present");
        assert_eq!(args.len(), resume_at + 2, "id must follow resume");
        // Everything before it is a well-formed -c pair.
        assert_eq!(resume_at % 2, 0, "leading args must pair up");
        for i in (0..resume_at).step_by(2) {
            assert_eq!(args[i], "-c", "only -c pairs may precede resume");
        }
    }

    #[test]
    fn claude_resume_is_unaffected() {
        assert_eq!(
            super::resume_args("claude", "abc"),
            vec!["--resume".to_string(), "abc".to_string()]
        );
        assert!(super::resume_args("vim", "abc").is_empty());
    }

    #[test]
    fn distinguishes_session_ids_from_project_names() {
        // Real ids from both agents.
        assert!(looks_like_session_id("019fc770"));
        assert!(looks_like_session_id("7ec5d280-a655-4577-83d5-7d0b93392cd1"));
        assert!(looks_like_session_id("dab626ac"));
        // Project names `amux <name>` must keep matching directories.
        assert!(!looks_like_session_id("mbox"));
        assert!(!looks_like_session_id("sitin"));
        assert!(!looks_like_session_id("amux"));
        assert!(!looks_like_session_id("agent-port"));
        // `deadbeef` is all-hex but has no digit, so it reads as a name.
        assert!(!looks_like_session_id("deadbeef"));
        // Too short to be an id prefix worth resolving.
        assert!(!looks_like_session_id("019f"));
    }

    #[test]
    fn prefers_the_last_ai_title_for_claude() {
        // Claude appends ai-title records and refines them as the session
        // goes; its resume picker shows the final one, so we must too.
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("s.jsonl");
        std::fs::write(
            &f,
            concat!(
                r#"{"type":"user","message":{"content":"first prompt here"}}"#, "\n",
                r#"{"type":"ai-title","aiTitle":"early guess"}"#, "\n",
                r#"{"type":"ai-title","aiTitle":"hono-to-python-backend-migration"}"#, "\n",
            ),
        )
        .unwrap();
        assert_eq!(
            session_summary(&f, "claude").as_deref(),
            Some("hono-to-python-backend-migration")
        );
        // Codex has no titles, so it falls back to the opening prompt.
        let g = dir.path().join("c.jsonl");
        std::fs::write(
            &g,
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"改一下导出功能"}}"#,
        )
        .unwrap();
        assert_eq!(session_summary(&g, "codex").as_deref(), Some("改一下导出功能"));
    }

    #[test]
    fn cleans_prompts_and_rejects_injected_blocks() {
        assert_eq!(clean_prompt("  hello   world \n"), "hello world");
        // Harness-injected turns are not the user's prompt.
        assert_eq!(clean_prompt("<local-command-caveat>Caveat: …"), "");
        assert_eq!(clean_prompt("<system-reminder>x</system-reminder>"), "");
        assert_eq!(clean_prompt("[{\"tool_result\": 1}]"), "");
        // Stray fragments aren't descriptions.
        assert_eq!(clean_prompt("ok"), "");
        // Multi-byte text survives intact.
        assert_eq!(clean_prompt(" 改一下导出功能 "), "改一下导出功能");
    }
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
