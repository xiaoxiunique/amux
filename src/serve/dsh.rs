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


/// Authority (`host:port`) of the dsh server, for the forwarder to dial.
fn authority() -> String {
    base_url()
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/')
        .to_string()
}

/// Port the relay ended up on, for clients to discover via /api/capabilities.
static RELAY_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

/// The relay's port, or `None` when it isn't running.
pub fn relay_port() -> Option<u16> {
    match RELAY_PORT.load(Ordering::Relaxed) {
        0 => None,
        p => Some(p),
    }
}

/// Relay `<host>:<listen_port>` to the dsh server.
///
/// dsh binds loopback only and rejects `--host 0.0.0.0` outright, so a phone
/// cannot reach its UI. Everything is copied through untouched except `Host`
/// and `Origin`, which are pointed at dsh's own address — see
/// [`relay_connection`]. That keeps `--trusted-host` off the user's plate: the
/// relay works from a LAN address, a Tailscale address or anything else,
/// without a flag to update every time the network changes.
///
/// # Exposure
///
/// This does what dsh declines to do for you — its UI can run arbitrary code,
/// and the relay has no authentication of its own, so amux's `--token` does
/// not cover it. It binds the same host `amux serve` does, so
/// `amux serve --host 127.0.0.1` keeps it local too.
pub async fn spawn_forwarder(host: &str, listen_port: u16) {
    let target = authority();
    let listener = match tokio::net::TcpListener::bind((host, listen_port)).await {
        Ok(l) => l,
        Err(error) => {
            eprintln!("[dsh] cannot listen on {host}:{listen_port}: {error}");
            return;
        }
    };
    RELAY_PORT.store(listen_port, Ordering::Relaxed);
    let tls = tls_config();
    let scheme = if tls.is_some() { "https" } else { "http" };
    println!("dsh UI relayed on port {listen_port} ({scheme}) -> {target}");
    if tls.is_none() && host != "127.0.0.1" && host != "localhost" {
        // Worth saying plainly: without TLS the UI simply will not work from
        // another device, because the browser blocks its WebSocket.
        println!("  no TLS cert in ~/.amux/dsh-{{cert,key}}.pem — the UI will only");
        println!("  work over loopback; browsers block WebSockets from a plain-HTTP");
        println!("  page on any other address");
    }
    if host != "127.0.0.1" && host != "localhost" {
        println!("  note: unauthenticated, and dsh can run code — trusted networks only");
    }

    loop {
        let Ok((inbound, _)) = listener.accept().await else {
            continue;
        };
        let target = target.clone();
        match tls.clone() {
            Some(cfg) => {
                tokio::spawn(async move {
                    let acceptor = tokio_rustls::TlsAcceptor::from(cfg);
                    match acceptor.accept(inbound).await {
                        Ok(stream) => relay_stream(stream, target).await,
                        // A failed handshake is usually a probe or a client
                        // that gave up; nothing to report.
                        Err(_) => {}
                    }
                });
            }
            None => {
                tokio::spawn(relay_connection(inbound, target));
            }
        }
    }
}

/// Carry one connection, rewriting only `Host` and `Origin` on the way up.
///
/// dsh accepts an `/api` call when the Host is loopback (or listed with
/// `--trusted-host`), and when any Origin present agrees with it. Pointing both
/// at dsh's own address means the relay works from any client address —
/// LAN, Tailscale, whatever — with no flags to keep in sync.
///
/// Everything else is copied untouched, and the moment a connection upgrades
/// (WebSocket) it becomes a plain byte pipe again.
async fn relay_connection(inbound: tokio::net::TcpStream, target: String) {
    relay_stream(inbound, target).await
}

