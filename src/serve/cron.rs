//! Read-only bridge to [CronBox](https://github.com/xiaoxiunique/cronbox) so the
//! phone client can inspect and manage scheduled jobs through `amux serve`.
//!
//! **Reads go to SQLite, writes go through the CLI.** The CLI has no JSON
//! output (only TSV and `key: value` text), so parsing it would be brittle;
//! the database is a stable, typed interface and amux already links rusqlite
//! for CC Switch. Mutations, though, must go through `cronbox` itself — it
//! owns the running scheduler's in-memory state, so writing rows behind its
//! back would not take effect until a restart.
//!
//! The database lives at `~/Library/Application Support/com.cronbox.app/`.
//! Note the `com.cronbox.app` bundle id: an older `cronbox/` directory may
//! also exist with an empty, out-of-date schema — this module deliberately
//! does not fall back to it.

use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use std::path::PathBuf;
use std::process::Command;

/// Cap on rows returned by `jobs`. There are thousands of historical runs
/// (8383 on the development machine), so an unbounded query would be a
/// mistake on a phone connection.
const MAX_LIMIT: usize = 200;
const DEFAULT_LIMIT: usize = 50;

/// Logs are excluded from list responses on purpose: they total ~19MB across
/// all jobs and a single run can hold 60KB. `job_log` fetches one on demand.
const MAX_LOG_BYTES: usize = 200_000;

fn db_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let p = home
        .join("Library/Application Support/com.cronbox.app/cronbox.db");
    p.exists().then_some(p)
}

/// Open the CronBox database read-only.
///
/// The daemon keeps it in WAL mode, so a reader never blocks it and always
/// sees a consistent snapshot.
fn open() -> Result<Connection, String> {
    let path = db_path().ok_or_else(|| {
        "CronBox database not found (is cronbox installed?)".to_string()
    })?;
    Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| format!("opening cronbox db: {e}"))
}

/// True when CronBox is present on this machine.
pub fn available() -> bool {
    db_path().is_some()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
    pub id: String,
    /// Absolute path, joined from `base_dir` + `script_path` — the database
    /// stores them separately but every consumer wants the whole thing.
    pub script: String,
    pub cron: String,
    pub timezone: String,
    pub enabled: bool,
    pub next_run_at: Option<String>,
    pub last_run_at: Option<String>,
    pub one_shot: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub id: String,
    pub schedule_id: Option<String>,
    pub script: String,
    /// `success` / `failure` / `running` / `queued` / `cancelled` / `skipped`.
    pub status: String,
    pub error: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub duration_ms: Option<i64>,
    pub created_at: String,
}

fn join_script(base_dir: Option<String>, script_path: String) -> String {
    match base_dir {
        Some(base) if !base.is_empty() => PathBuf::from(base)
            .join(&script_path)
            .to_string_lossy()
            .into_owned(),
        _ => script_path,
    }
}

pub fn schedules() -> Result<Vec<Schedule>, String> {
    let conn = open()?;
    let mut stmt = conn
        .prepare(
            "SELECT id, base_dir, script_path, cron_expr, timezone, enabled,
                    next_run_at, last_run_at, one_shot
             FROM schedules
             ORDER BY enabled DESC, next_run_at IS NULL, next_run_at",
        )
        .map_err(|e| format!("preparing schedules query: {e}"))?;

    let rows = stmt
        .query_map([], |r| {
            Ok(Schedule {
                id: r.get(0)?,
                script: join_script(r.get(1).ok(), r.get(2)?),
                cron: r.get(3)?,
                timezone: r.get(4)?,
                enabled: r.get::<_, i64>(5)? != 0,
                next_run_at: r.get(6).ok(),
                last_run_at: r.get(7).ok(),
                one_shot: r.get::<_, i64>(8).unwrap_or(0) != 0,
            })
        })
        .map_err(|e| format!("querying schedules: {e}"))?;

    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("reading schedules: {e}"))
}

/// Recent jobs, newest first. `status` filters to one state; `schedule_id`
/// narrows to a single schedule's history.
pub fn jobs(
    limit: Option<usize>,
    status: Option<&str>,
    schedule_id: Option<&str>,
) -> Result<Vec<Job>, String> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let conn = open()?;

    // Built by cases rather than string concatenation so every variant stays
    // a static SQL string with bound parameters.
    let base = "SELECT id, schedule_id, base_dir, script_path, status, error,
                       started_at, completed_at, duration_ms, created_at
                FROM jobs";
    let map = |r: &rusqlite::Row| -> rusqlite::Result<Job> {
        Ok(Job {
            id: r.get(0)?,
            schedule_id: r.get(1).ok(),
            script: join_script(r.get(2).ok(), r.get(3)?),
            status: r.get(4)?,
            error: r.get(5).ok(),
            started_at: r.get(6).ok(),
            completed_at: r.get(7).ok(),
            duration_ms: r.get(8).ok(),
            created_at: r.get(9)?,
        })
    };

    let out = match (status, schedule_id) {
        (Some(s), Some(sid)) => {
            let sql = format!("{base} WHERE status = ?1 AND schedule_id = ?2 ORDER BY created_at DESC LIMIT ?3");
            let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(rusqlite::params![s, sid, limit as i64], map)
                .map_err(|e| e.to_string())?;
            rows.collect::<Result<Vec<_>, _>>()
        }
        (Some(s), None) => {
            let sql = format!("{base} WHERE status = ?1 ORDER BY created_at DESC LIMIT ?2");
            let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(rusqlite::params![s, limit as i64], map)
                .map_err(|e| e.to_string())?;
            rows.collect::<Result<Vec<_>, _>>()
        }
        (None, Some(sid)) => {
            let sql = format!("{base} WHERE schedule_id = ?1 ORDER BY created_at DESC LIMIT ?2");
            let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(rusqlite::params![sid, limit as i64], map)
                .map_err(|e| e.to_string())?;
            rows.collect::<Result<Vec<_>, _>>()
        }
        (None, None) => {
            let sql = format!("{base} ORDER BY created_at DESC LIMIT ?1");
            let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(rusqlite::params![limit as i64], map)
                .map_err(|e| e.to_string())?;
            rows.collect::<Result<Vec<_>, _>>()
        }
    };
    out.map_err(|e| format!("reading jobs: {e}"))
}

