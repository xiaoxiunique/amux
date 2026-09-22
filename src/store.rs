//! One SQLite database for amux's own project and session metadata.
//!
//! Before this existed the same information lived in three JSON files across
//! two directories — `project-history.json`, `session-labels.json` and
//! `session-ids.json` — none of which knew about the others even though all
//! three are keyed by the same things (a directory path, a multiplexer session
//! name). That cost more than tidiness:
//!
//!   - `session-ids.json` was a whole-file read-modify-write with no lock and
//!     no atomic replace, written by three separate processes (the CLI on
//!     launch, the daemon on resume, the TUI on relaunch). Concurrent writes
//!     lost each other.
//!   - `project-history.json` was rewritten in full on every snapshot poll,
//!     i.e. every 2.5 seconds, to update one row.
//!
//! Deliberately *not* stored here: the status snapshot in `~/.amux/state/`,
//! which is the one store that already gets concurrency right (flock plus
//! temp-file-and-rename), and agent transcripts, which belong to the agents and
//! run to gigabytes.

use rusqlite::Connection;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

/// Database location. `AMUX_DB_PATH` overrides it outright, which is how tests
/// get an isolated database without touching the real home directory.
fn db_path() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("AMUX_DB_PATH") {
        if !explicit.is_empty() {
            return Some(PathBuf::from(explicit));
        }
    }
    default_db_path()
}

fn default_db_path() -> Option<PathBuf> {
    let dir = dirs::home_dir()?.join(".amux");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("amux.db"))
}

/// The open connection, together with the path it was opened for.
///
/// Keeping the path lets a changed `AMUX_DB_PATH` reopen rather than silently
/// keep serving the previous database — without that, tests would contaminate
/// each other through this cache.
static DB: LazyLock<Mutex<Option<(PathBuf, Connection)>>> = LazyLock::new(|| Mutex::new(None));

