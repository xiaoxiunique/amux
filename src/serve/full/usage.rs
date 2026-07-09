//! Token-usage feature (Claude Code + Codex), computed via `ccusage`.
//!
//! Two endpoints:
//! - `GET /api/usage` — all-time totals (cached).
//! - `GET /api/usage/daily` — per-day breakdown for the Usage page (cached).
//!
//! Both shell out to `bunx ccusage <subcmd> --json --offline`, parse the JSON,
//! and run the Claude + Codex invocations in parallel threads.

use axum::{
    body::Body,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use serde_json::json;
use std::collections::HashMap;
use std::process::Command;
use std::sync::{LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::serve::server::{is_authed, json_response, AppState};

/// Cached total token usage (Claude Code + Codex), computed via `ccusage`.
static USAGE_CACHE: LazyLock<Mutex<Option<(Instant, serde_json::Value)>>> =
    LazyLock::new(|| Mutex::new(None));
/// Cached per-day usage breakdown (Settings → Usage page).
static USAGE_DAILY_CACHE: LazyLock<Mutex<Option<(Instant, serde_json::Value)>>> =
    LazyLock::new(|| Mutex::new(None));
const USAGE_TTL: Duration = Duration::from_secs(300);

fn lock_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn command_stdout(command: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(command).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Run `ccusage <subcmd> --json` (via a login shell so bun/PATH resolve) and
/// extract the all-time totals. `subcmd` is e.g. "daily" (Claude Code) or
/// "codex daily" (Codex).
fn ccusage_totals(subcmd: &str) -> Option<serde_json::Value> {
    let cmd = format!("bunx ccusage {subcmd} --json --offline");
    let out = command_stdout("/bin/zsh", &["-lc", &cmd])?;
    let v: serde_json::Value = serde_json::from_str(out.trim()).ok()?;
    let t = v.get("totals")?;
    // Codex variant uses `costUSD`; Claude uses `totalCost`.
    let cost = t.get("totalCost").or_else(|| t.get("costUSD")).cloned();
    Some(json!({
        "totalTokens": t.get("totalTokens").cloned().unwrap_or(json!(0)),
        "inputTokens": t.get("inputTokens").cloned().unwrap_or(json!(0)),
        "outputTokens": t.get("outputTokens").cloned().unwrap_or(json!(0)),
        "cost": cost.unwrap_or(json!(0)),
    }))
}

fn compute_usage() -> serde_json::Value {
    // Run both in parallel — each ccusage invocation parses a lot of logs.
    let h_claude = thread::spawn(|| ccusage_totals("daily"));
    let h_codex = thread::spawn(|| ccusage_totals("codex daily"));
    let claude = h_claude.join().ok().flatten();
    let codex = h_codex.join().ok().flatten();
    json!({
        "ok": claude.is_some() || codex.is_some(),
        "claude": claude.unwrap_or(json!(null)),
        "codex": codex.unwrap_or(json!(null)),
    })
}

/// `GET /api/usage` — total Claude Code + Codex token usage (via ccusage),
/// cached for a few minutes since parsing the session logs is expensive.
pub(crate) async fn api_usage(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    {
        let cache = lock_recover(&USAGE_CACHE);
        if let Some((at, value)) = cache.as_ref() {
            if at.elapsed() < USAGE_TTL {
                return json_response(StatusCode::OK, value.clone());
            }
        }
    }
    let value = match tokio::task::spawn_blocking(compute_usage).await {
        Ok(v) => v,
        Err(error) => {
            return json_response(
                StatusCode::OK,
                json!({ "ok": false, "error": error.to_string() }),
            )
        }
    };
    *lock_recover(&USAGE_CACHE) = Some((Instant::now(), value.clone()));
    json_response(StatusCode::OK, value)
}

/// Run `ccusage <subcmd> --json` and return (all-time totals, per-day rows).
/// Each row is normalized to `{date, tokens, cost}` (Claude uses `period`/
/// `totalCost`, Codex uses `date`/`costUSD`).
fn ccusage_daily(subcmd: &str) -> Option<(serde_json::Value, Vec<serde_json::Value>)> {
    let cmd = format!("bunx ccusage {subcmd} --json --offline");
    let out = command_stdout("/bin/zsh", &["-lc", &cmd])?;
    let v: serde_json::Value = serde_json::from_str(out.trim()).ok()?;
    let totals = v
        .get("totals")
        .map(|t| {
            json!({
                "totalTokens": t.get("totalTokens").cloned().unwrap_or(json!(0)),
                "inputTokens": t.get("inputTokens").cloned().unwrap_or(json!(0)),
                "outputTokens": t.get("outputTokens").cloned().unwrap_or(json!(0)),
                "cost": t.get("totalCost").or_else(|| t.get("costUSD")).cloned().unwrap_or(json!(0)),
            })
        })
        .unwrap_or(json!(null));
    let rows = v
        .get("daily")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    let date = r
                        .get("period")
                        .or_else(|| r.get("date"))
                        .and_then(|d| d.as_str())?
                        .to_string();
                    let tokens = r.get("totalTokens").and_then(|t| t.as_f64()).unwrap_or(0.0);
                    let cost = r
                        .get("totalCost")
                        .or_else(|| r.get("costUSD"))
                        .and_then(|c| c.as_f64())
                        .unwrap_or(0.0);
                    Some(json!({ "date": date, "tokens": tokens, "cost": cost }))
                })
                .collect()
        })
        .unwrap_or_default();
    Some((totals, rows))
}