/// Jobs the scheduler considers in flight.
pub fn running() -> Result<Vec<Job>, String> {
    let mut out = jobs(Some(MAX_LIMIT), Some("running"), None)?;
    out.extend(jobs(Some(MAX_LIMIT), Some("queued"), None)?);
    Ok(out)
}

/// One job's captured output. Truncated from the front so the tail — where
/// a failure's cause usually is — always survives.
pub fn job_log(id_prefix: &str) -> Result<(String, String), String> {
    let conn = open()?;
    let pattern = format!("{id_prefix}%");
    let mut stmt = conn
        .prepare("SELECT id, logs FROM jobs WHERE id LIKE ?1 ORDER BY created_at DESC LIMIT 1")
        .map_err(|e| format!("preparing log query: {e}"))?;
    let mut rows = stmt
        .query(rusqlite::params![pattern])
        .map_err(|e| format!("querying log: {e}"))?;
    let row = rows
        .next()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no job matching {id_prefix}"))?;

    let id: String = row.get(0).map_err(|e| e.to_string())?;
    let logs: String = row.get(1).unwrap_or_default();
    let logs = if logs.len() > MAX_LOG_BYTES {
        let cut = logs.len() - MAX_LOG_BYTES;
        // Land on a char boundary so the string stays valid UTF-8.
        let start = (cut..logs.len())
            .find(|i| logs.is_char_boundary(*i))
            .unwrap_or(logs.len());
        format!("…(truncated)\n{}", &logs[start..])
    } else {
        logs
    };
    Ok((id, logs))
}

// ---------------------------------------------------------------- mutations

fn cronbox_bin() -> String {
    std::env::var("AMUX_CRONBOX")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "cronbox".to_string())
}

/// Run a cronbox subcommand. Mutations go through the CLI so the running
/// daemon updates its in-memory schedule state; writing to SQLite directly
/// would not take effect until it restarted.
fn run_cli(args: &[&str]) -> Result<String, String> {
    let out = Command::new(cronbox_bin())
        .args(args)
        .output()
        .map_err(|e| format!("running `{}`: {e}", cronbox_bin()))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if stderr.is_empty() { stdout } else { stderr });
    }
    Ok(stdout)
}

pub fn enable(id: &str) -> Result<String, String> {
    run_cli(&["schedules", "enable", id])
}

pub fn disable(id: &str) -> Result<String, String> {
    run_cli(&["schedules", "disable", id])
}

pub fn cancel(id: &str) -> Result<String, String> {
    run_cli(&["jobs", "cancel", id])
}

/// Trigger a schedule's script once, now.
///
/// Runs detached: some scripts take minutes (one on the development machine
/// runs for 536s), which would otherwise hold the HTTP request open until it
/// timed out. The new job shows up in `jobs` as it progresses.
pub fn trigger(schedule_id: &str) -> Result<String, String> {
    let conn = open()?;
    let pattern = format!("{schedule_id}%");
    let (base_dir, script_path): (Option<String>, String) = conn
        .query_row(
            "SELECT base_dir, script_path FROM schedules WHERE id LIKE ?1 LIMIT 1",
            rusqlite::params![pattern],
            |r| Ok((r.get(0).ok(), r.get(1)?)),
        )
        .map_err(|_| format!("no schedule matching {schedule_id}"))?;
    let base = base_dir.ok_or_else(|| "schedule has no base_dir".to_string())?;

    Command::new(cronbox_bin())
        .args(["run", &base, &script_path])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("spawning `{} run`: {e}", cronbox_bin()))?;
    Ok(format!("triggered {script_path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_path_joins_base_dir() {
        assert_eq!(
            join_script(Some("/base".into()), "s.sh".into()),
            "/base/s.sh"
        );
        // Absent or empty base_dir leaves the stored path untouched.
        assert_eq!(join_script(None, "/abs/s.sh".into()), "/abs/s.sh");
        assert_eq!(join_script(Some(String::new()), "s.sh".into()), "s.sh");
    }

    #[test]
    fn limit_is_clamped() {
        // Mirrors the clamp in jobs(): guards against a client asking for all
        // 8000+ rows, and against a nonsensical 0.
        assert_eq!(Some(9999usize).unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT), MAX_LIMIT);
        assert_eq!(Some(0usize).unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT), 1);
        assert_eq!(None::<usize>.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT), DEFAULT_LIMIT);
    }

    #[test]
    fn oversized_logs_keep_the_tail_on_a_char_boundary() {
        // A failure's cause is at the end, so truncation must drop the head.
        // Multi-byte chars must not be split.
        let logs = "错".repeat(MAX_LOG_BYTES); // 3 bytes each
        assert!(logs.len() > MAX_LOG_BYTES);
        let cut = logs.len() - MAX_LOG_BYTES;
        let start = (cut..logs.len())
            .find(|i| logs.is_char_boundary(*i))
            .unwrap();
        let tail = &logs[start..]; // must not panic
        assert!(tail.len() <= MAX_LOG_BYTES);
        assert!(tail.starts_with('错'));
    }

    #[test]
    fn available_does_not_panic_without_cronbox() {
        // Just exercises the path-probe; result depends on the machine.
        let _ = available();
    }
}