/// Run `f` against the database, or return `None` if it cannot be opened.
///
/// Every accessor degrades to `None` rather than failing: a broken database
/// must not stop amux from launching an agent, the same way `push.rs` keeps
/// serving when its database is unavailable.
fn with_db<T>(f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Option<T> {
    let path = db_path()?;
    let mut guard = match DB.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.as_ref().map(|(p, _)| p != &path).unwrap_or(true) {
        *guard = open(&path).map(|conn| (path.clone(), conn));
    }
    let (_, conn) = guard.as_ref()?;
    f(conn).ok()
}

fn open(path: &std::path::Path) -> Option<Connection> {
    let conn = Connection::open(path).ok()?;
    // WAL is what makes the short-lived CLI processes and the long-lived
    // daemon able to write concurrently — the property the JSON files lacked.
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS projects (
            path         TEXT PRIMARY KEY,
            name         TEXT NOT NULL,
            alias        TEXT,
            last_agent   TEXT NOT NULL DEFAULT '',
            last_seen_at TEXT NOT NULL DEFAULT '',
            launch_count INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS sessions (
            name            TEXT PRIMARY KEY,
            project_path    TEXT,
            agent           TEXT,
            label           TEXT,
            conversation_id TEXT,
            updated_at      TEXT NOT NULL DEFAULT ''
        );
        CREATE TABLE IF NOT EXISTS settings (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS summaries (
            transcript   TEXT PRIMARY KEY,
            agent        TEXT NOT NULL,
            summary      TEXT,
            source_mtime REAL NOT NULL,
            updated_at   TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS auto (
            session       TEXT PRIMARY KEY,
            goal          TEXT NOT NULL DEFAULT '',
            max_turns     INTEGER NOT NULL DEFAULT 10,
            used          INTEGER NOT NULL DEFAULT 0,
            allow_waiting INTEGER NOT NULL DEFAULT 0,
            enabled       INTEGER NOT NULL DEFAULT 0,
            updated_at    TEXT NOT NULL DEFAULT ''
        );
        CREATE TABLE IF NOT EXISTS timer (
            session     TEXT PRIMARY KEY,
            prompt      TEXT NOT NULL DEFAULT '',
            every_secs  INTEGER NOT NULL DEFAULT 1800,
            last_run_at TEXT NOT NULL DEFAULT '',
            skipped     INTEGER NOT NULL DEFAULT 0,
            enabled     INTEGER NOT NULL DEFAULT 0,
            updated_at  TEXT NOT NULL DEFAULT ''
        );
        CREATE TABLE IF NOT EXISTS activity (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            source     TEXT NOT NULL,
            session    TEXT NOT NULL,
            project    TEXT NOT NULL DEFAULT '',
            kind       TEXT NOT NULL,
            turn       INTEGER,
            max_turns  INTEGER,
            message    TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS activity_session_time
            ON activity (source, session, id DESC);",
    )
    .ok()?;
    // Only the database at its real location may import — and rename — the
    // files in the real home directory. A test pointing `AMUX_DB_PATH` at a
    // temporary file must never reach into `~/.amux`; it would migrate the
    // user's actual data into a throwaway database and rename the originals
    // out from under them. Tests drive [`migrate_from`] explicitly instead.
    if Some(path.to_path_buf()) == default_db_path() {
        if let Some(home) = dirs::home_dir() {
            migrate_from(&conn, &home);
        }
    }
    Some(conn)
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// One row of `projects`, mapped by callers into whatever shape they expose.
///
/// Kept free of serde on purpose: `/api/project-history` used to serialize its
/// on-disk struct straight to the wire, which is exactly why the storage format
/// could not be changed without changing the API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRow {
    pub path: String,
    pub name: String,
    pub alias: Option<String>,
    pub last_agent: String,
    pub last_seen_at: String,
    pub launch_count: u32,
}

pub fn projects() -> Vec<ProjectRow> {
    with_db(|conn| {
        let mut stmt = conn.prepare(
            "SELECT path, name, alias, last_agent, last_seen_at, launch_count FROM projects",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ProjectRow {
                path: row.get(0)?,
                name: row.get(1)?,
                alias: row.get(2)?,
                last_agent: row.get(3)?,
                last_seen_at: row.get(4)?,
                launch_count: row.get::<_, i64>(5)?.max(0) as u32,
            })
        })?;
        Ok(rows.flatten().collect())
    })
    .unwrap_or_default()
}

/// Insert or update one project, leaving any alias the user has set alone.
pub fn upsert_project(
    path: &str,
    name: &str,
    last_agent: &str,
    last_seen_at: &str,
    launch_count: u32,
) {
    with_db(|conn| {
        conn.execute(
            "INSERT INTO projects (path, name, alias, last_agent, last_seen_at, launch_count)
             VALUES (?1, ?2, NULL, ?3, ?4, ?5)
             ON CONFLICT(path) DO UPDATE SET
                name = excluded.name,
                last_agent = excluded.last_agent,
                last_seen_at = excluded.last_seen_at,
                launch_count = excluded.launch_count",
            rusqlite::params![path, name, last_agent, last_seen_at, launch_count],
        )?;
        Ok(())
    });
}

/// Make `projects` hold exactly these rows.
///
/// The caller owns the list — including the 80-entry cap that project history
/// has always applied — so anything absent is dropped, matching the old
/// behaviour of replacing the whole file. Aliases survive an update but not an
/// eviction, which is the same bargain the file made with every other field.
pub fn replace_projects(rows: &[ProjectRow]) {
    with_db(|conn| {
        let tx = conn.unchecked_transaction()?;
        for row in rows {
            tx.execute(
                "INSERT INTO projects (path, name, alias, last_agent, last_seen_at, launch_count)
                 VALUES (?1, ?2, NULL, ?3, ?4, ?5)
                 ON CONFLICT(path) DO UPDATE SET
                    name = excluded.name,
                    last_agent = excluded.last_agent,
                    last_seen_at = excluded.last_seen_at,
                    launch_count = excluded.launch_count",
                rusqlite::params![
                    row.path,
                    row.name,
                    row.last_agent,
                    row.last_seen_at,
                    row.launch_count
                ],
            )?;
        }
        // Rebuilding the keep-set as a temp table avoids an IN clause whose
        // length grows with the project count.
        tx.execute(
            "CREATE TEMP TABLE IF NOT EXISTS keep (path TEXT PRIMARY KEY)",
            [],
        )?;
        tx.execute("DELETE FROM keep", [])?;
        for row in rows {
            tx.execute("INSERT OR IGNORE INTO keep (path) VALUES (?1)", [&row.path])?;
        }
        // A name is an explicit choice, not an observation, so it does not
        // expire with the 80-entry history cap: reach project eighty-one and a
        // directory you deliberately named would otherwise lose that name with
        // no warning. The row stays; the API response is still capped, because
        // the cap is applied when reading.
        tx.execute(
            "DELETE FROM projects WHERE path NOT IN (SELECT path FROM keep) AND alias IS NULL",
            [],
        )?;
        tx.commit()?;
        Ok(())
    });
}

/// Name a project. An empty alias clears it.
pub fn set_alias(path: &str, alias: &str) {
    let alias = alias.trim();
    let value = (!alias.is_empty()).then_some(alias);
    with_db(|conn| {
        // The project may not have been seen yet, so this has to be able to
        // create the row — naming a directory before launching anything in it
        // is a reasonable thing to do.
        conn.execute(
            "INSERT INTO projects (path, name, alias, last_seen_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(path) DO UPDATE SET alias = excluded.alias",
            rusqlite::params![path, basename(path), value, now()],
        )?;
        Ok(())
    });
}

fn basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

pub fn labels() -> BTreeMap<String, String> {
    with_db(|conn| {
        let mut stmt = conn
            .prepare("SELECT name, label FROM sessions WHERE label IS NOT NULL AND label <> ''")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        Ok(rows.flatten().collect())
    })
    .unwrap_or_default()
}

/// Set or clear one session's label. An empty label removes it.
pub fn set_label(session: &str, label: &str) {
    let label = label.trim();
    let value = (!label.is_empty()).then_some(label);
    with_db(|conn| {
        conn.execute(
            "INSERT INTO sessions (name, label, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET label = excluded.label, updated_at = excluded.updated_at",
            rusqlite::params![session, value, now()],
        )?;
        Ok(())
    });
}

pub fn conversation_id(session: &str) -> Option<String> {
    with_db(|conn| {
        conn.query_row(
            "SELECT conversation_id FROM sessions WHERE name = ?1",
            [session],
            |row| row.get::<_, Option<String>>(0),
        )
    })
    .flatten()
    .filter(|id| !id.is_empty())
}

/// Every recorded session → conversation mapping.
///
/// Used to work out which conversations are already claimed, so a second
/// session in one directory does not adopt its neighbour's thread.
pub fn conversation_ids() -> BTreeMap<String, String> {
    with_db(|conn| {
        let mut stmt = conn.prepare(
            "SELECT name, conversation_id FROM sessions
             WHERE conversation_id IS NOT NULL AND conversation_id <> ''",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        Ok(rows.flatten().collect())
    })
    .unwrap_or_default()
}

pub fn set_conversation_id(session: &str, id: &str) {
    with_db(|conn| {
        conn.execute(
            "INSERT INTO sessions (name, conversation_id, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET
                conversation_id = excluded.conversation_id,
                updated_at = excluded.updated_at",
            rusqlite::params![session, id, now()],
        )?;
        Ok(())
    });
}

/// A cached summary for `transcript`, valid only while its mtime is unchanged.
///
/// The outer `Option` distinguishes "nothing cached" from "cached, and the
/// answer is that this transcript has no summary" — the latter is worth
/// remembering, since recomputing it means another full scan.
pub fn cached_summary(transcript: &str, mtime: f64) -> Option<Option<String>> {
    with_db(|conn| {
        conn.query_row(
            "SELECT summary FROM summaries WHERE transcript = ?1 AND source_mtime = ?2",
            rusqlite::params![transcript, mtime],
            |row| row.get::<_, Option<String>>(0),
        )
    })
}

pub fn put_summary(transcript: &str, agent: &str, summary: Option<&str>, mtime: f64) {
    with_db(|conn| {
        conn.execute(
            "INSERT INTO summaries (transcript, agent, summary, source_mtime, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(transcript) DO UPDATE SET
                agent = excluded.agent,
                summary = excluded.summary,
                source_mtime = excluded.source_mtime,
                updated_at = excluded.updated_at",
            rusqlite::params![transcript, agent, summary, mtime, now()],
        )?;
        Ok(())
    });
}

/// A remembered preference, by key.
pub fn setting(key: &str) -> Option<String> {
    with_db(|conn| {
        conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
            row.get::<_, String>(0)
        })
    })
    .filter(|v| !v.is_empty())
}

pub fn set_setting(key: &str, value: &str) {
    with_db(|conn| {
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![key, value],
        )?;
        Ok(())
    });
}

