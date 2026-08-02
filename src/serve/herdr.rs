//! Bridge for [herdr](https://herdr.dev) sessions.
//!
//! `amux serve --herdr` surfaces agents running inside herdr alongside the
//! regular rmux panes, so the phone client sees both. herdr has no
//! tmux-compatible CLI — it exposes a per-session unix socket driven by
//! `herdr --session <s> <group> <cmd>`, each call printing one line of JSON.
//!
//! Two behaviours were established by probing a live herdr 0.7.5 server, and
//! the code depends on them:
//!
//! * Auto-detected agents (the normal case — you type `claude` in a pane)
//!   carry **no `name` field**, unlike ones started via `agent start <name>`.
//!   So every command targets the **`pane_id`** (`w1:p1`), which works for
//!   both kinds.
//! * `agent prompt` only fills the composer; it does **not** submit. A
//!   separate `agent send-keys <target> Enter` is required. `send-keys` takes
//!   key *names* only — passing text returns `invalid_key`.

use crate::session;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Set once at startup by `--herdr`. `build_snapshot` consults this rather
/// than threading a flag through every call site.
static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Pane ids we hand out are prefixed so they can't collide with rmux's `%59`
/// and so `/api/send` can route back to the right backend.
pub const PANE_PREFIX: &str = "herdr:";

/// Cached pane text, keyed by our prefixed pane id.
///
/// `revision` is NOT a content version — it tracks pane structure/state
/// changes, and stays put while an agent streams output (verified against a
/// live server: revision held at 4 while the screen grew from 613B to 1339B).
/// So the cache is time-based: it only spares us a `pane read` for panes that
/// were just read, which is enough to keep the poll loop cheap without ever
/// serving stale text.
static TAIL_CACHE: LazyLock<Mutex<HashMap<String, (Instant, String)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// How long a cached tail may be reused. Shorter than the 2.5s snapshot tick,
/// so a pane's text is re-read essentially every poll while still collapsing
/// the bursts of extra snapshot builds that follow a send.
const TAIL_TTL: Duration = Duration::from_millis(900);

/// Remembers whether the herdr binary is missing so we log that once instead
/// of on every 2.5s poll.
static MISSING_LOGGED: AtomicBool = AtomicBool::new(false);

fn herdr_bin() -> String {
    std::env::var("AMUX_HERDR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "herdr".to_string())
}

/// Run a herdr CLI command and return stdout. `None` on any failure — a
/// missing/broken herdr must never disturb the rmux snapshot.
///
/// herdr reports API-level failures as **exit status 0 with an `{"error":…}`
/// body** (e.g. `invalid_key`), so the status code alone is not enough to tell
/// success from failure.
fn run(args: &[String]) -> Option<String> {
    let out = Command::new(herdr_bin())
        .args(args)
        .output()
        .map_err(|error| {
            if !MISSING_LOGGED.swap(true, Ordering::Relaxed) {
                eprintln!("[herdr] cannot run `{}`: {error}", herdr_bin());
            }
        })
        .ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        eprintln!(
            "[herdr] {:?} exited {}: {}",
            args,
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return None;
    }
    if let Some(message) = api_error(&stdout) {
        eprintln!("[herdr] {args:?} failed: {message}");
        return None;
    }
    Some(stdout)
}

/// Extract an API-level error message from a herdr reply, if present.
fn api_error(stdout: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct ErrBody {
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        code: Option<String>,
    }
    #[derive(Deserialize)]
    struct ErrEnvelope {
        error: ErrBody,
    }
    let env: ErrEnvelope = parse_line(stdout)?;
    Some(
        env.error
            .message
            .or(env.error.code)
            .unwrap_or_else(|| "unknown herdr error".to_string()),
    )
}

fn run_in(sessionname: &str, args: &[&str]) -> Option<String> {
    let mut argv = vec!["--session".to_string(), sessionname.to_string()];
    argv.extend(args.iter().map(|a| a.to_string()));
    run(&argv)
}

/// herdr replies with one JSON object per line; take the first parseable one.
fn parse_line<T: for<'de> Deserialize<'de>>(text: &str) -> Option<T> {
    text.lines()
        .find_map(|line| serde_json::from_str::<T>(line.trim()).ok())
}

#[derive(Deserialize)]
struct Envelope<T> {
    result: Option<T>,
}

#[derive(Deserialize)]
struct PaneList {
    #[serde(default)]
    panes: Vec<HerdrPane>,
}

/// A pane as reported by `herdr pane list`. Agent-bearing panes additionally
/// carry `agent` / `agent_status`; plain shell panes don't, and are skipped.
#[derive(Deserialize, Clone)]
pub struct HerdrPane {
    pub pane_id: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub agent_status: Option<String>,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub terminal_title: String,
    #[serde(default)]
    pub workspace_id: String,
}

/// One herdr agent pane, normalised into the shape `build_snapshot` wants.
pub struct Bridged {
    /// Prefixed pane id, e.g. `herdr:default:w1:p1`.
    pub id: String,
    /// Synthesised amux-style session name so existing clients pick the right
    /// avatar and project label without any change.
    pub session: String,
    pub cwd: String,
    pub title: String,
    /// herdr's own status string (`working` / `idle` / `blocked` / …).
    pub agent_status: String,
    /// `claude` / `codex` / …
    pub agent: String,
    pub tail: String,
    pub workspace_id: String,
}

/// Names of running herdr sessions (`herdr session list`). The command prints
/// a plain table, not JSON: `name status directory socket`.
fn running_sessions() -> Vec<String> {
    let Some(text) = run(&["session".to_string(), "list".to_string()]) else {
        return Vec::new();
    };
    text.lines()
        .skip(1) // header
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let name = cols.next()?;
            let status = cols.next()?;
            (status == "running").then(|| name.to_string())
        })
        .collect()
}

