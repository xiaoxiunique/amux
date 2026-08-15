//! Bridge for DeepSeek Harness (`dsh`) sessions.
//!
//! `dsh web` runs a local server whose sessions are real agent conversations —
//! same idea as an rmux pane, but reported structurally instead of scraped off
//! a terminal. Surfacing them alongside the rmux panes lets the phone app see
//! and drive them without knowing dsh exists.
//!
//! Unlike the rmux path there is no status guessing here: dsh reports
//! `running` directly, and hands over a generated title and token counts.

use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Pane ids are prefixed so the rest of the server can tell at a glance which
/// backend owns one, and route sends accordingly.
pub const PANE_PREFIX: &str = "dsh:";

/// Default address of `dsh web`.
const DEFAULT_BASE: &str = "http://127.0.0.1:3080";

/// Probed once at startup, then treated as fixed for the process lifetime.
static AVAILABLE: AtomicBool = AtomicBool::new(false);
static PROBED: AtomicBool = AtomicBool::new(false);

/// `dsh web` address, overridable for a non-default port.
fn base_url() -> String {
    std::env::var("AMUX_DSH_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_BASE.to_string())
}

/// True when a `dsh web` server answered the probe.
pub fn available() -> bool {
    if !PROBED.load(Ordering::Relaxed) {
        let ok = probe();
        AVAILABLE.store(ok, Ordering::Relaxed);
        PROBED.store(true, Ordering::Relaxed);
    }
    AVAILABLE.load(Ordering::Relaxed)
}

/// Ask the host to describe itself — the cheapest call that proves the server
/// is a real dsh and not something else squatting on the port.
fn probe() -> bool {
    rpc::<HostDescribe>("host.describe", serde_json::json!({})).is_some()
}

/// One RPC round-trip.
///
/// dsh speaks a small envelope over plain POST: every request carries a type
/// tag, a correlation id and a `payload`, and every reply wraps the value in
/// `result.ok` / `result.value`.
fn rpc<T: for<'de> Deserialize<'de>>(method: &str, payload: serde_json::Value) -> Option<T> {
    let body = serde_json::json!({
        "type": "client-request",
        // The id only has to be unique within a connection; dsh echoes it back
        // and we make one call at a time, so a constant is fine.
        "rpcId": "amux",
        "method": method,
        "payload": payload,
    });

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .ok()?;
    let text = client
        .post(format!("{}/api/{method}", base_url()))
        .json(&body)
        .send()
        .ok()?
        .text()
        .ok()?;

    let env: Envelope<T> = serde_json::from_str(&text).ok()?;
    if !env.result.ok {
        return None;
    }
    env.result.value
}

#[derive(Deserialize)]
struct Envelope<T> {
    result: RpcResult<T>,
}

#[derive(Deserialize)]
struct RpcResult<T> {
    ok: bool,
    #[serde(default = "none")]
    value: Option<T>,
}

fn none<T>() -> Option<T> {
    None
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HostDescribe {
    #[allow(dead_code)]
    version: Option<String>,
}

#[derive(Deserialize)]
struct SessionList {
    #[serde(default)]
    items: Vec<RawSession>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSession {
    session_id: String,
    #[serde(default)]
    running: bool,
    /// True until the session has any content — those are placeholders the UI
    /// created but nobody has spoken to yet.
    #[serde(default)]
    blank: bool,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    agent_preset: String,
    #[serde(default)]
    projections: Projections,
}

#[derive(Deserialize, Default)]
struct Projections {
    #[serde(default)]
    values: ProjectionValues,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ProjectionValues {
    /// dsh generates this from the opening exchange.
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    session_stats: SessionStats,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct SessionStats {
    #[serde(default)]
    turns: u64,
}

/// A dsh session, shaped like the rest of the server expects a pane to look.
pub struct Bridged {
    /// Prefixed pane id, e.g. `dsh:session-17ce5dcb-…`.
    pub id: String,
    /// Synthesised amux-style session name so existing clients pick the right
    /// avatar and project label with no change.
    pub session: String,
    pub cwd: String,
    pub title: String,
    /// True while dsh is producing a turn.
    pub running: bool,
    pub turns: u64,
    /// Which agent preset is driving it (`standard`, `code`, …).
    pub preset: String,
}

/// Every non-blank session dsh currently holds.
///
/// Blank sessions are skipped: they are empty shells the UI opens on "new
/// session" and would show up as phantom projects on the phone.
pub fn collect() -> Vec<Bridged> {
    if !available() {
        return Vec::new();
    }
    let list: SessionList = match rpc("session.list", serde_json::json!({})) {
        Some(l) => l,
        None => return Vec::new(),
    };

    list.items
        .into_iter()
        .filter(|s| !s.blank)
        .map(|s| {
            let title = s
                .projections
                .values
                .title
                .clone()
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_else(|| "(未命名会话)".to_string());
            Bridged {
                id: format!("{PANE_PREFIX}{}", s.session_id),
                session: session_name_for(&s.cwd),
                cwd: s.cwd.clone(),
                title,
                running: s.running,
                turns: s.projections.values.session_stats.turns,
                preset: s.agent_preset,
            }
        })
        .collect()
}

/// Name a dsh session the way `amux run` names an rmux one, so the client's
/// existing project grouping and avatar logic apply unchanged. `ds` marks the
/// backend, mirroring `cc` / `cx`.
fn session_name_for(cwd: &str) -> String {
    let path = std::path::Path::new(cwd);
    crate::session::session_name("ds", path)
}

/// True when this pane id belongs to dsh.
pub fn owns(pane_id: &str) -> bool {
    pane_id.starts_with(PANE_PREFIX)
}

/// Recover the dsh session id from a prefixed pane id.
pub fn session_id_of(pane_id: &str) -> Option<String> {
    pane_id.strip_prefix(PANE_PREFIX).map(str::to_string)
}

/// Send a message into a dsh session.
///
/// `mode: "queue"` is what the web UI itself sends: the turn is accepted even
/// when one is already running, rather than being rejected.
pub fn send(pane_id: &str, text: &str) -> Result<(), String> {
    let id = session_id_of(pane_id).ok_or_else(|| format!("not a dsh pane: {pane_id}"))?;
    let accepted: Accepted = rpc(
        "session.prompt",
        serde_json::json!({
            "sessionId": id,
            "mode": "queue",
            "content": [{ "type": "text", "text": text }],
        }),
    )
    .ok_or_else(|| "dsh rejected the prompt".to_string())?;

    if accepted.accepted {
        Ok(())
    } else {
        Err("dsh did not accept the prompt".to_string())
    }
}

#[derive(Deserialize)]
struct Accepted {
    #[serde(default)]
    accepted: bool,
}

/// Rendered conversation for the detail view, newest last.
///
/// Cached briefly: the snapshot loop and an open detail page would otherwise
/// ask for the same history several times a second.
static TAIL_CACHE: LazyLock<Mutex<std::collections::HashMap<String, (Instant, String)>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

const TAIL_TTL: Duration = Duration::from_millis(900);

pub fn tail(pane_id: &str) -> String {
    let Some(id) = session_id_of(pane_id) else {
        return String::new();
    };
    if let Some((at, cached)) = TAIL_CACHE
        .lock()
        .expect("dsh tail cache poisoned")
        .get(&id)
        .cloned()
    {
        if at.elapsed() < TAIL_TTL {
            return cached;
        }
    }

    let text = rpc::<History>("session.history", serde_json::json!({ "sessionId": id }))
        .map(|h| render(&h))
        .unwrap_or_default();

    TAIL_CACHE
        .lock()
        .expect("dsh tail cache poisoned")
        .insert(id, (Instant::now(), text.clone()));
    text
}

/// dsh returns an event log, not a message list: every turn is a stream of
/// typed events and the conversation has to be picked out of it.
#[derive(Deserialize)]
struct History {
    #[serde(default)]
    events: Vec<EventEnvelope>,
}

#[derive(Deserialize)]
struct EventEnvelope {
    event: Event,
}

#[derive(Deserialize)]
struct Event {
    #[serde(default)]
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    data: serde_json::Value,
}

/// Flatten the event log into the plain text the pane log renders.
///
/// Only the two settled message events are used. `assistant/chunk` carries the
/// same text again token by token (252 events for a 3-turn conversation), and
/// the rest is instrumentation.
fn render(h: &History) -> String {
    let mut out = String::new();
    for e in &h.events {
        let (prefix, content) = match e.event.kind.as_str() {
            // Plugins and the skill catalogue inject their context as
            // user-role messages — dsh tags the real ones `source.kind: user`.
            // Without this a session's log is mostly system-reminder boilerplate.
            "user/message" => {
                if e.event.data.pointer("/source/kind").and_then(|k| k.as_str()) != Some("user") {
                    continue;
                }
                ("› ", e.event.data.get("content"))
            }
            "assistant/message" => ("", e.event.data.pointer("/message/content")),
            _ => continue,
        };
        let Some(content) = content else { continue };
        let text = extract_text(content);
        if text.trim().is_empty() {
            continue;
        }
        out.push_str(prefix);
        out.push_str(text.trim());
        out.push_str("\n\n");
    }
    out
}

/// Display text from a content value: a bare string, or an array of typed
/// parts. `reasoning` parts are the model's private thinking — dsh's own UI
/// hides them behind a disclosure, so they don't belong in a log tail.
fn extract_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter(|p| p.get("type").and_then(|t| t.as_str()) != Some("reasoning"))
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owns_only_prefixed_ids() {
        assert!(owns("dsh:session-abc"));
        assert!(!owns("%7"));
        assert!(!owns("herdr:default:w1:p1"));
    }

    #[test]
    fn round_trips_the_session_id() {
        assert_eq!(
            session_id_of("dsh:session-17ce5dcb").as_deref(),
            Some("session-17ce5dcb")
        );
        assert_eq!(session_id_of("%7"), None);
    }

    #[test]
    fn session_name_matches_the_cli_convention() {
        // Same shape as `amux run` produces, so the client groups dsh sessions
        // under the same project as the rmux ones.
        let n = session_name_for("/tmp/myproject");
        assert!(n.starts_with("ds_"), "got {n}");
        assert_eq!(n.split('_').count(), 3, "got {n}");
    }

    #[test]
    fn extracts_text_from_both_content_shapes() {
        assert_eq!(extract_text(&serde_json::json!("hi")), "hi");
        assert_eq!(
            extract_text(&serde_json::json!([
                {"type": "text", "text": "a"},
                {"type": "text", "text": "b"}
            ])),
            "ab"
        );
        // Tool calls and other non-text parts contribute nothing.
        assert_eq!(extract_text(&serde_json::json!([{"type": "tool"}])), "");
        // Reasoning parts are dropped even when they carry text.
        assert_eq!(
            extract_text(&serde_json::json!([
                {"type": "reasoning", "text": "thinking"},
                {"type": "text", "text": "answer"}
            ])),
            "answer"
        );
        assert_eq!(extract_text(&serde_json::json!(null)), "");
    }

    #[test]
    fn renders_a_conversation_from_the_event_log() {
        // Shape captured verbatim from `dsh web` 0.1.0-rc.6.
        let raw = r#"{"events":[
          {"event":{"type":"turn/start","data":{}}},
          {"event":{"type":"user/message","data":{"source":{"kind":"user"},"content":[{"type":"text","text":"用一句话说明你是谁"}]}}},
          {"event":{"type":"user/message","data":{"source":{"kind":"plugin"},"content":[{"type":"text","text":"Current runtime context…"}]}}},
          {"event":{"type":"user/message","data":{"source":{"kind":"skill-catalog"},"content":[{"type":"text","text":"<system-reminder>"}]}}},
          {"event":{"type":"assistant/chunk","data":{"delta":"我"}}},
          {"event":{"type":"assistant/message","data":{"message":{"role":"assistant","content":[
              {"type":"reasoning","text":"The user asks in Chinese."},
              {"type":"text","text":"我是编码智能体。"}]}}}},
          {"event":{"type":"turn/end","data":{}}}
        ]}"#;
        let h: History = serde_json::from_str(raw).unwrap();
        let out = render(&h);

        assert!(out.contains("› 用一句话说明你是谁"));
        assert!(out.contains("我是编码智能体。"));
        // The model's private thinking is hidden in dsh's own UI; it must not
        // leak into the log tail.
        assert!(!out.contains("The user asks in Chinese"));
        // Plugin and skill-catalogue injections arrive as user-role messages
        // but are not things the user said.
        assert!(!out.contains("Current runtime context"));
        assert!(!out.contains("system-reminder"));
        // Streaming chunks repeat the settled message — counting them twice
        // would double every reply.
        assert_eq!(out.matches("我是编码智能体。").count(), 1);
        assert_eq!(out.matches("\n\n").count(), 2);
    }

    #[test]
    fn an_empty_log_renders_nothing() {
        let h: History = serde_json::from_str(r#"{"events":[]}"#).unwrap();
        assert!(render(&h).is_empty());
    }

    #[test]
    fn parses_a_real_session_list_payload() {
        // Captured verbatim from `dsh web` 0.1.0-rc.6.
        let raw = r#"{"result":{"ok":true,"value":{"items":[
          {"sessionId":"session-17ce5dcb","updatedAt":1786773128994,"running":false,
           "blank":false,"cwd":"/private/tmp/dsh-probe","agentPreset":"standard",
           "projections":{"asOfSeq":2,"values":{
             "sessionStats":{"turns":2,"steps":2},
             "title":"AI助手身份简介",
             "tokenUsage":{"outputTokens":93}}}}]}}}"#;
        let env: Envelope<SessionList> = serde_json::from_str(raw).unwrap();
        let items = env.result.value.unwrap().items;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].session_id, "session-17ce5dcb");
        assert_eq!(items[0].projections.values.title.as_deref(), Some("AI助手身份简介"));
        assert_eq!(items[0].projections.values.session_stats.turns, 2);
        assert!(!items[0].running);
    }

    #[test]
    fn a_failed_rpc_yields_nothing() {
        let raw = r#"{"result":{"ok":false,"error":{"code":"bad-request"}}}"#;
        let env: Envelope<SessionList> = serde_json::from_str(raw).unwrap();
        assert!(!env.result.ok);
    }
}