// --------------------------------------------------------------- auto mode

/// Auto mode: keep a stopped agent going until its goal is met or the turn
/// budget runs out. One row per multiplexer session, keyed by session name
/// because that is what the daemon's pane snapshots carry.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoConfig {
    pub session: String,
    pub goal: String,
    pub max_turns: u32,
    pub used: u32,
    pub allow_waiting: bool,
    pub enabled: bool,
}

fn auto_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AutoConfig> {
    Ok(AutoConfig {
        session: row.get(0)?,
        goal: row.get(1)?,
        max_turns: row.get(2)?,
        used: row.get(3)?,
        allow_waiting: row.get::<_, i64>(4)? != 0,
        enabled: row.get::<_, i64>(5)? != 0,
    })
}

pub fn auto_get(session: &str) -> Option<AutoConfig> {
    with_db(|conn| {
        conn.query_row(
            "SELECT session, goal, max_turns, used, allow_waiting, enabled
             FROM auto WHERE session = ?1",
            [session],
            auto_row,
        )
    })
}

pub fn auto_list() -> Vec<AutoConfig> {
    with_db(|conn| {
        let mut stmt = conn.prepare(
            "SELECT session, goal, max_turns, used, allow_waiting, enabled
             FROM auto ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], auto_row)?;
        rows.collect()
    })
    .unwrap_or_default()
}

/// Arm auto mode, resetting the turn counter so a re-armed goal starts fresh.
/// Returns false when the database could not be opened (nothing was written).
pub fn auto_enable(session: &str, goal: &str, max_turns: u32, allow_waiting: bool) -> bool {
    // The other half of the rule in `timer_enable`: one session, one thing
    // typing into it.
    timer_disable(session);
    with_db(|conn| {
        conn.execute(
            "INSERT INTO auto (session, goal, max_turns, used, allow_waiting, enabled, updated_at)
             VALUES (?1, ?2, ?3, 0, ?4, 1, ?5)
             ON CONFLICT(session) DO UPDATE SET
                goal = excluded.goal,
                max_turns = excluded.max_turns,
                used = 0,
                allow_waiting = excluded.allow_waiting,
                enabled = 1,
                updated_at = excluded.updated_at",
            rusqlite::params![session, goal, max_turns, allow_waiting, now()],
        )?;
        Ok(())
    })
    .is_some()
}

/// Disarm without forgetting the goal — re-enabling resumes the same budget.
/// A session's schedule: what to send, and how often.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimerConfig {
    pub session: String,
    pub prompt: String,
    pub every_secs: u32,
    /// When the prompt was last actually sent — not when it was next due.
    ///
    /// The difference matters after a skip: counting from the last real send
    /// means a busy agent costs you one run, where counting from the due time
    /// would fire the moment it went quiet and again immediately after.
    pub last_run_at: String,
    /// How many runs were skipped because the agent was working. Shown, not
    /// acted on.
    pub skipped: u32,
    pub enabled: bool,
}

fn timer_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TimerConfig> {
    Ok(TimerConfig {
        session: row.get(0)?,
        prompt: row.get(1)?,
        every_secs: row.get(2)?,
        last_run_at: row.get(3)?,
        skipped: row.get(4)?,
        enabled: row.get::<_, i64>(5)? != 0,
    })
}

