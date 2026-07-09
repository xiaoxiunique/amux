//! APNs push notifications + per-session notify-config.
//!
//! Ported from the Agent Port monitor service. Provides:
//! - A local SQLite store (device tokens + per-session notify config) that
//!   survives restarts; the service still runs fully in-memory if the DB
//!   can't be opened.
//! - Token-based APNs auth (ES256 JWT signed with a `.p8` key), HTTP/2 to
//!   `api(.sandbox).push.apple.com`.
//! - Status-transition notifications: when a phone-initiated Claude turn stops
//!   for confirmation (Running→Waiting) or finishes (Running→Idle/Done), push
//!   a Chinese-language alert to every registered device — but only for opted-in
//!   sessions and only for turns kicked off from a phone.
//!
//! Handlers: `GET/POST /api/pane/notify-config`, `GET /api/push/status`,
//! `POST /api/push/register`, `POST /api/push/test`.

use axum::{
    body::Body,
    extract::{Json, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::serve::server::{
    is_authed, json_response, now_iso, pane_is_claude, AppState, Pane, PaneStatus,
};

// ===========================================================================
// Global mutable state (in-memory caches; the DB is the durable backing store)
// ===========================================================================

// Registered iOS device tokens (hex). Populated by POST /api/push/register.
static DEVICE_TOKENS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));
// Cached APNs provider JWT + when it was minted; refreshed ~every 50 min.
static APNS_JWT: LazyLock<Mutex<Option<(String, Instant)>>> = LazyLock::new(|| Mutex::new(None));
// Local SQLite DB for state that must survive restarts (registered device
// tokens + notify config). None if it can't be opened, in which case the
// service still runs fully in-memory.
static DB: LazyLock<Mutex<Option<rusqlite::Connection>>> =
    LazyLock::new(|| Mutex::new(open_db()));
// Per-session notification config (key = pane.path), cached in memory and
// persisted in `notify_config`. Loaded at startup.
static NOTIFY_CONFIG: LazyLock<Mutex<HashMap<String, NotifyConfig>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
// Per-session "this turn was kicked off from a phone" flag (key = pane.path).
// Set by a mobile /api/send, cleared by a desktop send, consumed when a
// status-change notification fires. In-memory only.
static MOBILE_TRIGGERED: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
// Previous status per session (key = pane.path) for status-change detection.
static NOTIFY_PREV_STATUS: LazyLock<Mutex<HashMap<String, PaneStatus>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Lock a mutex, recovering the guard even if a previous holder panicked
/// (poisoned) so one panic doesn't cascade into every later lock.
fn lock_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ===========================================================================
// Local persistence (SQLite)
// ===========================================================================

/// State directory: `$AGENT_MONITOR_STATE_DIR` if set, else `~/.amux`.
fn db_path() -> Option<PathBuf> {
    let dir = match env::var_os("AGENT_MONITOR_STATE_DIR") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => dirs::home_dir()?.join(".amux"),
    };
    fs::create_dir_all(&dir).ok()?;
    Some(dir.join("agent-port.db"))
}

/// Open (and migrate) the local SQLite DB. Returns None on any failure so the
/// service degrades to in-memory-only rather than refusing to start.
fn open_db() -> Option<rusqlite::Connection> {
    let path = db_path()?;
    let conn = rusqlite::Connection::open(&path).ok()?;
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    conn.execute(
        "CREATE TABLE IF NOT EXISTS device_tokens (
            token TEXT PRIMARY KEY,
            created_at TEXT NOT NULL
        )",
        [],
    )
    .ok()?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS notify_config (
            key TEXT PRIMARY KEY,
            enabled INTEGER NOT NULL,
            events TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )",
        [],
    )
    .ok()?;
    Some(conn)
}

/// Load persisted device tokens into the in-memory set at startup.
pub(crate) fn load_device_tokens() {
    let tokens: Vec<String> = {
        let guard = lock_recover(&DB);
        let Some(conn) = guard.as_ref() else {
            return;
        };
        let Ok(mut stmt) = conn.prepare("SELECT token FROM device_tokens") else {
            return;
        };
        let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0)) else {
            return;
        };
        rows.flatten().collect()
    };
    let mut set = lock_recover(&DEVICE_TOKENS);
    for token in tokens {
        set.insert(token);
    }
}

