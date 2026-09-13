use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::PathBuf,
};

#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookState {
    Running,
    Waiting,
    Idle,
    Failed,
    Done,
}

impl HookState {
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "start" | "started" | "run" | "running" => Ok(Self::Running),
            "wait" | "waiting" | "blocked" => Ok(Self::Waiting),
            "idle" => Ok(Self::Idle),
            "fail" | "failed" | "error" => Ok(Self::Failed),
            "done" | "complete" | "completed" | "finish" | "finished" => Ok(Self::Done),
            other => bail!(
                "invalid hook state '{other}' (expected running, waiting, idle, failed, or done)"
            ),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookStatusEvent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    pub state: HookState,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatusSnapshot {
    by_pane_id: BTreeMap<String, HookStatusEvent>,
    by_session: BTreeMap<String, HookStatusEvent>,
}

impl StatusSnapshot {
    fn apply(&mut self, event: HookStatusEvent) {
        if let Some(pane_id) = event.pane_id.as_ref().filter(|value| !value.is_empty()) {
            self.by_pane_id.insert(pane_id.clone(), event.clone());
        }
        if let Some(session) = event.session.as_ref().filter(|value| !value.is_empty()) {
            self.by_session.insert(session.clone(), event);
        }
    }
}

pub fn state_dir() -> PathBuf {
    std::env::var_os("AMUX_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".amux")
                .join("state")
        })
}

fn events_path() -> PathBuf {
    state_dir().join("events.ndjson")
}

fn snapshot_path() -> PathBuf {
    state_dir().join("snapshot.json")
}

fn lock_path() -> PathBuf {
    state_dir().join(".lock")
}

struct StateLock {
    #[allow(dead_code)]
    file: File,
}

impl StateLock {
    fn acquire() -> Result<Self> {
        fs::create_dir_all(state_dir()).context("creating amux state dir")?;
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .open(lock_path())
            .context("opening amux state lock")?;
        lock_file(&file).context("locking amux state")?;
        Ok(Self { file })
    }
}