/// Map herdr's agent kind to the amux alias used in session names, which is
/// what the Flutter client keys its avatar off.
fn alias_for(agent: &str) -> &'static str {
    match agent {
        "codex" => "cx",
        _ => "cc",
    }
}

/// Build the amux-style session name for a herdr pane.
///
/// `cc-herdr_<project>_<hash8>` — the client takes the text before `-` as the
/// agent alias (the same rule that makes `cc-glm_*` work), so the avatar and
/// project name render correctly with no client change. The `-herdr` marker
/// also keeps it distinct from the rmux session for the same directory.
fn session_name_for(agent: &str, cwd: &str) -> String {
    let alias = format!("{}-herdr", alias_for(agent));
    session::session_name(&alias, Path::new(cwd))
}

/// Read a pane's visible text, reusing a very recent cached read.
fn tail_for(sessionname: &str, pane: &HerdrPane, id: &str) -> String {
    if let Some((at, cached)) = TAIL_CACHE
        .lock()
        .expect("herdr tail cache mutex poisoned")
        .get(id)
    {
        if at.elapsed() < TAIL_TTL {
            return cached.clone();
        }
    }
    let text = run_in(sessionname, &["pane", "read", &pane.pane_id]).unwrap_or_default();
    TAIL_CACHE
        .lock()
        .expect("herdr tail cache mutex poisoned")
        .insert(id.to_string(), (Instant::now(), text.clone()));
    text
}

/// Collect every agent pane across all running herdr sessions.
///
/// Returns an empty vec when herdr isn't installed or no session is up, so the
/// caller can append unconditionally.
pub fn collect() -> Vec<Bridged> {
    if !enabled() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for sessionname in running_sessions() {
        let Some(text) = run_in(&sessionname, &["pane", "list"]) else {
            continue;
        };
        let Some(env) = parse_line::<Envelope<PaneList>>(&text) else {
            continue;
        };
        let Some(list) = env.result else { continue };

        for pane in list.panes {
            // Only panes actually running an agent are worth surfacing.
            let (Some(agent), Some(status)) = (pane.agent.clone(), pane.agent_status.clone())
            else {
                continue;
            };
            if pane.cwd.is_empty() {
                continue;
            }
            let id = format!("{PANE_PREFIX}{sessionname}:{}", pane.pane_id);
            let tail = tail_for(&sessionname, &pane, &id);
            out.push(Bridged {
                session: session_name_for(&agent, &pane.cwd),
                cwd: pane.cwd.clone(),
                title: if pane.terminal_title.is_empty() {
                    agent.clone()
                } else {
                    pane.terminal_title.clone()
                },
                agent_status: status,
                agent,
                tail,
                workspace_id: pane.workspace_id.clone(),
                id,
            });
        }
    }
    prune_cache(&out);
    out
}