/// Persist one device token (idempotent).
fn save_device_token(token: &str) {
    let guard = lock_recover(&DB);
    if let Some(conn) = guard.as_ref() {
        let _ = conn.execute(
            "INSERT OR IGNORE INTO device_tokens (token, created_at) VALUES (?1, ?2)",
            rusqlite::params![token, now_iso()],
        );
    }
}

#[derive(Clone)]
struct NotifyConfig {
    enabled: bool,
    events: Vec<String>,
}

impl NotifyConfig {
    fn default_off() -> Self {
        NotifyConfig {
            enabled: false,
            events: vec!["waiting".to_string(), "done".to_string()],
        }
    }
}

/// Load persisted notification configs into memory at startup.
pub(crate) fn load_notify_config() {
    let rows: Vec<(String, bool, String)> = {
        let guard = lock_recover(&DB);
        let Some(conn) = guard.as_ref() else {
            return;
        };
        let Ok(mut stmt) = conn.prepare("SELECT key, enabled, events FROM notify_config") else {
            return;
        };
        let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? != 0,
                row.get::<_, String>(2)?,
            ))
        }) else {
            return;
        };
        rows.flatten().collect()
    };
    let mut map = lock_recover(&NOTIFY_CONFIG);
    for (key, enabled, events) in rows {
        let events = events
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        map.insert(key, NotifyConfig { enabled, events });
    }
}

/// Persist (upsert) one session's notification config.
fn save_notify_config(key: &str, cfg: &NotifyConfig) {
    let guard = lock_recover(&DB);
    if let Some(conn) = guard.as_ref() {
        let _ = conn.execute(
            "INSERT INTO notify_config (key, enabled, events, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(key) DO UPDATE SET
               enabled = excluded.enabled,
               events = excluded.events,
               updated_at = excluded.updated_at",
            rusqlite::params![key, cfg.enabled as i64, cfg.events.join(","), now_iso()],
        );
    }
}

/// Current notification config for a session path (default = disabled).
fn notify_config_for(path: &str) -> NotifyConfig {
    lock_recover(&NOTIFY_CONFIG)
        .get(path)
        .cloned()
        .unwrap_or_else(NotifyConfig::default_off)
}