fn compute_usage_daily() -> serde_json::Value {
    let h_claude = thread::spawn(|| ccusage_daily("daily"));
    let h_codex = thread::spawn(|| ccusage_daily("codex daily"));
    let (claude_totals, claude_rows) = h_claude
        .join()
        .ok()
        .flatten()
        .unwrap_or((json!(null), Vec::new()));
    let (codex_totals, codex_rows) = h_codex
        .join()
        .ok()
        .flatten()
        .unwrap_or((json!(null), Vec::new()));

    // Merge both agents by date. BTreeMap keeps dates sorted ascending.
    let mut by_date: std::collections::BTreeMap<String, (f64, f64, f64, f64)> =
        std::collections::BTreeMap::new();
    for r in &claude_rows {
        if let Some(d) = r.get("date").and_then(|d| d.as_str()) {
            let e = by_date.entry(d.to_string()).or_default();
            e.0 += r.get("tokens").and_then(|t| t.as_f64()).unwrap_or(0.0);
            e.1 += r.get("cost").and_then(|c| c.as_f64()).unwrap_or(0.0);
        }
    }
    for r in &codex_rows {
        if let Some(d) = r.get("date").and_then(|d| d.as_str()) {
            let e = by_date.entry(d.to_string()).or_default();
            e.2 += r.get("tokens").and_then(|t| t.as_f64()).unwrap_or(0.0);
            e.3 += r.get("cost").and_then(|c| c.as_f64()).unwrap_or(0.0);
        }
    }
    // Newest first.
    let days: Vec<serde_json::Value> = by_date
        .into_iter()
        .rev()
        .map(|(date, (ct, cc, xt, xc))| {
            json!({
                "date": date,
                "claudeTokens": ct as i64,
                "claudeCost": cc,
                "codexTokens": xt as i64,
                "codexCost": xc,
            })
        })
        .collect();

    json!({
        "ok": claude_totals.is_object() || codex_totals.is_object() || !days.is_empty(),
        "claude": claude_totals,
        "codex": codex_totals,
        "days": days,
    })
}

/// `GET /api/usage/daily` — per-day Claude + Codex usage for the Usage page.
pub(crate) async fn api_usage_daily(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    {
        let cache = lock_recover(&USAGE_DAILY_CACHE);
        if let Some((at, value)) = cache.as_ref() {
            if at.elapsed() < USAGE_TTL {
                return json_response(StatusCode::OK, value.clone());
            }
        }
    }
    let value = match tokio::task::spawn_blocking(compute_usage_daily).await {
        Ok(v) => v,
        Err(error) => {
            return json_response(
                StatusCode::OK,
                json!({ "ok": false, "error": error.to_string() }),
            )
        }
    };
    *lock_recover(&USAGE_DAILY_CACHE) = Some((Instant::now(), value.clone()));
    json_response(StatusCode::OK, value)
}