/// Drop cache entries for panes that no longer exist.
fn prune_cache(current: &[Bridged]) {
    let live: std::collections::HashSet<&str> = current.iter().map(|b| b.id.as_str()).collect();
    TAIL_CACHE
        .lock()
        .expect("herdr tail cache mutex poisoned")
        .retain(|id, _| live.contains(id.as_str()));
}

/// Split a prefixed pane id back into `(session, pane_id)`.
/// `herdr:default:w1:p1` → `("default", "w1:p1")`.
pub fn split_pane_id(id: &str) -> Option<(String, String)> {
    let rest = id.strip_prefix(PANE_PREFIX)?;
    let (sessionname, pane) = rest.split_once(':')?;
    if sessionname.is_empty() || pane.is_empty() {
        return None;
    }
    Some((sessionname.to_string(), pane.to_string()))
}

/// True when this pane id belongs to the herdr backend.
pub fn owns(pane_id: &str) -> bool {
    pane_id.starts_with(PANE_PREFIX)
}

/// Send a message to a herdr agent.
///
/// `agent prompt` fills the composer without submitting, so `enter` is a
/// separate `send-keys` call. Both target the pane id, which works for
/// auto-detected agents (they have no name).
pub fn send(pane_id: &str, text: &str, enter: bool) -> Result<(), String> {
    let (sessionname, pane) =
        split_pane_id(pane_id).ok_or_else(|| format!("not a herdr pane id: {pane_id}"))?;

    run_in(&sessionname, &["agent", "prompt", &pane, text])
        .ok_or_else(|| format!("herdr agent prompt failed for {pane}"))?;

    if enter {
        // Give the composer a moment to accept the text before submitting.
        std::thread::sleep(Duration::from_millis(120));
        run_in(&sessionname, &["agent", "send-keys", &pane, "Enter"])
            .ok_or_else(|| format!("herdr send-keys Enter failed for {pane}"))?;
    }
    Ok(())
}

/// Send a bare key (e.g. `Enter`, `Escape`) to a herdr agent pane.
pub fn send_key(pane_id: &str, key: &str) -> Result<(), String> {
    let (sessionname, pane) =
        split_pane_id(pane_id).ok_or_else(|| format!("not a herdr pane id: {pane_id}"))?;
    run_in(&sessionname, &["agent", "send-keys", &pane, key])
        .ok_or_else(|| format!("herdr send-keys {key} failed for {pane}"))
        .map(|_| ())
}

/// Log once that herdr produced nothing, to aid diagnosis without spamming.
pub fn note_empty_once() {
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        eprintln!("[herdr] enabled but no running herdr session with agents was found");
    }
}