// ===========================================================================
// Push notifications (APNs, token-based auth via a .p8 key)
// ===========================================================================

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PushRegisterRequest {
    device_token: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PushTestRequest {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

struct ApnsConfig {
    key_id: String,
    team_id: String,
    key_pem: String,
    bundle_id: String,
    production: bool,
}

/// Read APNs credentials from the environment. Returns None (push disabled)
/// when any required value is missing, so the service runs fine without push.
fn apns_config() -> Option<ApnsConfig> {
    let key_id = env::var("APNS_KEY_ID").ok().filter(|s| !s.is_empty())?;
    let team_id = env::var("APNS_TEAM_ID").ok().filter(|s| !s.is_empty())?;
    let key_path = env::var("APNS_KEY_PATH").ok().filter(|s| !s.is_empty())?;
    let key_pem = fs::read_to_string(&key_path).ok()?;
    let bundle_id = env::var("APNS_BUNDLE_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "dev.hcg.agentPort".to_string());
    let production = env::var("APNS_ENV")
        .map(|v| {
            let v = v.to_ascii_lowercase();
            v == "production" || v == "prod"
        })
        .unwrap_or(false);
    Some(ApnsConfig {
        key_id,
        team_id,
        key_pem,
        bundle_id,
        production,
    })
}

/// Mint (and cache ~50 min) the APNs provider JWT (ES256, signed with the .p8).
fn apns_jwt(cfg: &ApnsConfig) -> Option<String> {
    if let Some((token, at)) = lock_recover(&APNS_JWT).as_ref() {
        if at.elapsed() < Duration::from_secs(50 * 60) {
            return Some(token.clone());
        }
    }
    #[derive(Serialize)]
    struct Claims {
        iss: String,
        iat: i64,
    }
    let key = jsonwebtoken::EncodingKey::from_ec_pem(cfg.key_pem.as_bytes()).ok()?;
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
    header.kid = Some(cfg.key_id.clone());
    let claims = Claims {
        iss: cfg.team_id.clone(),
        iat: chrono::Utc::now().timestamp(),
    };
    let token = jsonwebtoken::encode(&header, &claims, &key).ok()?;
    *lock_recover(&APNS_JWT) = Some((token.clone(), Instant::now()));
    Some(token)
}

/// Send one alert to one device token over APNs HTTP/2. Blocking.
fn apns_send_one(
    cfg: &ApnsConfig,
    jwt: &str,
    device_token: &str,
    title: &str,
    body: &str,
    pane_id: Option<&str>,
) -> Result<(), String> {
    let host = if cfg.production {
        "api.push.apple.com"
    } else {
        "api.sandbox.push.apple.com"
    };
    let url = format!("https://{host}/3/device/{device_token}");
    let mut payload = json!({
        "aps": {
            "alert": { "title": title, "body": body },
            "sound": "default",
        }
    });
    if let Some(pane_id) = pane_id {
        payload["paneId"] = json!(pane_id);
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(&url)
        .header("authorization", format!("bearer {jwt}"))
        .header("apns-topic", cfg.bundle_id.as_str())
        .header("apns-push-type", "alert")
        .header("apns-priority", "10")
        .json(&payload)
        .send()
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(format!("{status}: {}", resp.text().unwrap_or_default()))
    }
}

/// Fire-and-forget push to every registered device (used by status-change
/// notifications). Logs failures; never blocks the caller.
fn push_to_all(title: String, body: String, pane_id: Option<String>) {
    let Some(cfg) = apns_config() else {
        return;
    };
    let tokens: Vec<String> = lock_recover(&DEVICE_TOKENS).iter().cloned().collect();
    if tokens.is_empty() {
        return;
    }
    tokio::task::spawn_blocking(move || {
        let Some(jwt) = apns_jwt(&cfg) else {
            eprintln!("[push] failed to mint APNs JWT");
            return;
        };
        for token in tokens {
            if let Err(error) = apns_send_one(&cfg, &jwt, &token, &title, &body, pane_id.as_deref())
            {
                eprintln!("[push] send failed: {error}");
            }
        }
    });
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NotifyConfigRequest {
    key: String,
    enabled: bool,
    #[serde(default)]
    events: Vec<String>,
}

pub(crate) async fn api_notify_config_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    let Some(key) = query.get("key").filter(|value| !value.is_empty()) else {
        return json_response(StatusCode::BAD_REQUEST, json!({ "error": "key is required" }));
    };
    let cfg = notify_config_for(key);
    json_response(
        StatusCode::OK,
        json!({ "ok": true, "enabled": cfg.enabled, "events": cfg.events }),
    )
}

pub(crate) async fn api_notify_config_set(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Json(body): Json<NotifyConfigRequest>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    if body.key.is_empty() {
        return json_response(StatusCode::BAD_REQUEST, json!({ "error": "key is required" }));
    }
    let allowed = ["waiting", "done"];
    let events: Vec<String> = body
        .events
        .into_iter()
        .filter(|event| allowed.contains(&event.as_str()))
        .collect();
    let cfg = NotifyConfig {
        enabled: body.enabled,
        events,
    };
    lock_recover(&NOTIFY_CONFIG).insert(body.key.clone(), cfg.clone());
    save_notify_config(&body.key, &cfg);
    json_response(
        StatusCode::OK,
        json!({ "ok": true, "enabled": cfg.enabled, "events": cfg.events }),
    )
}

pub(crate) async fn api_push_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    let cfg = apns_config();
    let env = match &cfg {
        Some(c) if c.production => "production",
        Some(_) => "sandbox",
        None => "unset",
    };
    let count = lock_recover(&DEVICE_TOKENS).len();
    json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "configured": cfg.is_some(),
            "env": env,
            "deviceCount": count,
        }),
    )
}

pub(crate) async fn api_push_register(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Json(body): Json<PushRegisterRequest>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    let token = body.device_token.trim().to_string();
    if token.is_empty() {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({ "error": "deviceToken is required" }),
        );
    }
    lock_recover(&DEVICE_TOKENS).insert(token.clone());
    save_device_token(&token);
    let count = lock_recover(&DEVICE_TOKENS).len();
    json_response(StatusCode::OK, json!({ "ok": true, "count": count }))
}