pub fn timer_get(session: &str) -> Option<TimerConfig> {
    with_db(|conn| {
        let mut stmt = conn.prepare(
            "SELECT session, prompt, every_secs, last_run_at, skipped, enabled
             FROM timer WHERE session = ?1",
        )?;
        let mut rows = stmt.query_map([session], timer_row)?;
        Ok(rows.next().transpose()?)
    })
    .flatten()
}

/// Put a session on a schedule.
///
/// Turns auto off for it in the same breath: both paste into the same pane, and
/// two of them typing at one agent interrupt each other. The first send waits a
/// full interval rather than going out now — arming a schedule should not be a
/// way to send something immediately by accident.
pub fn timer_enable(session: &str, prompt: &str, every_secs: u32) -> bool {
    auto_disable(session);
    with_db(|conn| {
        conn.execute(
            "INSERT INTO timer (session, prompt, every_secs, last_run_at, skipped, enabled, updated_at)
             VALUES (?1, ?2, ?3, ?4, 0, 1, ?4)
             ON CONFLICT(session) DO UPDATE SET
                prompt = excluded.prompt,
                every_secs = excluded.every_secs,
                last_run_at = excluded.last_run_at,
                skipped = 0,
                enabled = 1,
                updated_at = excluded.updated_at",
            rusqlite::params![session, prompt, every_secs, now()],
        )?;
        Ok(())
    })
    .is_some()
}

pub fn timer_disable(session: &str) {
    with_db(|conn| {
        conn.execute(
            "UPDATE timer SET enabled = 0, updated_at = ?2 WHERE session = ?1",
            rusqlite::params![session, now()],
        )?;
        Ok(())
    });
}

/// Record that the prompt went out, which is what the next interval counts from.
pub fn timer_mark_run(session: &str) {
    with_db(|conn| {
        conn.execute(
            "UPDATE timer SET last_run_at = ?2, updated_at = ?2 WHERE session = ?1",
            rusqlite::params![session, now()],
        )?;
        Ok(())
    });
}

/// Record that a due run was passed over because the agent was working.
///
/// Deliberately leaves `last_run_at` alone: the run is lost, not deferred.
pub fn timer_mark_skipped(session: &str) {
    with_db(|conn| {
        conn.execute(
            "UPDATE timer SET skipped = skipped + 1, updated_at = ?2 WHERE session = ?1",
            rusqlite::params![session, now()],
        )?;
        Ok(())
    });
}

/// Every session currently on a schedule, for the tree's marker.
/// Every armed schedule, by session.
///
/// One query for a whole snapshot. The alternative — asking per pane while
/// building the list — is forty round trips every two and a half seconds to
/// answer a question about at most a handful of sessions.
pub fn timers_enabled() -> std::collections::BTreeMap<String, TimerConfig> {
    with_db(|conn| {
        let mut stmt = conn.prepare(
            "SELECT session, prompt, every_secs, last_run_at, skipped, enabled
             FROM timer WHERE enabled = 1",
        )?;
        let rows = stmt.query_map([], timer_row)?;
        Ok(rows
            .filter_map(Result::ok)
            .map(|c| (c.session.clone(), c))
            .collect())
    })
    .unwrap_or_default()
}

pub fn timer_enabled_sessions() -> std::collections::BTreeSet<String> {
    with_db(|conn| {
        let mut stmt = conn.prepare("SELECT session FROM timer WHERE enabled = 1")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        Ok(rows.filter_map(Result::ok).collect())
    })
    .unwrap_or_default()
}

pub fn auto_disable(session: &str) {
    with_db(|conn| {
        conn.execute(
            "UPDATE auto SET enabled = 0, updated_at = ?2 WHERE session = ?1",
            rusqlite::params![session, now()],
        )?;
        Ok(())
    });
}

/// Count one delivered continuation; returns the new total.
pub fn auto_bump(session: &str) -> u32 {
    with_db(|conn| {
        conn.execute(
            "UPDATE auto SET used = used + 1, updated_at = ?2 WHERE session = ?1",
            rusqlite::params![session, now()],
        )?;
        conn.query_row(
            "SELECT used FROM auto WHERE session = ?1",
            [session],
            |row| row.get(0),
        )
    })
    .unwrap_or(0)
}

// ------------------------------------------------------------------ activity

/// One line of run history: what auto mode or a schedule did to a session.
///
/// Kept in the database rather than only on the daemon's stderr, because that
/// log is truncated on every restart and cannot be filtered or audited per
/// project. The daemon still prints the same lines for live watching.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityEntry {
    pub id: i64,
    /// `auto` or `timer`.
    pub source: String,
    pub session: String,
    /// The pane's working directory, so history can be grouped by project.
    pub project: String,
    /// `continued` | `stopped` | `unavailable` | `disabled` | `failed` |
    /// `sent` | `skipped`.
    pub kind: String,
    /// Continuations sent so far, when the source counts them.
    pub turn: Option<u32>,
    pub max_turns: Option<u32>,
    pub message: String,
    pub created_at: String,
}

/// Rows kept per (source, session).
///
/// The old stderr log grew without bound — 1545 lines of the same model failure
/// in one night — so the database caps what it keeps rather than trusting every
/// caller to.
const ACTIVITY_KEEP_PER_SESSION: usize = 500;