/// Unused today, kept so the poll loop can later throttle herdr queries
/// independently of the main tick.
#[allow(dead_code)]
pub struct Throttle(Instant);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_id_roundtrip() {
        let id = format!("{PANE_PREFIX}default:w1:p1");
        assert!(owns(&id));
        let (s, p) = split_pane_id(&id).unwrap();
        assert_eq!(s, "default");
        assert_eq!(p, "w1:p1");
    }

    #[test]
    fn rmux_pane_ids_are_not_claimed() {
        assert!(!owns("%59"));
        assert!(split_pane_id("%59").is_none());
        assert!(split_pane_id("herdr:").is_none());
        assert!(split_pane_id("herdr:onlysession").is_none());
    }

    #[test]
    fn session_name_matches_client_avatar_rules() {
        let claude = session_name_for("claude", "/tmp/myproj");
        let codex = session_name_for("codex", "/tmp/myproj");
        // Client takes the text before '_' then before '-' as the alias.
        assert!(claude.starts_with("cc-herdr_"), "{claude}");
        assert!(codex.starts_with("cx-herdr_"), "{codex}");
        // Distinct from each other, and stable for the same directory.
        assert_ne!(claude, codex);
        assert_eq!(claude, session_name_for("claude", "/tmp/myproj"));
        // Would not collide with a plain rmux session for the same dir.
        assert_ne!(claude, session::session_name("cc", Path::new("/tmp/myproj")));
    }

    #[test]
    fn parses_a_pane_list_reply() {
        // Shape captured from a live herdr 0.7.5 server.
        let line = r#"{"id":"cli:pane:list","result":{"panes":[{"agent":"claude","agent_status":"working","cwd":"/tmp/p","focused":true,"pane_id":"w1:p1","revision":3,"terminal_title":"Claude Code","workspace_id":"w1"}],"type":"pane_list"}}"#;
        let env: Envelope<PaneList> = parse_line(line).unwrap();
        let panes = env.result.unwrap().panes;
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].pane_id, "w1:p1");
        assert_eq!(panes[0].agent.as_deref(), Some("claude"));
        assert_eq!(panes[0].agent_status.as_deref(), Some("working"));
        assert_eq!(panes[0].revision, 3);
    }

    #[test]
    fn skips_panes_without_an_agent() {
        // A plain shell pane: no agent / agent_status.
        let line = r#"{"result":{"panes":[{"agent_status":"unknown","cwd":"/tmp/p","pane_id":"w1:p1","revision":1,"workspace_id":"w1"}],"type":"pane_list"}}"#;
        let env: Envelope<PaneList> = parse_line(line).unwrap();
        let panes = env.result.unwrap().panes;
        // agent is absent, so collect() would skip it.
        assert!(panes[0].agent.is_none());
    }

    #[test]
    fn session_list_table_is_parsed_for_running_only() {
        // running_sessions parses this shape; exercise the same logic.
        let table = "name status directory socket\n\
                     default stopped /a /a/s.sock\n\
                     work running /b /b/s.sock\n";
        let names: Vec<String> = table
            .lines()
            .skip(1)
            .filter_map(|line| {
                let mut c = line.split_whitespace();
                let n = c.next()?;
                let s = c.next()?;
                (s == "running").then(|| n.to_string())
            })
            .collect();
        assert_eq!(names, vec!["work"]);
    }

    #[test]
    fn api_errors_are_detected_despite_exit_zero() {
        // herdr returns exit 0 with an error body; run() must treat that as a
        // failure, else a failed send would be reported as delivered.
        let bad = r#"{"error":{"code":"invalid_key","message":"unsupported key foo"},"id":"cli:agent:send-keys"}"#;
        assert_eq!(api_error(bad).as_deref(), Some("unsupported key foo"));
        let ok = r#"{"id":"cli:agent:send-keys","result":{"type":"ok"}}"#;
        assert!(api_error(ok).is_none());
    }

    #[test]
    fn tail_cache_expires_so_streaming_output_is_not_stale() {
        // Regression: the cache was keyed on `revision`, but herdr holds that
        // steady while an agent streams, which froze the tail. It must be
        // time-based instead.
        let id = "herdr:s:w1:p1";
        TAIL_CACHE
            .lock()
            .unwrap()
            .insert(id.to_string(), (Instant::now(), "old".into()));
        // A just-written entry is reused...
        let fresh = TAIL_CACHE.lock().unwrap().get(id).map(|(at, t)| (at.elapsed() < TAIL_TTL, t.clone()));
        assert_eq!(fresh, Some((true, "old".to_string())));
        // ...but one older than the TTL is not.
        TAIL_CACHE.lock().unwrap().insert(
            id.to_string(),
            (Instant::now() - TAIL_TTL - Duration::from_millis(50), "old".into()),
        );
        let stale = TAIL_CACHE.lock().unwrap().get(id).map(|(at, _)| at.elapsed() < TAIL_TTL);
        assert_eq!(stale, Some(false));
        TAIL_CACHE.lock().unwrap().remove(id);
    }
}