/// Manual test: push a sample alert to every registered device and return the
/// per-device APNs result inline (so env / token mismatches are visible).
pub(crate) async fn api_push_test(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Json(body): Json<PushTestRequest>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    let Some(cfg) = apns_config() else {
        return json_response(
            StatusCode::OK,
            json!({ "ok": false, "error": "APNs not configured (set APNS_KEY_ID / APNS_TEAM_ID / APNS_KEY_PATH)" }),
        );
    };
    let title = body.title.unwrap_or_else(|| "Agent Port".to_string());
    let text = body.body.unwrap_or_else(|| "测试推送 ✅".to_string());
    let results = tokio::task::spawn_blocking(move || {
        let tokens: Vec<String> = lock_recover(&DEVICE_TOKENS).iter().cloned().collect();
        if tokens.is_empty() {
            return json!({ "ok": false, "error": "no registered devices" });
        }
        let Some(jwt) = apns_jwt(&cfg) else {
            return json!({ "ok": false, "error": "failed to mint APNs JWT" });
        };
        let env = if cfg.production { "production" } else { "sandbox" };
        let per: Vec<serde_json::Value> = tokens
            .iter()
            .map(|t| {
                let head: String = t.chars().take(12).collect();
                match apns_send_one(&cfg, &jwt, t, &title, &text, None) {
                    Ok(()) => json!({ "token": format!("{head}…"), "ok": true }),
                    Err(e) => json!({ "token": format!("{head}…"), "ok": false, "error": e }),
                }
            })
            .collect();
        json!({ "ok": true, "env": env, "results": per })
    })
    .await
    .unwrap_or_else(|e| json!({ "ok": false, "error": format!("join error: {e}") }));
    json_response(StatusCode::OK, results)
}

// ===========================================================================
// Status-change notifications
// ===========================================================================

/// Tag (or clear) a session's "kicked off from a phone" flag based on the
/// `x-agent-port-source` header of the send that triggered this turn. Mobile
/// (`ios`/`android`) sets it; anything else (desktop) clears it.
///
/// Call this from the send handler with the request headers and the target
/// pane's path.
pub(crate) fn mark_send_source(headers: &HeaderMap, path: &str) {
    if path.is_empty() {
        return;
    }
    let source = headers
        .get("x-agent-port-source")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let mut map = lock_recover(&MOBILE_TRIGGERED);
    if matches!(source, "ios" | "android") {
        map.insert(path.to_string(), Instant::now());
    } else {
        map.remove(path);
    }
}

/// Push a notification when a Claude session stops for a confirmation
/// (Running→Waiting) or finishes its turn (Running→Idle/Done) — but only for
/// turns kicked off from a phone (MOBILE_TRIGGERED) and sessions that opted in.
pub(crate) fn notify_status_changes(panes: &[Pane]) {
    for pane in panes {
        let path = pane.path.trim();
        if path.is_empty() {
            continue;
        }
        if !pane_is_claude(&pane.session, &pane.command, &pane.title) {
            continue;
        }
        let cur = pane.status.clone();
        let prev = {
            let mut map = lock_recover(&NOTIFY_PREV_STATUS);
            let prev = map.get(path).cloned();
            map.insert(path.to_string(), cur.clone());
            prev
        };
        let event = match (prev, cur) {
            (Some(PaneStatus::Running), PaneStatus::Waiting) => "waiting",
            (Some(PaneStatus::Running), PaneStatus::Idle | PaneStatus::Done) => "done",
            _ => continue,
        };
        let cfg = notify_config_for(path);
        if !cfg.enabled || !cfg.events.iter().any(|e| e == event) {
            continue;
        }
        // Only notify for phone-initiated turns; a desktop send (or no send at
        // all — the user typing directly in tmux) means they're at the computer.
        if lock_recover(&MOBILE_TRIGGERED).remove(path).is_none() {
            continue;
        }
        let project = path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(path);
        let body = if event == "waiting" {
            "需要你确认 ⏳"
        } else {
            "任务完成 ✅"
        };
        push_to_all(project.to_string(), body.to_string(), Some(pane.id.clone()));
    }
}