/// Record one run-history line, trimming that session's older rows.
pub fn activity_append(
    source: &str,
    session: &str,
    project: &str,
    kind: &str,
    turn: Option<u32>,
    max_turns: Option<u32>,
    message: &str,
) {
    with_db(|conn| {
        conn.execute(
            "INSERT INTO activity
                (source, session, project, kind, turn, max_turns, message, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![source, session, project, kind, turn, max_turns, message, now()],
        )?;
        // Trim on write, keyed on id rather than created_at: rows written in
        // the same millisecond order the same either way.
        conn.execute(
            "DELETE FROM activity
             WHERE source = ?1 AND session = ?2
               AND id NOT IN (
                   SELECT id FROM activity WHERE source = ?1 AND session = ?2
                   ORDER BY id DESC LIMIT ?3
               )",
            rusqlite::params![source, session, ACTIVITY_KEEP_PER_SESSION as i64],
        )?;
        Ok(())
    });
}

/// Recent run history, newest first.
///
/// `source` and `session` narrow the result when given; otherwise the whole
/// table is fair game, which is what the per-project audit view wants.
pub fn activity_list(
    source: Option<&str>,
    session: Option<&str>,
    limit: usize,
) -> Vec<ActivityEntry> {
    let limit = limit.clamp(1, 1000) as i64;
    with_db(|conn| {
        let mut stmt = conn.prepare(
            "SELECT id, source, session, project, kind, turn, max_turns, message, created_at
             FROM activity
             WHERE (?1 IS NULL OR source = ?1)
               AND (?2 IS NULL OR session = ?2)
             ORDER BY id DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(rusqlite::params![source, session, limit], |row| {
            Ok(ActivityEntry {
                id: row.get(0)?,
                source: row.get(1)?,
                session: row.get(2)?,
                project: row.get(3)?,
                kind: row.get(4)?,
                turn: row.get::<_, Option<i64>>(5)?.map(|v| v.max(0) as u32),
                max_turns: row.get::<_, Option<i64>>(6)?.map(|v| v.max(0) as u32),
                message: row.get(7)?,
                created_at: row.get(8)?,
            })
        })?;
        rows.collect()
    })
    .unwrap_or_default()
}

// ---------------------------------------------------------------- migration

/// Import the three JSON stores this database replaces, once.
///
/// Guarded on both tables being empty rather than on a marker file: a marker
/// can be deleted while the data is still there, and re-importing would then
/// overwrite newer rows with stale JSON. Each source file is renamed to
/// `*.migrated` afterwards so nothing can read a stale copy — renamed, not
/// deleted, so the import can be inspected or undone by hand.
fn migrate_from(conn: &Connection, home: &std::path::Path) {
    let already: i64 = conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM projects) + (SELECT COUNT(*) FROM sessions)",
            [],
            |row| row.get(0),
        )
        .unwrap_or(1);
    if already > 0 {
        return;
    }

    for (path, name) in [
        (home.join(".agent-monitor/project-history.json"), "projects"),
        (home.join(".amux/session-labels.json"), "labels"),
        (home.join(".amux/session-ids.json"), "ids"),
    ] {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let imported = match name {
            "projects" => import_projects(conn, &text),
            "labels" => import_map(conn, &text, "label"),
            _ => import_map(conn, &text, "conversation_id"),
        };
        if imported {
            let _ = std::fs::rename(&path, path.with_extension("json.migrated"));
        }
    }
}

fn import_projects(conn: &Connection, text: &str) -> bool {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Entry {
        path: String,
        name: String,
        #[serde(default)]
        last_agent: String,
        #[serde(default)]
        last_seen_at: String,
        #[serde(default)]
        launch_count: u32,
    }
    let Ok(entries) = serde_json::from_str::<Vec<Entry>>(text) else {
        return false;
    };
    for e in entries {
        let _ = conn.execute(
            "INSERT OR REPLACE INTO projects
                (path, name, alias, last_agent, last_seen_at, launch_count)
             VALUES (?1, ?2, NULL, ?3, ?4, ?5)",
            rusqlite::params![e.path, e.name, e.last_agent, e.last_seen_at, e.launch_count],
        );
    }
    true
}

