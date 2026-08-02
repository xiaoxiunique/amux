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
    write_snapshot_unlocked(&snapshot)?;
    Ok(event)
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