#[cfg(unix)]
fn lock_file(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn lock_file(_file: &File) -> std::io::Result<()> {
    Ok(())
}

fn read_snapshot_unlocked() -> StatusSnapshot {
    let path = snapshot_path();
    let Ok(text) = fs::read_to_string(path) else {
        return StatusSnapshot::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn write_snapshot_unlocked(snapshot: &StatusSnapshot) -> Result<()> {
    fs::create_dir_all(state_dir()).context("creating amux state dir")?;
    let final_path = snapshot_path();
    let tmp_path = final_path.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(snapshot).context("encoding amux status snapshot")?;
    fs::write(&tmp_path, json).context("writing amux status snapshot temp file")?;
    fs::rename(&tmp_path, &final_path).context("replacing amux status snapshot")?;
    Ok(())
}

pub fn record_status(
    pane_id: Option<String>,
    session: Option<String>,
    state: HookState,
    source: Option<String>,
    task_id: Option<String>,
    message: Option<String>,
) -> Result<HookStatusEvent> {
    if pane_id.as_deref().unwrap_or_default().is_empty()
        && session.as_deref().unwrap_or_default().is_empty()
    {
        bail!("hook status requires --pane or --session");
    }

    let event = HookStatusEvent {
        pane_id: pane_id.filter(|value| !value.is_empty()),
        session: session.filter(|value| !value.is_empty()),
        state,
        source: source
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "hook".to_string()),
        task_id: task_id.filter(|value| !value.is_empty()),
        message: message.filter(|value| !value.is_empty()),
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    };

    let _lock = StateLock::acquire()?;
    let mut events = OpenOptions::new()
        .create(true)
        .append(true)
        .open(events_path())
        .context("opening amux status event log")?;
    serde_json::to_writer(&mut events, &event).context("encoding amux status event")?;
    events
        .write_all(b"\n")
        .context("writing amux status event")?;

    let mut snapshot = read_snapshot_unlocked();
    snapshot.apply(event.clone());
    prune(&mut snapshot);
    write_snapshot_unlocked(&snapshot)?;
    Ok(event)
}

/// How long a status entry may sit in the snapshot.
///
/// Long enough that a session which has been quietly running for days keeps its
/// last reported state, short enough that dead sessions do not pile up: nothing
/// pruned this file before, so it had collected 27 pane entries three weeks
/// old. The freshness bound in [`current_status_since`] already stops those
/// being *believed*; this stops them being *kept*.
const RETENTION_SECS: i64 = 7 * 24 * 60 * 60;

fn prune(snapshot: &mut StatusSnapshot) {
    let cutoff = chrono::Utc::now().timestamp() - RETENTION_SECS;
    // An unparseable timestamp is dropped rather than kept forever — it can
    // never satisfy a freshness check either, so it is dead weight.
    let fresh = |event: &HookStatusEvent| {
        chrono::DateTime::parse_from_rfc3339(&event.created_at)
            .map(|at| at.timestamp() >= cutoff)
            .unwrap_or(false)
    };
    snapshot.by_pane_id.retain(|_, event| fresh(event));
    snapshot.by_session.retain(|_, event| fresh(event));
}

/// Like [`current_status`], but rejects an event older than `started_at`
/// (seconds since the Unix epoch — typically the session's creation time).
///
/// Without this bound a dead session's final event answers for its successor
/// forever. Pane ids are recycled, and amux derives session names from the
/// directory, so killing a session and starting another in the same place
/// reproduces the name exactly. Callers treat an explicit hook as authoritative
/// and let it override live inference, so a stale entry is not merely ignored —
/// it silently suppresses the correct answer. The snapshot on this machine held
/// 27 such entries, three weeks old.
pub fn current_status_since(
    pane_id: &str,
    session: &str,
    started_at: u64,
) -> Option<HookStatusEvent> {
    let event = current_status(pane_id, session)?;
    let created = chrono::DateTime::parse_from_rfc3339(&event.created_at).ok()?;
    // An unparseable timestamp is treated as stale rather than trusted: the
    // whole point here is to stop guesses from masquerading as facts.
    (created.timestamp().max(0) as u64 >= started_at).then_some(event)
}

pub fn current_status(pane_id: &str, session: &str) -> Option<HookStatusEvent> {
    let snapshot = read_snapshot_unlocked();
    if !pane_id.is_empty() {
        if let Some(event) = snapshot.by_pane_id.get(pane_id) {
            return Some(event.clone());
        }
    }
    if !session.is_empty() {
        if let Some(event) = snapshot.by_session.get(session) {
            return Some(event.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writing must also take out the trash: nothing pruned this file before,
    /// so it accumulated entries indefinitely.
    #[test]
    fn writing_drops_entries_past_the_retention_window() {
        let _home_guard = crate::test_home::lock();
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("AMUX_STATE_DIR");
        std::env::set_var("AMUX_STATE_DIR", tmp.path());

        // Hand-write a snapshot holding one ancient entry and one recent one.
        let old = HookStatusEvent {
            pane_id: Some("%0".into()),
            session: Some("dead".into()),
            state: HookState::Waiting,
            source: "claude-notification".into(),
            task_id: None,
            message: None,
            created_at: (chrono::Utc::now() - chrono::Duration::days(30))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        };
        let mut seed = StatusSnapshot::default();
        seed.apply(old);
        write_snapshot_unlocked(&seed).unwrap();
        assert_eq!(read_snapshot_unlocked().by_pane_id.len(), 1);

        // Any write prunes.
        record_status(
            Some("%9".into()),
            Some("live".into()),
            HookState::Running,
            Some("claude-prompt".into()),
            None,
            None,
        )
        .unwrap();

        let after = read_snapshot_unlocked();
        assert!(after.by_pane_id.contains_key("%9"), "fresh entry lost");
        assert!(!after.by_pane_id.contains_key("%0"), "30-day-old entry kept");
        assert!(!after.by_session.contains_key("dead"), "30-day-old session kept");

        match prev {
            Some(v) => std::env::set_var("AMUX_STATE_DIR", v),
            None => std::env::remove_var("AMUX_STATE_DIR"),
        }
    }

    /// A dead session's last event must not answer for its successor.
    ///
    /// This is the failure the whole bound exists for: callers let an explicit
    /// hook override live inference, so a stale entry does not merely go
    /// unnoticed — it suppresses the correct status indefinitely.
    #[test]
    fn an_event_older_than_the_session_is_rejected() {
        let _home_guard = crate::test_home::lock();
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("AMUX_STATE_DIR");
        std::env::set_var("AMUX_STATE_DIR", tmp.path());

        record_status(
            Some("%0".into()),
            Some("cc_proj_1a2b3c4d".into()),
            HookState::Waiting,
            Some("claude-notification".into()),
            None,
            None,
        )
        .unwrap();

        let now = chrono::Utc::now().timestamp() as u64;

        // A session that already existed when the event landed still owns it.
        assert!(current_status_since("%0", "cc_proj_1a2b3c4d", now - 60).is_some());

        // One created afterwards is a different session reusing the id — the
        // pane number is recycled, and amux rebuilds the same name for the same
        // directory, so both keys collide.
        assert!(current_status_since("%0", "cc_proj_1a2b3c4d", now + 60).is_none());

        // Unbounded lookup keeps the old behaviour, which is what made three
        // week old entries look current.
        assert!(current_status("%0", "cc_proj_1a2b3c4d").is_some());

        match prev {
            Some(v) => std::env::set_var("AMUX_STATE_DIR", v),
            None => std::env::remove_var("AMUX_STATE_DIR"),
        }
    }

    #[test]
    fn parse_accepts_common_state_names() {
        assert_eq!(HookState::parse("start").unwrap(), HookState::Running);
        assert_eq!(HookState::parse("completed").unwrap(), HookState::Done);
        assert!(HookState::parse("unknown").is_err());
    }

    #[test]
    fn record_and_read_status_by_pane_and_session() {
        let _guard = TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("AMUX_STATE_DIR", tmp.path());

        let event = record_status(
            Some("%1".to_string()),
            Some("cc-demo_proj_deadbeef".to_string()),
            HookState::Done,
            Some("test".to_string()),
            Some("t1".to_string()),
            Some("ok".to_string()),
        )
        .unwrap();

        assert_eq!(event.state, HookState::Done);
        assert_eq!(current_status("%1", "").unwrap().state, HookState::Done);
        assert_eq!(
            current_status("", "cc-demo_proj_deadbeef").unwrap().state,
            HookState::Done
        );

        std::env::remove_var("AMUX_STATE_DIR");
    }
}