/// Path prefixes the dsh frontend requests with absolute URLs.
///
/// Its HTML and plugin manifest reference `/assets/…`, `/plugins/…` and
/// `/api/…` from the origin root, so mounting the UI under `/dsh/` only works
/// if these are also served from the root. Proxying them by prefix avoids
/// rewriting HTML — and, more importantly, avoids missing the plugin URLs the
/// client loads dynamically at runtime.
pub const PROXY_PREFIXES: &[&str] = &["/assets/", "/plugins/", "/api/", "/client"];

/// True when a request path belongs to the dsh frontend rather than amux.
///
/// `/api/` is deliberately excluded here: amux has its own `/api/` routes,
/// which are matched first by the router. Only paths that fall through to the
/// fallback reach this.
pub fn owns_path(path: &str) -> bool {
    PROXY_PREFIXES.iter().any(|p| path.starts_with(p))
        || path == "/favicon.svg"
        || path == "/manifest.webmanifest"
}

/// Forward one request to `dsh web` and return its response verbatim.
///
/// The Origin header is rewritten to dsh's own address: it guards `/api` with
/// a browser-trust fence that rejects any other origin, and a WebView loading
/// the UI from amux would otherwise be refused.
pub async fn proxy(
    method: reqwest::Method,
    path_and_query: &str,
    headers: &[(String, String)],
    body: Vec<u8>,
) -> Result<(u16, Vec<(String, String)>, Vec<u8>), String> {
    let base = base_url();
    let url = format!("{base}{path_and_query}");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;

    let mut req = client.request(method, &url);
    for (k, v) in headers {
        let lower = k.to_ascii_lowercase();
        // Hop-by-hop and identity headers must not be forwarded as-is.
        if matches!(
            lower.as_str(),
            "host" | "origin" | "referer" | "connection" | "content-length" | "accept-encoding"
        ) {
            continue;
        }
        req = req.header(k, v);
    }
    req = req.header("Origin", &base).header("Referer", format!("{base}/"));
    if !body.is_empty() {
        req = req.body(body);
    }

    let res = req.send().await.map_err(|e| format!("dsh proxy: {e}"))?;
    let status = res.status().as_u16();
    let out_headers: Vec<(String, String)> = res
        .headers()
        .iter()
        .filter(|(k, _)| {
            !matches!(
                k.as_str(),
                "connection" | "transfer-encoding" | "content-length"
            )
        })
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.to_string(), v.to_string())))
        .collect();
    let bytes = res.bytes().await.map_err(|e| e.to_string())?.to_vec();
    Ok((status, out_headers, bytes))
}

#[cfg(test)]
mod proxy_tests {
    use super::*;

    #[test]
    fn recognises_frontend_paths() {
        assert!(owns_path("/assets/index-Dqw48FrP.js"));
        assert!(owns_path("/plugins/@deepseek-ai/dsh-client-runtime/client.js?rev=1"));
        assert!(owns_path("/favicon.svg"));
        assert!(owns_path("/manifest.webmanifest"));
        // amux's own UI must keep serving these.
        assert!(!owns_path("/main.dart.js"));
        assert!(!owns_path("/index.html"));
        assert!(!owns_path("/"));
    }
}

/// `ws://…` form of the dsh base address, for proxying its event streams.
pub fn ws_base() -> String {
    base_url().replacen("http://", "ws://", 1).replacen("https://", "wss://", 1)
}

/// Origin header value dsh's trust fence accepts.
pub fn origin_header() -> reqwest::header::HeaderValue {
    reqwest::header::HeaderValue::from_str(&base_url())
        .unwrap_or_else(|_| reqwest::header::HeaderValue::from_static(DEFAULT_BASE))
}