/// Same relay over any stream, so a TLS-wrapped connection reuses it verbatim.
async fn relay_stream<S>(inbound: S, target: String)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    let (client_read, client_write) = tokio::io::split(inbound);

    // Both the proxied responses and the relay's own replies go to the same
    // socket, so they share it under a lock.
    let client_out = std::sync::Arc::new(tokio::sync::Mutex::new(client_write));

    // The upstream is opened on demand, not up front. dsh advertises
    // `Keep-Alive: timeout=5`, and the relay answers `/` itself — so an eager
    // connection would sit idle through the whole page load and be closed
    // before the first API call reached it. That call then vanished into a
    // dead socket, which is what left the session list empty on a slower
    // network while a fast one happened to finish inside the five seconds.
    let mut upstream: Option<tokio::net::tcp::OwnedWriteHalf> = None;
    let upstream_alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    let mut reader = BufReader::new(client_read);
    loop {
        // --- request line + headers ---
        let mut head = Vec::new();
        let mut upgrading = false;
        let mut content_length = 0usize;
        let mut chunked = false;
        let mut request_line = String::new();

        loop {
            let mut line = Vec::new();
            match reader.read_until(b'\n', &mut line).await {
                Ok(0) => return, // client hung up
                Ok(_) => {}
                Err(_) => return,
            }
            if request_line.is_empty() {
                request_line = String::from_utf8_lossy(&line).trim().to_string();
            }
            let is_blank = line == b"\r\n" || line == b"\n";
            let lower = String::from_utf8_lossy(&line).to_ascii_lowercase();

            if lower.starts_with("host:") {
                head.extend_from_slice(format!("Host: {target}\r\n").as_bytes());
            } else if lower.starts_with("origin:") {
                head.extend_from_slice(format!("Origin: http://{target}\r\n").as_bytes());
            } else if lower.starts_with("sec-fetch-site:") {
                // A cross-site marker is refused outright; after the rewrite
                // above the request genuinely is same-origin.
                head.extend_from_slice(b"Sec-Fetch-Site: same-origin\r\n");
            } else {
                if lower.starts_with("upgrade:") {
                    upgrading = true;
                } else if let Some(v) = lower.strip_prefix("content-length:") {
                    content_length = v.trim().parse().unwrap_or(0);
                } else if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
                    chunked = true;
                }
                head.extend_from_slice(&line);
            }
            if is_blank {
                break;
            }
        }

        // Two paths the relay answers itself.
        let (method, path) = split_request_line(&request_line);
        if path.starts_with(UI_STATE_PATH) {
            let mut body = vec![0u8; content_length];
            if content_length > 0 && reader.read_exact(&mut body).await.is_err() {
                return;
            }
            if method == "POST" {
                save_ui_state(&String::from_utf8_lossy(&body));
                if write_simple(&client_out, "application/json", b"{\"ok\":true}")
                    .await
                    .is_err()
                {
                    return;
                }
            } else {
                let state = load_ui_state();
                if write_simple(&client_out, "application/json", state.as_bytes())
                    .await
                    .is_err()
                {
                    return;
                }
            }
            continue;
        }
        if method == "GET" && (path == "/" || path == "/index.html") {
            if let Some(html) = injected_index().await {
                if write_simple(&client_out, "text/html; charset=utf-8", &html)
                    .await
                    .is_err()
                {
                    return;
                }
                continue;
            }
            // Injection failed (dsh restarting?) — fall through and proxy it.
        }

        // Connect (or reconnect) only now that something must actually go
        // upstream. A connection dsh dropped while idle is replaced instead of
        // being written into.
        if upstream.is_some() && !upstream_alive.load(std::sync::atomic::Ordering::Relaxed) {
            upstream = None;
        }
        if upstream.is_none() {
            let Ok(sock) = tokio::net::TcpStream::connect(&target).await else {
                return;
            };
            let (mut server_read, server_write) = sock.into_split();
            upstream = Some(server_write);
            upstream_alive.store(true, std::sync::atomic::Ordering::Relaxed);

            let sink = client_out.clone();
            let alive = upstream_alive.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16 * 1024];
                loop {
                    match server_read.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let mut w = sink.lock().await;
                            if w.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                alive.store(false, std::sync::atomic::Ordering::Relaxed);
            });
        }
        let server_write = upstream.as_mut().expect("upstream just ensured");

        if server_write.write_all(&head).await.is_err() {
            return;
        }

        // --- body ---
        if chunked {
            // Rare from this client, and framing it wrongly would corrupt the
            // stream; hand the rest over verbatim instead.
            let _ = tokio::io::copy(&mut reader, server_write).await;
            return;
        }
        if content_length > 0 {
            let mut body = vec![0u8; content_length];
            if reader.read_exact(&mut body).await.is_err() {
                return;
            }
            if server_write.write_all(&body).await.is_err() {
                return;
            }
        }

        if upgrading {
            // Past the handshake this is no longer HTTP.
            let _ = tokio::io::copy(&mut reader, server_write).await;
            return;
        }
    }
}