/// Import a `{session: value}` map into one column of `sessions`.
fn import_map(conn: &Connection, text: &str, column: &str) -> bool {
    let Ok(map) = serde_json::from_str::<BTreeMap<String, String>>(text) else {
        return false;
    };
    let sql = format!(
        "INSERT INTO sessions (name, {column}, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(name) DO UPDATE SET {column} = excluded.{column}"
    );
    let stamp = now();
    for (session, value) in map {
        let _ = conn.execute(&sql, rusqlite::params![session, value, stamp]);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Open an isolated database and return its path.
    fn scratch(dir: &std::path::Path) -> PathBuf {
        let path = dir.join("amux.db");
        std::env::set_var("AMUX_DB_PATH", &path);
        path
    }

    /// A schedule round-trips, and arming one puts auto away.
    ///
    /// The exclusion is the part worth pinning down: both of them paste into
    /// the same pane, and two things typing at one agent interrupt each other
    /// in ways that are hard to see afterwards.
    #[test]
    fn a_schedule_replaces_auto_on_the_same_session() {
        let _guard = crate::test_home::lock();
        let tmp = tempfile::tempdir().unwrap();
        scratch(tmp.path());
        let s = "oc_proj_1a2b3c4d";

        assert!(timer_get(s).is_none());
        assert!(auto_enable(s, "ship it", 5, false));
        assert!(auto_get(s).unwrap().enabled);

        assert!(timer_enable(s, "看一下 CI", 900));
        let cfg = timer_get(s).unwrap();
        assert_eq!(cfg.prompt, "看一下 CI");
        assert_eq!(cfg.every_secs, 900);
        assert!(cfg.enabled);
        assert_eq!(cfg.skipped, 0);
        // The first run is a full interval away: arming must not be a way to
        // send something right now by accident.
        assert!(!cfg.last_run_at.is_empty(), "arming stamps the clock");
        assert!(
            !auto_get(s).unwrap().enabled,
            "auto was left armed alongside timer"
        );

        // And back the other way.
        assert!(auto_enable(s, "ship it", 5, false));
        assert!(
            !timer_get(s).unwrap().enabled,
            "timer was left armed alongside auto"
        );
    }

    /// A skipped run is lost, not deferred.
    ///
    /// `last_run_at` is what the next interval counts from, so a skip must not
    /// touch it — stamping it would push the schedule out every time the agent
    /// was busy, and counting from the due time instead would fire the moment
    /// it went quiet and again immediately after.
    #[test]
    fn a_skipped_run_leaves_the_clock_where_it_was() {
        let _guard = crate::test_home::lock();
        let tmp = tempfile::tempdir().unwrap();
        scratch(tmp.path());
        let s = "oc_proj_1a2b3c4d";

        assert!(timer_enable(s, "p", 60));
        // Age the clock to something no fresh stamp could equal: `now()` is
        // millisecond-granular and the whole test runs inside a few of them, so
        // comparing two fresh stamps would pass whether or not a skip touched it.
        const AGED: &str = "2000-01-01T00:00:00.000Z";
        let age = |to: &str| {
            let to = to.to_string();
            with_db(move |conn| {
                conn.execute(
                    "UPDATE timer SET last_run_at = ?2 WHERE session = ?1",
                    rusqlite::params![s, to],
                )?;
                Ok(())
            });
        };
        age(AGED);
        let armed = timer_get(s).unwrap().last_run_at;
        assert_eq!(armed, AGED);

        timer_mark_skipped(s);
        timer_mark_skipped(s);
        let after = timer_get(s).unwrap();
        assert_eq!(after.skipped, 2);
        assert_eq!(after.last_run_at, AGED, "a skip moved the clock");

        timer_mark_run(s);
        let moved = timer_get(s).unwrap().last_run_at;
        assert_ne!(moved, AGED, "a real send must move the clock");

        // Disabling leaves the row, so re-arming later starts from a clean slate
        // rather than inheriting a stale skip count.
        timer_disable(s);
        assert!(!timer_get(s).unwrap().enabled);
        assert!(timer_enable(s, "p", 60));
        assert_eq!(timer_get(s).unwrap().skipped, 0);
    }

    /// The tree asks for one set rather than one row per session.
    #[test]
    fn scheduled_sessions_are_listed_for_the_tree() {
        let _guard = crate::test_home::lock();
        let tmp = tempfile::tempdir().unwrap();
        scratch(tmp.path());

        assert!(timer_enabled_sessions().is_empty());
        assert!(timer_enable("cc_a_11111111", "x", 60));
        assert!(timer_enable("oc_b_22222222", "y", 60));
        timer_disable("oc_b_22222222");

        let on = timer_enabled_sessions();
        assert!(on.contains("cc_a_11111111"));
        assert!(
            !on.contains("oc_b_22222222"),
            "a disabled schedule still showed"
        );
    }

    #[test]
    fn labels_and_conversation_ids_round_trip() {
        let _guard = crate::test_home::lock();
        let tmp = tempfile::tempdir().unwrap();
        scratch(tmp.path());

        set_label("cc_proj_1a2b3c4d", "工作");
        set_conversation_id("cc_proj_1a2b3c4d", "conv-a");

        // Both columns live on one row now, where three files used to
        // disagree about the same session.
        assert_eq!(
            labels().get("cc_proj_1a2b3c4d").map(String::as_str),
            Some("工作")
        );
        assert_eq!(
            conversation_id("cc_proj_1a2b3c4d").as_deref(),
            Some("conv-a")
        );

        // An empty label clears rather than storing "".
        set_label("cc_proj_1a2b3c4d", "  ");
        assert!(labels().is_empty());
        assert_eq!(
            conversation_id("cc_proj_1a2b3c4d").as_deref(),
            Some("conv-a")
        );

        std::env::remove_var("AMUX_DB_PATH");
    }

    #[test]
    fn auto_mode_round_trips_and_counts_turns() {
        let _guard = crate::test_home::lock();
        let tmp = tempfile::tempdir().unwrap();
        scratch(tmp.path());

        assert!(auto_get("cc_proj_1a2b3c4d").is_none());

        assert!(auto_enable("cc_proj_1a2b3c4d", "ship the feature", 3, true));
        let cfg = auto_get("cc_proj_1a2b3c4d").unwrap();
        assert_eq!(cfg.goal, "ship the feature");
        assert_eq!(cfg.max_turns, 3);
        assert_eq!(cfg.used, 0);
        assert!(cfg.allow_waiting);
        assert!(cfg.enabled);

        assert_eq!(auto_bump("cc_proj_1a2b3c4d"), 1);
        assert_eq!(auto_bump("cc_proj_1a2b3c4d"), 2);
        assert_eq!(auto_get("cc_proj_1a2b3c4d").unwrap().used, 2);

        auto_disable("cc_proj_1a2b3c4d");
        assert!(!auto_get("cc_proj_1a2b3c4d").unwrap().enabled);

        // Re-arming resets the counter, so a fresh run gets its full budget.
        assert!(auto_enable("cc_proj_1a2b3c4d", "second goal", 5, false));
        let cfg = auto_get("cc_proj_1a2b3c4d").unwrap();
        assert_eq!(cfg.used, 0);
        assert_eq!(cfg.goal, "second goal");
        assert!(!cfg.allow_waiting);

        assert_eq!(auto_list().len(), 1);

        std::env::remove_var("AMUX_DB_PATH");
    }

    /// Naming a project must survive the daemon's own upserts, which run on
    /// every snapshot poll — an alias that a background refresh wipes is worse
    /// than no alias at all.
    #[test]
    fn an_alias_survives_project_refreshes() {
        let _guard = crate::test_home::lock();
        let tmp = tempfile::tempdir().unwrap();
        scratch(tmp.path());

        set_alias("/work/iotex", "IoTeX 主线");
        upsert_project("/work/iotex", "iotex", "claude", "2026-09-13T00:00:00Z", 3);

        let row = projects()
            .into_iter()
            .find(|p| p.path == "/work/iotex")
            .unwrap();
        assert_eq!(row.alias.as_deref(), Some("IoTeX 主线"));
        assert_eq!(row.last_agent, "claude");
        assert_eq!(row.launch_count, 3);

        std::env::remove_var("AMUX_DB_PATH");
    }

    /// Naming a directory must not, by itself, make it a project.
    ///
    /// Project history is the allowlist `browsable_roots` gives `/api/files/*`,
    /// so a row created purely by `amux alias` would expose that directory to
    /// the phone's file browser. The marker is an empty `last_agent`: nothing
    /// has ever been launched there.
    #[test]
    fn naming_a_directory_records_no_agent() {
        let _guard = crate::test_home::lock();
        let tmp = tempfile::tempdir().unwrap();
        scratch(tmp.path());

        set_alias("/work/secrets", "私密");
        let row = projects()
            .into_iter()
            .find(|p| p.path == "/work/secrets")
            .unwrap();
        assert_eq!(row.alias.as_deref(), Some("私密"));
        assert!(
            row.last_agent.is_empty(),
            "an alias-only row must stay distinguishable from a real project"
        );

        // Actually launching there turns it into one, keeping the name.
        upsert_project(
            "/work/secrets",
            "secrets",
            "claude",
            "2026-09-13T00:00:00Z",
            1,
        );
        let row = projects()
            .into_iter()
            .find(|p| p.path == "/work/secrets")
            .unwrap();
        assert_eq!(row.last_agent, "claude");
        assert_eq!(row.alias.as_deref(), Some("私密"));

        std::env::remove_var("AMUX_DB_PATH");
    }

    /// History is capped at eighty entries, and a name must not expire with it.
    #[test]
    fn a_named_project_is_not_evicted_by_the_history_cap() {
        let _guard = crate::test_home::lock();
        let tmp = tempfile::tempdir().unwrap();
        scratch(tmp.path());

        set_alias("/work/named", "有名字的");
        upsert_project("/work/plain", "plain", "claude", "2026-01-01T00:00:00Z", 0);
        assert_eq!(projects().len(), 2);

        // A refresh that mentions neither of them — what happens once both have
        // fallen outside the newest eighty.
        replace_projects(&[ProjectRow {
            path: "/work/other".into(),
            name: "other".into(),
            alias: None,
            last_agent: "codex".into(),
            last_seen_at: "2026-02-01T00:00:00Z".into(),
            launch_count: 1,
        }]);

        let rows = projects();
        let paths: Vec<&str> = rows.iter().map(|r| r.path.as_str()).collect();
        assert!(
            paths.contains(&"/work/other"),
            "the refreshed row is missing"
        );
        assert!(
            !paths.contains(&"/work/plain"),
            "an unnamed row should be evicted"
        );
        assert!(
            paths.contains(&"/work/named"),
            "eviction silently discarded a name the user chose"
        );

        std::env::remove_var("AMUX_DB_PATH");
    }

    /// Re-running the import must not resurrect the JSON files' contents over
    /// newer rows. The guard is "the tables are empty", so this checks that a
    /// second pass over the same home directory changes nothing.
    #[test]
    fn migration_imports_once_and_does_not_overwrite_newer_data() {
        let _guard = crate::test_home::lock();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join(".amux")).unwrap();
        std::fs::create_dir_all(home.join(".agent-monitor")).unwrap();
        std::fs::write(
            home.join(".agent-monitor/project-history.json"),
            r#"[{"path":"/work/alpha","name":"alpha","lastAgent":"claude","lastSeenAt":"2026-01-01T00:00:00Z","launchCount":2}]"#,
        )
        .unwrap();
        std::fs::write(
            home.join(".amux/session-labels.json"),
            r#"{"cc_alpha_11111111":"旧标签"}"#,
        )
        .unwrap();
        std::fs::write(
            home.join(".amux/session-ids.json"),
            r#"{"cc_alpha_11111111":"conv-old"}"#,
        )
        .unwrap();

        let conn = Connection::open(tmp.path().join("t.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (path TEXT PRIMARY KEY, name TEXT NOT NULL, alias TEXT,
                last_agent TEXT NOT NULL DEFAULT '', last_seen_at TEXT NOT NULL DEFAULT '',
                launch_count INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE sessions (name TEXT PRIMARY KEY, project_path TEXT, agent TEXT,
                label TEXT, conversation_id TEXT, updated_at TEXT NOT NULL DEFAULT '');",
        )
        .unwrap();

        migrate_from(&conn, &home);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM projects", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "project row not imported");
        let label: String = conn
            .query_row(
                "SELECT label FROM sessions WHERE name = 'cc_alpha_11111111'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(label, "旧标签");
        let id: String = conn
            .query_row(
                "SELECT conversation_id FROM sessions WHERE name = 'cc_alpha_11111111'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(id, "conv-old", "labels and ids must land on the same row");

        // Sources are renamed so no stale copy can be read back.
        assert!(!home.join(".amux/session-labels.json").exists());
        assert!(home.join(".amux/session-labels.json.migrated").exists());

        // Now the user renames the session. A second migration pass — a
        // restart, say — must leave that alone.
        conn.execute(
            "UPDATE sessions SET label = '新标签' WHERE name = 'cc_alpha_11111111'",
            [],
        )
        .unwrap();
        std::fs::rename(
            home.join(".amux/session-labels.json.migrated"),
            home.join(".amux/session-labels.json"),
        )
        .unwrap();

        migrate_from(&conn, &home);

        let label: String = conn
            .query_row(
                "SELECT label FROM sessions WHERE name = 'cc_alpha_11111111'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(label, "新标签", "re-import clobbered newer data");
    }

    /// A cached summary is only valid for the transcript it was computed from,
    /// at the size it then had. Claude's summary means scanning the whole file
    /// — measured at 1.6s on a 286MB transcript — so a stale hit is not a
    /// small error.
    #[test]
    fn a_summary_cache_entry_expires_when_the_transcript_changes() {
        let _guard = crate::test_home::lock();
        let tmp = tempfile::tempdir().unwrap();
        scratch(tmp.path());

        assert!(
            cached_summary("/t/a.jsonl", 100.0).is_none(),
            "empty cache hit"
        );

        put_summary("/t/a.jsonl", "claude", Some("逆向登录接口"), 100.0);
        assert_eq!(
            cached_summary("/t/a.jsonl", 100.0),
            Some(Some("逆向登录接口".into()))
        );

        // A rewritten transcript invalidates it.
        assert!(
            cached_summary("/t/a.jsonl", 101.0).is_none(),
            "stale mtime hit"
        );

        // "No summary" is itself worth caching — recomputing it costs a full scan.
        put_summary("/t/b.jsonl", "codex", None, 5.0);
        assert_eq!(cached_summary("/t/b.jsonl", 5.0), Some(None));

        std::env::remove_var("AMUX_DB_PATH");
    }

    /// Run history survives the log being truncated and can be narrowed to a
    /// source or a session — the two things the old stderr log could not do.
    #[test]
    fn activity_round_trips_and_filters() {
        let _guard = crate::test_home::lock();
        let tmp = tempfile::tempdir().unwrap();
        scratch(tmp.path());

        activity_append("auto", "cc_a_11111111", "/p/a", "continued", Some(1), Some(50), "keep going");
        activity_append("auto", "cc_a_11111111", "/p/a", "stopped", Some(2), Some(50), "goal met");
        activity_append("timer", "cc_a_11111111", "/p/a", "sent", None, None, "看一下 CI");
        activity_append("auto", "cx_b_22222222", "/p/b", "unavailable", Some(0), Some(50), "boom");

        // Newest first.
        let all = activity_list(None, None, 10);
        assert_eq!(all.len(), 4);
        assert_eq!(all[0].kind, "unavailable");
        assert_eq!(all[3].message, "keep going");

        assert_eq!(activity_list(Some("auto"), None, 10).len(), 3);
        assert_eq!(activity_list(Some("timer"), None, 10).len(), 1);
        assert_eq!(activity_list(None, Some("cc_a_11111111"), 10).len(), 3);

        let one = activity_list(Some("timer"), Some("cc_a_11111111"), 10);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].message, "看一下 CI");
        assert_eq!(one[0].project, "/p/a");

        std::env::remove_var("AMUX_DB_PATH");
    }

    /// The cap is what keeps a retry loop from growing the table forever, the
    /// way the stderr log grew to 1545 lines in one night.
    #[test]
    fn activity_is_capped_per_session() {
        let _guard = crate::test_home::lock();
        let tmp = tempfile::tempdir().unwrap();
        scratch(tmp.path());

        let session = "cc_busy_33333333";
        for i in 0..ACTIVITY_KEEP_PER_SESSION + 2 {
            activity_append("auto", session, "/p/x", "unavailable", None, None, &format!("failure {i}"));
        }
        let rows = activity_list(Some("auto"), Some(session), 1000);
        assert_eq!(rows.len(), ACTIVITY_KEEP_PER_SESSION);
        // The two oldest are gone; the newest is kept.
        assert_eq!(rows[0].message, format!("failure {}", ACTIVITY_KEEP_PER_SESSION + 1));
        assert!(!rows.iter().any(|r| r.message == "failure 0"));

        // Another session's history is untouched by that trim.
        activity_append("auto", "cc_other_44444444", "/p/y", "continued", None, None, "hi");
        assert_eq!(activity_list(None, Some("cc_other_44444444"), 10).len(), 1);

        std::env::remove_var("AMUX_DB_PATH");
    }
}