/// Path the relay answers itself, for sharing UI state across origins.
///
/// Served from the relay's own port so the page can call it without CORS.
const UI_STATE_PATH: &str = "/__amux/dsh-ui-state";

fn ui_state_file() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".amux").join("dsh-ui-state.json"))
}

fn load_ui_state() -> String {
    ui_state_file()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .filter(|s| s.trim_start().starts_with('{'))
        .unwrap_or_else(|| "{}".to_string())
}

fn save_ui_state(body: &str) {
    // Only accept a JSON object; anything else would break the page on load.
    if !body.trim_start().starts_with('{') {
        return;
    }
    if let Some(p) = ui_state_file() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, body);
    }
}

/// Script injected into dsh's page so every origin sees the same UI state.
///
/// dsh keeps "which session am I looking at", per-session drafts and the
/// sidebar layout in localStorage, which browsers scope per origin — so the
/// same dsh reached at `127.0.0.1:3080` and at a Tailscale address behaves
/// like two installs. The session data itself is server-side and was always
/// shared; only this view state was not.
///
/// The stored state is embedded rather than fetched: the script then runs
/// synchronously ahead of dsh's bundle, so the app reads the restored values
/// instead of racing them.
fn injection(state: &str) -> String {
    format!(
        r#"<script>(function(){{
try {{
  var shared = {state};
  for (var k in shared) {{ try {{ localStorage.setItem(k, shared[k]); }} catch (e) {{}} }}
}} catch (e) {{}}
var timer = null;
function push() {{
  var out = {{}};
  for (var i = 0; i < localStorage.length; i++) {{
    var k = localStorage.key(i);
    if (k && k.indexOf('dsh.') === 0) out[k] = localStorage.getItem(k);
  }}
  fetch('{UI_STATE_PATH}', {{
    method: 'POST',
    headers: {{ 'content-type': 'application/json' }},
    body: JSON.stringify(out)
  }}).catch(function () {{}});
}}
var origSet = localStorage.setItem.bind(localStorage);
localStorage.setItem = function (k, v) {{
  origSet(k, v);
  // Coalesce: dsh writes several keys per interaction.
  if (String(k).indexOf('dsh.') === 0) {{
    clearTimeout(timer);
    timer = setTimeout(push, 400);
  }}
}};
}})();</script>"#
    )
}

/// Fetch dsh's root document and return it with the sync script inserted.
async fn injected_index() -> Option<Vec<u8>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .ok()?;
    let base = base_url();
    let html = client
        .get(&base)
        .header("Host", authority())
        .header("Origin", &base)
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()?;

    // Right after <head> so it precedes every script dsh loads.
    let script = injection(&load_ui_state());
    let out = match html.find("<head>") {
        Some(i) => {
            let mut s = String::with_capacity(html.len() + script.len());
            s.push_str(&html[..i + 6]);
            s.push_str(&script);
            s.push_str(&html[i + 6..]);
            s
        }
        // No <head> means dsh restructured its shell; serve it unchanged
        // rather than guessing where the script belongs.
        None => html,
    };
    Some(out.into_bytes())
}

/// `("GET", "/path")` from a request line, empty on anything malformed.
fn split_request_line(line: &str) -> (&str, &str) {
    let mut parts = line.split(' ');
    (parts.next().unwrap_or(""), parts.next().unwrap_or(""))
}

/// Write a complete, self-contained HTTP response to the client.
async fn write_simple(
    sink: &std::sync::Arc<tokio::sync::Mutex<impl tokio::io::AsyncWrite + Unpin>>,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: keep-alive\r\n\r\n",
        body.len()
    );
    let mut w = sink.lock().await;
    w.write_all(head.as_bytes()).await?;
    w.write_all(body).await
}

/// Where the relay looks for a TLS certificate.
///
/// Browsers refuse to open a WebSocket from a page served over plain HTTP on
/// anything but a loopback address — which is exactly how the phone reaches
/// this. Serving the relay over TLS is the only way the dsh UI works off this
/// machine at all.
///
/// A Tailscale-issued certificate is used when one is present: it is publicly
/// trusted, so nothing has to be installed on the phone. Obtain one with:
///
/// ```text
/// tailscale cert --cert-file ~/.amux/dsh-cert.pem \
///                --key-file  ~/.amux/dsh-key.pem  <machine>.<tailnet>.ts.net
/// ```
fn tls_paths() -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let dir = dirs::home_dir()?.join(".amux");
    let cert = dir.join("dsh-cert.pem");
    let key = dir.join("dsh-key.pem");
    (cert.is_file() && key.is_file()).then_some((cert, key))
}

/// True when the relay will serve HTTPS.
pub fn tls_enabled() -> bool {
    tls_paths().is_some()
}

/// The hostname the certificate is issued for.
///
/// TLS only validates against the name in the certificate, so a client that
/// built the URL from an IP would fail the handshake. Reporting the name lets
/// it use one that matches.
pub fn tls_host() -> Option<String> {
    let (cert_path, _) = tls_paths()?;
    let pem = std::fs::read(&cert_path).ok()?;
    let cert = rustls_pemfile::certs(&mut pem.as_slice())
        .next()?
        .ok()?;
    // Read the first DNS name out of the subjectAltName extension. Parsing the
    // whole certificate would need another dependency; the name is a plain
    // IA5String, and Tailscale's certs carry exactly one.
    let der = cert.as_ref();
    let host = find_first_dns_name(der)?;
    Some(host)
}

/// Scan DER for the SAN extension's first dNSName (context tag 0x82).
fn find_first_dns_name(der: &[u8]) -> Option<String> {
    let mut i = 0;
    while i + 2 < der.len() {
        if der[i] == 0x82 {
            let len = der[i + 1] as usize;
            if len > 3 && i + 2 + len <= der.len() {
                if let Ok(s) = std::str::from_utf8(&der[i + 2..i + 2 + len]) {
                    // A hostname, not arbitrary bytes that happened to match.
                    if s.contains('.')
                        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
                    {
                        return Some(s.to_string());
                    }
                }
            }
        }
        i += 1;
    }
    None
}

/// Load the certificate into a rustls config.
fn tls_config() -> Option<std::sync::Arc<tokio_rustls::rustls::ServerConfig>> {
    let (cert_path, key_path) = tls_paths()?;

    // rustls refuses to pick a backend on its own when more than one could
    // apply. reqwest already pulls in ring, so name it explicitly.
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    });

    let certs: Vec<_> = rustls_pemfile::certs(&mut std::io::BufReader::new(
        std::fs::File::open(&cert_path).ok()?,
    ))
    .collect::<Result<_, _>>()
    .ok()?;
    let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(
        std::fs::File::open(&key_path).ok()?,
    ))
    .ok()??;

    let config = tokio_rustls::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .ok()?;
    Some(std::sync::Arc::new(config))
}
