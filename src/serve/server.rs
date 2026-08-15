use std::{
    collections::HashMap,
    env, fs,
    io::{Read, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    sync::{LazyLock, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::{Body, Bytes},
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::{header, HeaderMap, Response, StatusCode, Uri},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use include_dir::{include_dir, Dir};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{broadcast, mpsc};

const DEFAULT_PORT: u16 = 8787;
const DEFAULT_HOST: &str = "0.0.0.0";
const FIELD_SEPARATOR: &str = "\t";
/// A pane's agent session file written within this many seconds counts as
/// "actively working" (poll cadence is 2.5s; small enough to flip to idle
/// promptly, large enough not to flap across brief think/tool gaps).
const RUNNING_WINDOW_SECS: f64 = 8.0;

static BUFFER_COUNTER: AtomicU64 = AtomicU64::new(0);
static PANE_ACTIVITY: LazyLock<Mutex<HashMap<String, PaneActivity>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static MESSAGE_CACHE: LazyLock<Mutex<HashMap<String, Vec<InteractionMessage>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static PENDING_INTERPRETATIONS: LazyLock<Mutex<HashMap<String, ()>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
// Pending-message queue (Claude Code only): messages the user sent while the
// agent was busy are held here, keyed by pane id, and flushed when the pane
// goes Idle/Done. See `flush_pending_messages`.
static PENDING_MESSAGES: LazyLock<Mutex<HashMap<String, Vec<PendingMessage>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
// Last status computed per pane by `build_snapshot`; read by `api_send` to
// decide whether a Claude pane is currently busy (so it should queue).
static PANE_STATUS_CACHE: LazyLock<Mutex<HashMap<String, PaneStatus>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
// Cooldown after a flush, so multi-queued messages are delivered one per idle
// cycle (giving the agent time to start working) rather than dumped together.
static PENDING_FLUSH_AT: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static PENDING_ID_COUNTER: AtomicU64 = AtomicU64::new(0);
/// Persistent sysinfo handle for cross-platform CPU/memory sampling. Kept alive
/// so CPU% is measured as the delta between snapshot polls.
static SYSINFO: LazyLock<Mutex<sysinfo::System>> =
    LazyLock::new(|| Mutex::new(sysinfo::System::new()));
static PANE_LOG_REFRESH_BURST_IDS: LazyLock<Mutex<HashMap<String, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static CC_SWITCH_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static PROJECT_HISTORY_CACHE: LazyLock<Mutex<Option<Vec<ProjectHistoryEntry>>>> =
    LazyLock::new(|| Mutex::new(None));
static DEVICE_INFO_CACHE: LazyLock<Option<DeviceInfo>> =
    LazyLock::new(collect_device_info_uncached);
static TMUX_PROGRAM_PATH: LazyLock<String> = LazyLock::new(resolve_tmux_program_path);
static PANE_LOG_REFRESH_BURST_COUNTER: AtomicU64 = AtomicU64::new(0);
static SNAPSHOT_REFRESH_COUNTER: AtomicU64 = AtomicU64::new(0);
const PANE_LOG_REFRESH_BURST_DELAYS_MS: &[u64] = &[0, 80, 180, 360, 700, 1200, 2200, 3800];
const PANE_COMMAND_TAIL_SETTLE_DELAYS_MS: &[u64] = &[0, 80, 180, 360, 700];
const PANE_COMMAND_TAIL_LINE_COUNT: usize = 800;

struct PaneActivity {
    tail_hash: u64,
    changed_at: Instant,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
enum InteractionRole {
    Agent,
    User,
    System,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
enum InteractionKind {
    Summary,
    Status,
    Question,
    PermissionRequest,
    Progress,
    Error,
    Done,
    Notification,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "lowercase")]
enum InteractionPriority {
    Low,
    Normal,
    High,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "lowercase")]
enum InteractionActionStyle {
    Default,
    Destructive,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct InteractionAction {
    label: String,
    payload: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    style: Option<InteractionActionStyle>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct InteractionSource {
    #[serde(rename = "type")]
    source_type: String,
    excerpt: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct InteractionMessage {
    id: String,
    pane_id: String,
    role: InteractionRole,
    kind: InteractionKind,
    priority: InteractionPriority,
    title: String,
    body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    actions: Option<Vec<InteractionAction>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<InteractionSource>,
    created_at: String,
}

#[derive(Clone)]
pub(crate) struct AppState {
    token: String,
    snapshots: broadcast::Sender<serde_json::Value>,
    pane_log_refreshes: broadcast::Sender<String>,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PaneStatus {
    Running,
    Waiting,
    Idle,
    Failed,
    Done,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Pane {
    pub(crate) id: String,
    target: String,
    pub(crate) session: String,
    window_index: String,
    window_name: String,
    pane_index: String,
    pub(crate) command: String,
    pub(crate) path: String,
    active: bool,
    pid: Option<u32>,
    pub(crate) title: String,
    tail: String,
    pub(crate) status: PaneStatus,
    reason: String,
    updated_at: String,
    /// Seconds since the agent's session file was last written (`null` if no
    /// session file was found). Small = actively working. Real activity signal
    /// for the UI, independent of the coarse status enum.
    activity_age_secs: Option<f64>,
    messages: Vec<InteractionMessage>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct SystemStats {
    cpu_usage: Option<f64>,
    memory_usage: Option<f64>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct DeviceInfo {
    name: Option<String>,
    model_identifier: Option<String>,
    kind: String,
    model_name: String,
}

#[derive(Debug, Serialize, Clone)]
struct Snapshot {
    ok: bool,
    now: String,
    panes: Vec<Pane>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<SystemStats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    device: Option<DeviceInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct BasePane {
    id: String,
    target: String,
    session: String,
    window_index: String,
    window_name: String,
    pane_index: String,
    command: String,
    path: String,
    active: bool,
    pid: Option<u32>,
    title: String,
}

#[derive(Debug)]
struct TmuxOutput {
    stdout: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendRequest {
    pane_id: String,
    text: String,
    enter: Option<bool>,
    submit_key: Option<String>,
    vim_mode: Option<bool>,
    /// When true, bypass the pending queue and send immediately even if the
    /// Claude pane is busy.
    force: Option<bool>,
}

/// One queued message awaiting an idle Claude pane. Serialized for `/api/pending`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PendingMessage {
    id: String,
    text: String,
    created_at: String,
    /// Whether to press the submit key (Enter) after pasting, on flush.
    #[serde(skip)]
    enter: bool,
    #[serde(skip)]
    vim_mode: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingUpdateRequest {
    pane_id: String,
    id: String,
    text: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingDeleteRequest {
    pane_id: String,
    id: String,
}

#[derive(Debug, Deserialize)]
struct RefineTextRequest {
    text: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KillSessionRequest {
    pane_id: Option<String>,
    session: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CcSwitchRequest {
    app_type: String,
    provider_id: String,
}

#[derive(Debug, Deserialize)]
struct CcSwitchProviderRow {
    id: String,
    app_type: String,
    name: String,
    is_current: i64,
    settings_config: String,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct CcSwitchProvider {
    id: String,
    app_type: String,
    name: String,
    is_current: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
    has_api_key: bool,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct CcSwitchApp {
    app_type: String,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    active_provider_id: Option<String>,
    providers: Vec<CcSwitchProvider>,
}

struct ValidatedCcSwitchProvider {
    normalized_config: String,
}

struct PreparedCcSwitchSettingsUpdate {
    settings_path: PathBuf,
    tmp_path: PathBuf,
}

struct CcSwitchDbRollbackState {
    active_provider_ids: Vec<String>,
    proxy_backup_config: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CcSwitchProxyBackupRow {
    original_config: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct ProjectHistoryEntry {
    path: String,
    name: String,
    last_agent: String,
    last_seen_at: String,
    launch_count: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LaunchProjectRequest {
    path: String,
    agent: String,
}

#[derive(Debug, Deserialize)]
struct TerminalMessage {
    #[serde(rename = "type")]
    message_type: Option<String>,
    data: Option<String>,
    cols: Option<u16>,
    rows: Option<u16>,
    lines: Option<i32>,
}

enum TerminalEvent {
    Data(String),
    Exit,
}

/// The embedded Flutter web client (built from agent-port, pruned to a single
/// local canvaskit renderer). Served at `/` so `amux serve` gives a zero-install
/// browser UI with no external files or network fetches.
static WEBUI: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/webui");

fn web_content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("json") => "application/json; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        _ => "application/octet-stream",
    }
}

/// Serve the embedded web client, falling back to index.html for SPA routes.
/// This is the router fallback, so it only runs for paths no `/api` or `/ws`
/// route matched.
async fn serve_webui(uri: Uri) -> Response<Body> {
    let raw = uri.path().trim_start_matches('/');
    let path = if raw.is_empty() { "index.html" } else { raw };
    let (contents, ctype) = match WEBUI.get_file(path) {
        Some(file) => (file.contents(), web_content_type(path)),
        None => match WEBUI.get_file("index.html") {
            Some(index) => (index.contents(), "text/html; charset=utf-8"),
            None => {
                return Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Body::from("web client not bundled"))
                    .unwrap()
            }
        },
    };
    Response::builder()
        .header(header::CONTENT_TYPE, ctype)
        .body(Body::from(Bytes::from_static(contents)))
        .unwrap()
}

/// Start the agent-monitor HTTP+WS server.
/// This is the entry point called from `amux serve --foreground`.
pub async fn run_server(host: &str, port: u16, token: &str) {
    let (snapshots, _) = broadcast::channel(32);
    let (pane_log_refreshes, _) = broadcast::channel(128);

    let state = AppState {
        token: token.to_string(),
        snapshots,
        pane_log_refreshes,
    };

    // Full build: load persisted APNs device tokens + per-pane notify config.
    #[cfg(feature = "full")]
    {
        crate::serve::full::push::load_device_tokens();
        crate::serve::full::push::load_notify_config();
    }

    spawn_snapshot_loop(state.clone());

    let app = Router::new()
        .route("/api/snapshot", get(api_snapshot))
        .route("/api/pane/context", get(api_pane_context))
        .route("/api/send", post(api_send))
        .route("/api/pending", get(api_pending_list))
        .route("/api/pending/update", post(api_pending_update))
        .route("/api/pending/delete", post(api_pending_delete))
        .route("/api/pending/clear", post(api_pending_clear))
        .route("/api/refine-text", post(api_refine_text))
        .route("/api/upload-image", post(api_upload_image))
        .route("/api/key", post(api_key))
        .route("/api/session/kill", post(api_kill_session))
        .route("/api/project-history", get(api_project_history))
        .route(
            "/api/project-history/launch",
            post(api_project_history_launch),
        )
        .route("/api/cc-switch", get(api_cc_switch_status))
        .route("/api/capabilities", get(api_capabilities))
        .route("/api/usb/devices", get(api_usb_devices))
        .route("/api/usb/screenshot", get(api_usb_screenshot))
        .route("/api/files/roots", get(api_files_roots))
        .route("/api/files/list", get(api_files_list))
        .route("/api/files/read", get(api_files_read))
        .route("/api/files/download", get(api_files_download))
        .route("/api/sessions", get(api_sessions))
        .route("/api/sessions/resume", post(api_sessions_resume))
        .route("/api/cron/schedules", get(api_cron_schedules))
        .route("/api/cron/jobs", get(api_cron_jobs))
        .route("/api/cron/jobs/running", get(api_cron_running))
        .route("/api/cron/log", get(api_cron_log))
        .route("/api/cron/action", post(api_cron_action))
        .route("/api/cc-switch/switch", post(api_cc_switch_switch))
        .route("/ws", get(snapshot_ws))
        .route("/pane-log/ws", get(pane_log_ws))
        .route("/terminal/ws", get(terminal_ws));

    // Full build (`--features full`) adds the agent-port host extras:
    // macOS control-center, token-usage, and APNs push.
    #[cfg(feature = "full")]
    let app = {
        use crate::serve::full::{control_center as cc, push, usage};
        app.route("/api/apps", get(cc::api_apps))
            .route("/api/apps/installed", get(cc::api_apps_installed))
            .route("/api/apps/open", post(cc::api_apps_open))
            .route("/api/apps/icon", get(cc::api_apps_icon))
            .route("/api/apps/quit", post(cc::api_apps_quit))
            .route("/api/apps/screenshot", get(cc::api_app_screenshot))
            .route("/api/screen", get(cc::api_screen))
            .route("/api/usage", get(usage::api_usage))
            .route("/api/usage/daily", get(usage::api_usage_daily))
            .route("/api/push/register", post(push::api_push_register))
            .route("/api/push/test", post(push::api_push_test))
            .route("/api/push/status", get(push::api_push_status))
            .route(
                "/api/pane/notify-config",
                get(push::api_notify_config_get).post(push::api_notify_config_set),
            )
    };

    let app = app.fallback(serve_webui).with_state(state.clone());

    let bind_addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .unwrap_or_else(|error| panic!("failed to bind {bind_addr}: {error}"));
    let addr = listener
        .local_addr()
        .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], port)));

    println!("Agent monitor listening on http://{addr}");
    println!("Web UI:  http://localhost:{port}");
    if token.is_empty() {
        println!("Token auth is disabled. Set --token to require a token.");
    } else {
        println!("Token auth is enabled.");
    }

    axum::serve(listener, app).await.expect("server failed");
}

pub(crate) fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn run_tmux(args: &[String]) -> Result<TmuxOutput, String> {
    let output = tmux_command()
        .args(args)
        .output()
        .map_err(|error| format!("failed to run tmux: {error}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let message = if stderr.is_empty() {
            format!("tmux exited with {}", output.status)
        } else {
            stderr
        };
        return Err(message);
    }

    Ok(TmuxOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
    })
}

fn tmux_program_path() -> &'static str {
    TMUX_PROGRAM_PATH.as_str()
}

fn resolve_tmux_program_path() -> String {
    // An explicit full path (must be a real file) always wins.
    if let Some(path) = env::var_os("AGENT_MONITOR_TMUX_PATH")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
    {
        return path.to_string_lossy().into_owned();
    }

    // Default multiplexer (rmux), or whatever AMUX_MUX / AGENT_MONITOR_TMUX_PATH
    // names. The lsof-based server discovery below is a tmux-only macOS hack, so
    // only run it when we're actually driving tmux.
    let bin = crate::tmux::mux_bin();
    if bin.ends_with("tmux") {
        if let Some(path) = discover_tmux_server_program_path() {
            return path;
        }
    }
    bin
}

fn discover_tmux_server_program_path() -> Option<String> {
    let output = sanitized_command("tmux")
        .args(["display-message", "-p", "#{pid}"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let pid = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u32>()
        .ok()?;

    let output = Command::new("lsof")
        .args(["-nP", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find(|line| line.contains(" txt ") && line.contains("/tmux"))
        .and_then(|line| {
            line.split_whitespace()
                .find(|part| part.starts_with('/') && part.ends_with("/tmux"))
        })
        .map(ToString::to_string)
}

fn sanitized_command(program: &str) -> Command {
    let mut command = Command::new(program);
    command
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("TMUX_PROGRAM")
        .env_remove("TMUX_CONF")
        .env_remove("TMUX_CONF_LOCAL")
        .env_remove("TMUX_SOCKET");
    // When serve is started from inside a Claude Code session it inherits that
    // session's markers. An agent launched further down this chain then sees
    // itself as a *child* session and turns transcript saving off — so the
    // conversation is never recorded, and resuming it later finds nothing.
    // These identify one specific running session and must not be handed down.
    for key in [
        "CLAUDECODE",
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_PID",
    ] {
        command.env_remove(key);
    }
    command
}

fn tmux_command() -> Command {
    sanitized_command(tmux_program_path())
}

fn sanitize_tmux_command_builder(command: &mut CommandBuilder) {
    command.env_remove("TMUX");
    command.env_remove("TMUX_PANE");
    command.env_remove("TMUX_PROGRAM");
    command.env_remove("TMUX_CONF");
    command.env_remove("TMUX_CONF_LOCAL");
    command.env_remove("TMUX_SOCKET");
}

fn scroll_tmux_pane(pane_id: &str, lines: i32) {
    if pane_id.is_empty() {
        return;
    }
    let safe_lines = lines.clamp(-200, 200);
    if safe_lines == 0 {
        return;
    }

    let is_in_mode = run_tmux(&[
        "display-message".to_string(),
        "-p".to_string(),
        "-t".to_string(),
        pane_id.to_string(),
        "#{pane_in_mode}".to_string(),
    ])
    .map(|output| output.stdout.trim() == "1")
    .unwrap_or(false);
    if !is_in_mode {
        let _ = run_tmux(&[
            "copy-mode".to_string(),
            "-t".to_string(),
            pane_id.to_string(),
        ]);
    }
    let direction = if safe_lines > 0 {
        "scroll-up"
    } else {
        "scroll-down"
    };
    let _ = run_tmux(&[
        "send-keys".to_string(),
        "-t".to_string(),
        pane_id.to_string(),
        "-X".to_string(),
        "-N".to_string(),
        safe_lines.abs().to_string(),
        direction.to_string(),
    ]);
}

fn exit_tmux_copy_mode(pane_id: &str) {
    if pane_id.is_empty() {
        return;
    }
    let _ = run_tmux(&[
        "send-keys".to_string(),
        "-t".to_string(),
        pane_id.to_string(),
        "-X".to_string(),
        "cancel".to_string(),
    ]);
}

fn is_no_tmux_server_error(error: &str) -> bool {
    error.contains("no server running")
}

fn list_panes() -> Result<Vec<BasePane>, String> {
    let format = [
        "#{session_name}",
        "#{window_index}",
        "#{window_name}",
        "#{pane_index}",
        "#{pane_id}",
        "#{pane_current_command}",
        "#{pane_current_path}",
        "#{pane_active}",
        "#{pane_pid}",
        "#{pane_title}",
    ]
    .join(FIELD_SEPARATOR);

    let result = match run_tmux(&[
        "list-panes".to_string(),
        "-a".to_string(),
        "-F".to_string(),
        format,
    ]) {
        Ok(output) => output,
        Err(error) if is_no_tmux_server_error(&error) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    let panes = result
        .stdout
        .trim()
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut parts = line.split(FIELD_SEPARATOR);
            let session = parts.next().unwrap_or_default().to_string();
            let window_index = parts.next().unwrap_or_default().to_string();
            let window_name = parts.next().unwrap_or_default().to_string();
            let pane_index = parts.next().unwrap_or_default().to_string();
            let id = parts.next().unwrap_or_default().to_string();
            let command = parts.next().unwrap_or_default().to_string();
            let path = parts.next().unwrap_or_default().to_string();
            let active = parts.next().unwrap_or_default() == "1";
            let pid = parts.next().and_then(|value| value.parse::<u32>().ok());
            let title = parts.next().unwrap_or_default().to_string();
            let target = format!("{session}:{window_index}.{pane_index}");
            let title = if title.is_empty() {
                command.clone()
            } else {
                title
            };

            BasePane {
                id,
                target,
                session,
                window_index,
                window_name,
                pane_index,
                command,
                path,
                active,
                pid,
                title,
            }
        })
        .collect();

    Ok(panes)
}

fn capture_pane(pane_id: &str) -> String {
    capture_pane_lines(pane_id, 300)
}

fn capture_pane_lines(pane_id: &str, lines: usize) -> String {
    let safe_lines = lines.clamp(50, 5000);
    let primary = run_tmux(&[
        "capture-pane".to_string(),
        "-p".to_string(),
        "-J".to_string(),
        "-S".to_string(),
        format!("-{safe_lines}"),
        "-t".to_string(),
        pane_id.to_string(),
    ])
    .map(|output| output.stdout.trim_end().to_string())
    .unwrap_or_default();
    if !primary.trim().is_empty() {
        return primary;
    }

    run_tmux(&[
        "capture-pane".to_string(),
        "-p".to_string(),
        "-a".to_string(),
        "-q".to_string(),
        "-J".to_string(),
        "-S".to_string(),
        format!("-{safe_lines}"),
        "-t".to_string(),
        pane_id.to_string(),
    ])
    .map(|output| output.stdout.trim_end().to_string())
    .unwrap_or_default()
}

fn context_line_count(value: Option<&String>) -> usize {
    value
        .and_then(|item| item.parse::<usize>().ok())
        .unwrap_or(1200)
        .clamp(100, 5000)
}

fn pane_log_line_count(value: Option<&String>) -> usize {
    value
        .and_then(|item| item.parse::<usize>().ok())
        .unwrap_or(300)
        .clamp(50, 1000)
}

fn detect_image_upload(image: &[u8]) -> Option<(&'static str, &'static str)> {
    if image.len() >= 3 && image[0] == 0xff && image[1] == 0xd8 && image[2] == 0xff {
        return Some((".jpg", "image/jpeg"));
    }

    if image.len() >= 8
        && image[0] == 0x89
        && image[1] == 0x50
        && image[2] == 0x4e
        && image[3] == 0x47
        && image[4] == 0x0d
        && image[5] == 0x0a
        && image[6] == 0x1a
        && image[7] == 0x0a
    {
        return Some((".png", "image/png"));
    }

    None
}

fn safe_upload_pane_name(pane_id: &str) -> String {
    let safe = pane_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '.' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();

    if safe.is_empty() {
        "unknown".to_string()
    } else {
        safe
    }
}

fn upload_output_dir() -> PathBuf {
    // Use the stable state dir, not the process CWD: when the bundled app
    // launches `amux serve` from Finder/Dock the CWD is `/`, so a CWD-relative
    // `output/mobile-uploads` would try to mkdir under `/` and fail
    // (read-only file system). ~/.agent-monitor is always writable.
    agent_monitor_state_dir().join("mobile-uploads")
}

fn user_home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn agent_monitor_state_dir() -> PathBuf {
    env::var_os("AGENT_MONITOR_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| user_home_dir().join(".agent-monitor"))
}

fn project_history_path() -> PathBuf {
    env::var_os("AGENT_MONITOR_PROJECT_HISTORY_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| agent_monitor_state_dir().join("project-history.json"))
}

fn cc_switch_db_path() -> PathBuf {
    env::var_os("CC_SWITCH_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| user_home_dir().join(".cc-switch").join("cc-switch.db"))
}

fn cc_switch_settings_path() -> PathBuf {
    env::var_os("CC_SWITCH_SETTINGS_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| user_home_dir().join(".cc-switch").join("settings.json"))
}

fn cc_switch_app_path() -> String {
    env::var("CC_SWITCH_APP_PATH").unwrap_or_else(|_| "/Applications/CC Switch.app".to_string())
}

fn cc_switch_skip_restart() -> bool {
    env::var("CC_SWITCH_SKIP_RESTART")
        .map(|value| {
            matches!(
                value.as_str(),
                "1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON"
            )
        })
        .unwrap_or(false)
}

fn paste_text(pane_id: &str, text: &str) -> Result<(), String> {
    let counter = BUFFER_COUNTER.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let buffer_name = format!("agent-monitor-{timestamp}-{counter}");

    run_tmux(&[
        "set-buffer".to_string(),
        "-b".to_string(),
        buffer_name.clone(),
        "--".to_string(),
        text.to_string(),
    ])?;

    let paste = run_tmux(&[
        "paste-buffer".to_string(),
        "-d".to_string(),
        "-p".to_string(),
        "-b".to_string(),
        buffer_name.clone(),
        "-t".to_string(),
        pane_id.to_string(),
    ]);

    if paste.is_err() {
        let _ = run_tmux(&["delete-buffer".to_string(), "-b".to_string(), buffer_name]);
    }

    paste.map(|_| ())
}

fn project_name_from_path(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|value| value.to_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| path.to_string())
}

fn session_agent_alias(session: &str) -> Option<&str> {
    let prefix = session.split_once('_')?.0;
    let alias = prefix
        .split_once('-')
        .map(|(alias, _)| alias)
        .unwrap_or(prefix);
    match alias {
        "cc" | "cx" => Some(alias),
        _ => None,
    }
}

fn agent_kind_for_pane(pane: &BasePane, tail: &str) -> Option<&'static str> {
    // The amux session-name prefix is authoritative — a `cc_` pane is Claude
    // even when its terminal is full of the word "codex" (e.g. a conversation
    // *about* codex), and vice versa. Content sniffing is only a fallback for
    // panes not launched by amux.
    match session_agent_alias(&pane.session) {
        Some("cx") => return Some("codex"),
        Some("cc") => return Some("claude"),
        _ => {}
    }

    if is_codex_pane(pane, tail) {
        return Some("codex");
    }

    let haystack = format!("{}\n{}\n{}", pane.session, pane.command, pane.title).to_lowercase();
    if pane.command == "claude" || haystack.contains("claude") {
        return Some("claude");
    }

    None
}

fn project_session_name(agent: &str, path: &str) -> Result<String, String> {
    let alias = if agent == "codex" { "cx" } else { "cc" };
    // Reuse the CLI's naming (crate::session) so a project launched from the web
    // UI / app lands on the SAME tmux session `amux run` would create for that
    // directory. Canonicalize first to match run.rs's cwd handling and its
    // SHA-256 hash (this used to be a separate SHA-1 path, which never matched).
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
    Ok(crate::session::session_name(alias, &abs))
}

fn agent_launch_command(agent: &str) -> Result<String, String> {
    let (env_key, default_command) = match agent {
        "claude" => ("AGENT_MONITOR_CC_COMMAND", "claude"),
        "codex" => ("AGENT_MONITOR_CX_COMMAND", "codex --yolo"),
        _ => return Err(format!("unsupported agent: {agent}")),
    };

    Ok(env::var(env_key)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| default_command.to_string()))
}

/// Same as [`agent_launch_command`], for sibling modules in `serve`.
pub(crate) fn agent_launch_command_for(agent: &str) -> Result<String, String> {
    agent_launch_command(agent)
}

/// True when the multiplexer already has a session by this exact name.
pub(crate) fn mux_has_session(name: &str) -> bool {
    // Exact match — a suffixed session must not shadow the primary name.
    crate::tmux::has_session(name)
}

/// Pin a session to follow whichever client last used it.
///
/// window-size is per-session and snapshotted from the global at creation time,
/// so a session created before the config was installed stays stuck at the
/// narrowest client that ever attached. Best-effort — an older multiplexer
/// without the option must not fail the launch.
pub(crate) fn pin_window_size(name: &str) {
    let _ = tmux_command()
        .args(["set-option", "-t", name, "window-size", "latest"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Create a detached session running `command` in `cwd`.
///
/// Goes through `tmux_command()` so the child doesn't inherit this server's own
/// `TMUX`/`TMUX_PANE` and end up nested.
pub(crate) fn mux_new_session(name: &str, cwd: &str, command: &str) -> Result<(), String> {
    let output = tmux_command()
        .args(["new-session", "-d", "-s", name, "-c", cwd, command])
        .output()
        .map_err(|error| format!("failed to launch {command}: {error}"))?;
    if output.status.success() {
        pin_window_size(name);
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        format!("multiplexer exited with {}", output.status)
    } else {
        stderr
    })
}

fn is_codex_pane(pane: &BasePane, tail: &str) -> bool {
    let haystack = format!(
        "{}\n{}\n{}\n{}",
        pane.session, pane.command, pane.title, tail
    )
    .to_lowercase();

    session_agent_alias(&pane.session) == Some("cx")
        || pane.command == "codex"
        || haystack.contains("codex")
        || haystack.contains("gpt-")
}

fn hook_status_for_pane(pane: &BasePane) -> Option<(PaneStatus, String)> {
    let event = crate::state::current_status(&pane.id, &pane.session)?;
    let status = match event.state {
        crate::state::HookState::Running => PaneStatus::Running,
        crate::state::HookState::Waiting => PaneStatus::Waiting,
        crate::state::HookState::Idle => PaneStatus::Idle,
        crate::state::HookState::Failed => PaneStatus::Failed,
        crate::state::HookState::Done => PaneStatus::Done,
    };
    let reason = event.message.unwrap_or_else(|| {
        format!(
            "explicit status from {} at {}",
            event.source, event.created_at
        )
    });
    Some((status, reason))
}

/// Seconds since the pane's agent session file (codex rollout / claude jsonl)
/// was last written. `None` when no matching file is found. A small value means
/// the agent is actively appending output → working.
/// Cache of `(agent, cwd)` → resolved session-file path, with the time it was
/// resolved. Finding that path is expensive — for codex it stats every rollout
/// in ~/.codex/sessions (hundreds of files) and reads the head of up to 60 to
/// match the cwd. That ran per pane per 2.5s poll, and every codex pane rescans
/// the *same* directory, so it dominated the daemon's CPU. The mapping is
/// stable (a directory's active session file doesn't change second to second),
/// so cache it briefly; the mtime itself is still stat'd fresh every call, which
/// is what liveness actually depends on.
static SESSION_FILE_CACHE: LazyLock<Mutex<HashMap<String, (Option<PathBuf>, Instant)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

const SESSION_FILE_CACHE_TTL: Duration = Duration::from_secs(30);
/// A miss is re-checked much sooner: a freshly launched agent creates its
/// session file lazily, and we don't want a cached `None` to hide it for 30s.
const SESSION_FILE_CACHE_MISS_TTL: Duration = Duration::from_secs(3);

fn cached_session_file(agent: &str, cwd: &str) -> Option<PathBuf> {
    let key = format!("{agent}\u{1}{cwd}");
    if let Some((path, at)) = SESSION_FILE_CACHE
        .lock()
        .expect("session file cache mutex poisoned")
        .get(&key)
    {
        let ttl = if path.is_some() {
            SESSION_FILE_CACHE_TTL
        } else {
            SESSION_FILE_CACHE_MISS_TTL
        };
        if at.elapsed() < ttl {
            return path.clone();
        }
    }
    let resolved = crate::commands::session_ids::session_file_for(agent, Path::new(cwd));
    SESSION_FILE_CACHE
        .lock()
        .expect("session file cache mutex poisoned")
        .insert(key, (resolved.clone(), Instant::now()));
    resolved
}

fn session_activity_age(agent: &str, cwd: &str) -> Option<f64> {
    let path = cached_session_file(agent, cwd)?;
    let modified = path.metadata().ok()?.modified().ok()?;
    let secs = modified.duration_since(UNIX_EPOCH).ok()?.as_secs_f64();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs_f64();
    Some((now - secs).max(0.0))
}

/// True when the pane's bottom shows a live "actively working" indicator — the
/// agent's spinner with an interrupt hint (codex `• Working (… esc to interrupt)`,
/// claude `thinking (… esc to interrupt)`). This is authoritative for Running:
/// it overrides a stale completion hook (codex-notify / claude-stop), which is
/// necessarily out of date once the agent has begun a new turn.
/// Lowercased last `n` lines of a pane tail. Status signals all live at the
/// bottom of the screen, so callers work on this slice instead of lowercasing
/// the whole ~19KB scrollback on every poll (14 panes x 2.5s adds up fast).
fn recent_lower(tail: &str, n: usize) -> String {
    let mut lines: Vec<&str> = tail.lines().rev().take(n).collect();
    lines.reverse();
    lines.join("\n").to_lowercase()
}

fn agent_actively_working(tail: &str) -> bool {
    let recent = recent_lower(tail, 18);
    // Interruptible spinner: codex "• Working (… esc to interrupt)", Claude
    // "✻ thinking (… esc to interrupt)".
    if contains_any(&recent, &["esc to interrupt", "/stop to close"])
        && contains_any(&recent, &["working (", "thinking (", "running ("])
    {
        return true;
    }
    // Claude Code streaming spinner, e.g. "✽ Baking… (3m 13s · ↓ 10.9k tokens)".
    // Both markers must sit on the SAME line: the ellipsis-paren "… (" plus a
    // parenthesised "tokens)". A finished subagent result line looks like
    // "-purpose  Research …            43m 27s · ↓ 2.2k tokens" — it carries the
    // "· ↓" arrow but has no "… (" and no closing paren, so it must not count as
    // live work. After a turn ends the spinner becomes "✻ Churned for 1m 23s",
    // which matches neither marker.
    recent
        .lines()
        .any(|l| l.contains("… (") && l.contains("tokens)"))
}

/// Claude Code at its end-of-turn idle input prompt — the agent has finished and
/// is awaiting the next message, distinct from a mid-task permission / y-n prompt.
///
/// The signal is Claude's editable composer at the bottom of the pane: the `❯`
/// prompt plus the `-- INSERT --` / `bypass permissions` mode line. Claude only
/// renders that composer when it wants your next message — not while streaming
/// and not while a y-n choice is on screen. (An earlier version keyed off the
/// `/clear to save … tokens` hint, but Claude only prints that once context use
/// is high, so quiet panes were missed.)
fn claude_idle_ready(tail: &str) -> bool {
    // The composer and any y-n choice both render at the bottom of the screen.
    let low = recent_lower(tail, 24);
    let composer = low.contains("-- insert --") || low.contains("bypass permissions on");
    let prompt = low.contains('❯') || low.contains('>');
    let yn = contains_any(
        &low,
        &[
            "yes/no", "(y/n)", " y/n", "proceed?", "do you want", "allow once",
            "allow always", "yes, continue", "no, skip", "1. yes", "2. no",
        ],
    );
    composer && prompt && !yn && !agent_actively_working(tail)
}

fn infer_status(
    pane: &BasePane,
    tail: &str,
    changed_recently: bool,
    file_age: Option<f64>,
) -> (PaneStatus, String) {
    let agent_like = agent_kind_for_pane(pane, tail).is_some();

    // `prompt_zone` = the last few non-empty lines, i.e. the bottom of the
    // screen where a real prompt / result / error actually sits. Keyword checks
    // run against THIS, not the whole scrollback, so a conversation that merely
    // *mentions* "proceed?" / "error:" / "done" doesn't flip the status.
    let prompt_zone = tail
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase();

    // A crash or an on-screen prompt at the bottom of the pane needs the user's
    // attention and takes priority even over active work — check these first,
    // but only against `prompt_zone` so scrollback that merely mentions the
    // words doesn't trip them.
    if contains_any(
        &prompt_zone,
        &[
            "failed",
            "error:",
            "panic:",
            "exception",
            "traceback",
            "exited 1",
            "exited 2",
            "exited 101",
            "exited 127",
            "exited 128",
            "exit 1",
            "exit 2",
            "exit 101",
            "exit 127",
            "exit 128",
        ],
    ) {
        return (
            PaneStatus::Failed,
            "recent output looks like a failure".to_string(),
        );
    }

    if contains_any(
        &prompt_zone,
        &[
            "do you want",
            "proceed?",
            "continue?",
            "confirm",
            "yes/no",
            "y/n",
            "(y/n)",
            "allow?",
            "approve",
        ],
    ) {
        return (PaneStatus::Waiting, "looks like it needs input".to_string());
    }

    // Liveness: an agent actively writing its session file (or showing the live
    // "esc to interrupt" spinner) is Running. When no session file is found,
    // fall back to the terminal change signal.
    let file_fresh = file_age.map(|a| a < RUNNING_WINDOW_SECS).unwrap_or(false);
    let live_agent_work = agent_actively_working(tail);
    let file_fallback = file_age.is_none() && changed_recently;

    if agent_like && (file_fresh || live_agent_work || file_fallback) {
        let why = if file_fresh {
            "session file is actively being written"
        } else if live_agent_work {
            "agent reports active work"
        } else {
            "recent output changed"
        };
        return (PaneStatus::Running, why.to_string());
    }

    if contains_any(
        &prompt_zone,
        &[
            "success",
            "completed",
            "done",
            "finished",
            "tests passed",
            "all checks passed",
        ],
    ) {
        return (PaneStatus::Done, "recent output looks complete".to_string());
    }

    // Agent pane that isn't actively writing and shows no prompt → idle,
    // waiting for the user's next message.
    if agent_like {
        return (
            PaneStatus::Idle,
            "agent session is quiet — waiting for you".to_string(),
        );
    }

    // Non-agent processes: fall back to terminal output changes.
    if changed_recently {
        return (PaneStatus::Running, "recent output changed".to_string());
    }

    if tail.trim().is_empty() || ["zsh", "bash", "fish", "nu"].contains(&pane.command.as_str()) {
        return (PaneStatus::Idle, "shell pane".to_string());
    }

    (
        PaneStatus::Running,
        format!(
            "{} is active",
            if pane.command.is_empty() {
                "process"
            } else {
                &pane.command
            }
        ),
    )
}

fn contains_any(value: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| value.contains(needle))
}

fn strip_terminal_noise(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            continue;
        }

        output.push(ch);
    }

    output
}

fn tail_hash(value: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in value.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn activity_fingerprint(tail: &str) -> String {
    // Only the last handful of lines end up in the fingerprint, so walk the tail
    // bottom-up and stop once we have enough. Cleaning all ~300 captured lines
    // first and *then* taking 24 threw away ~90% of the work every poll, per
    // pane — the dominant cost in the 2.5s snapshot tick.
    let mut kept: Vec<String> = Vec::with_capacity(24);
    for raw in tail.lines().rev() {
        let line = strip_terminal_noise(raw)
            .replace(['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'], "")
            .trim()
            .to_string();
        if line.is_empty()
            || line.chars().all(|ch| "╭╮╰╯│─ ".contains(ch))
            || line.starts_with('›')
        {
            continue;
        }
        let lower = line.to_lowercase();
        if lower.starts_with("─ worked for")
            || (lower.contains("context ") && lower.contains("% used"))
            || (contains_any(&lower, &["working (", "thinking (", "running ("])
                && contains_any(&lower, &["esc to interrupt", "/stop to close"]))
        {
            continue;
        }
        kept.push(line);
        if kept.len() == 24 {
            break;
        }
    }
    kept.reverse();
    kept.join("\n")
}

fn clean_task_title(value: &str) -> String {
    strip_terminal_noise(value)
        .trim_start_matches(|ch: char| {
            ch.is_whitespace() || ch == '✳' || ('\u{2800}'..='\u{28ff}').contains(&ch)
        })
        .trim()
        .to_string()
}

fn meaningful_tail_lines(tail: &str, count: usize) -> Vec<String> {
    tail.lines()
        .map(strip_terminal_noise)
        .map(|line| {
            line.replace(['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'], "")
                .trim()
                .to_string()
        })
        .filter(|line| !line.is_empty())
        .filter(|line| !line.chars().all(|ch| "╭╮╰╯│─━═— ".contains(ch)))
        .filter(|line| !line.starts_with("--"))
        .filter(|line| !line.starts_with('›'))
        .rev()
        .take(count)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn interaction_source(lines: &[String]) -> Option<InteractionSource> {
    if lines.is_empty() {
        return None;
    }

    Some(InteractionSource {
        source_type: "log".to_string(),
        excerpt: lines
            .iter()
            .rev()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n"),
    })
}

fn summarize_recent_work(tail: &str) -> String {
    let mut lines = meaningful_tail_lines(tail, 32)
        .into_iter()
        .fold(Vec::<String>::new(), |mut acc, line| {
            if acc.last() != Some(&line) {
                acc.push(line);
            }
            acc
        })
        .into_iter()
        .filter(|line| {
            let lower = line.to_lowercase();
            contains_any(
                &lower,
                &[
                    "succeeded",
                    "passed",
                    "finished",
                    "completed",
                    "done",
                    "fixed",
                    "updated",
                    "created",
                    "generated",
                    "built",
                    "compiled",
                    "checked",
                    "installed",
                    "launched",
                    "failed",
                    "error",
                ],
            )
        })
        .collect::<Vec<_>>();

    if lines.len() > 4 {
        lines = lines.split_off(lines.len() - 4);
    }

    if lines.is_empty() {
        let recent = meaningful_tail_lines(tail, 4).into_iter().fold(
            Vec::<String>::new(),
            |mut acc, line| {
                if acc.last() != Some(&line) {
                    acc.push(line);
                }
                acc
            },
        );
        if recent.is_empty() {
            return "No recent work has been captured yet.".to_string();
        }
        return recent
            .into_iter()
            .map(|line| format!("- {}", limit_string(&line, 150)))
            .collect::<Vec<_>>()
            .join("\n");
    }

    let lines = lines
        .into_iter()
        .map(|line| {
            if line.chars().count() > 140 {
                format!("{}...", limit_string(&line, 137))
            } else {
                line
            }
        })
        .collect::<Vec<_>>();

    lines
        .into_iter()
        .map(|line| format!("- {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn phase_feedback_message(
    pane: &BasePane,
    lines: &[String],
    status: &PaneStatus,
    reason: &str,
    now: &str,
    fingerprint: u64,
    source: Option<InteractionSource>,
) -> InteractionMessage {
    let last_line = lines.last().cloned();
    let mut message = InteractionMessage {
        id: format!(
            "{}:feedback:{}:{fingerprint}",
            pane.id,
            pane_status_key(status)
        ),
        pane_id: pane.id.clone(),
        role: InteractionRole::Agent,
        kind: InteractionKind::Notification,
        priority: InteractionPriority::Normal,
        title: "Phase feedback".to_string(),
        body: last_line
            .as_ref()
            .map(|line| format!("Latest checkpoint: {line}"))
            .unwrap_or_else(|| {
                "The agent is still working. Feedback will update when the next checkpoint appears."
                    .to_string()
            }),
        actions: None,
        source,
        created_at: now.to_string(),
    };

    match status {
        PaneStatus::Running => {}
        PaneStatus::Waiting => {
            message.priority = InteractionPriority::High;
            message.title = "Blocked".to_string();
            message.body =
                "The agent is waiting for your reply before it can continue.".to_string();
        }
        PaneStatus::Failed => {
            message.priority = InteractionPriority::High;
            message.title = "Needs follow-up".to_string();
            message.body = if reason.is_empty() {
                last_line.unwrap_or_else(|| {
                    "The last phase needs attention before work can continue.".to_string()
                })
            } else {
                reason.to_string()
            };
        }
        PaneStatus::Done => {
            message.title = "Ready for next instruction".to_string();
            message.body =
                "Recent work appears complete. You can send a follow-up instruction below."
                    .to_string();
        }
        PaneStatus::Idle => {
            message.priority = InteractionPriority::Low;
            message.title = "Ready".to_string();
            message.body =
                "No active work is running. Send a new instruction below when you want the agent to continue."
                    .to_string();
        }
    }

    message
}

fn local_interaction_messages(
    pane: &BasePane,
    tail: &str,
    status: &PaneStatus,
    reason: &str,
    now: &str,
) -> Vec<InteractionMessage> {
    let lines = meaningful_tail_lines(tail, 10);
    let source = interaction_source(&lines);
    let fingerprint = tail_hash(&activity_fingerprint(tail));
    let title = clean_task_title(&pane.title);
    let history_message = InteractionMessage {
        id: format!("{}:summary:{fingerprint}", pane.id),
        pane_id: pane.id.clone(),
        role: InteractionRole::Agent,
        kind: InteractionKind::Summary,
        priority: InteractionPriority::Low,
        title: "Recent work".to_string(),
        body: summarize_recent_work(tail),
        actions: None,
        source: source.clone(),
        created_at: now.to_string(),
    };
    let mut message = InteractionMessage {
        id: format!(
            "{}:current:{}:{fingerprint}",
            pane.id,
            pane_status_key(status)
        ),
        pane_id: pane.id.clone(),
        role: InteractionRole::Agent,
        kind: InteractionKind::Status,
        priority: InteractionPriority::Low,
        title: "Idle".to_string(),
        body: if reason.is_empty() {
            "The agent is idle right now.".to_string()
        } else {
            reason.to_string()
        },
        actions: None,
        source,
        created_at: now.to_string(),
    };

    match status {
        PaneStatus::Waiting => {
            let prompt = lines
                .last()
                .cloned()
                .unwrap_or_else(|| "I need your input before I can continue.".to_string());
            let lower = prompt.to_lowercase();
            let is_permission = contains_any(
                &lower,
                &[
                    "allow",
                    "approve",
                    "permission",
                    "continue",
                    "proceed",
                    "yes/no",
                    "y/n",
                ],
            );
            message.kind = if is_permission {
                InteractionKind::PermissionRequest
            } else {
                InteractionKind::Question
            };
            message.priority = InteractionPriority::High;
            message.title = if is_permission {
                "Approval needed".to_string()
            } else {
                "Agent is asking".to_string()
            };
            message.body = prompt;
            message.actions = Some(vec![
                InteractionAction {
                    label: "Yes".to_string(),
                    payload: "yes".to_string(),
                    style: Some(InteractionActionStyle::Default),
                },
                InteractionAction {
                    label: "No".to_string(),
                    payload: "no".to_string(),
                    style: Some(InteractionActionStyle::Destructive),
                },
                InteractionAction {
                    label: "Continue".to_string(),
                    payload: "继续".to_string(),
                    style: Some(InteractionActionStyle::Default),
                },
            ]);
        }
        PaneStatus::Running => {
            message.kind = InteractionKind::Progress;
            message.priority = InteractionPriority::Normal;
            message.title = "Working".to_string();
            message.body = if title.is_empty() {
                if reason.is_empty() {
                    "Working on the current task.".to_string()
                } else {
                    reason.to_string()
                }
            } else {
                format!("Working on {title}.")
            };
        }
        PaneStatus::Failed => {
            message.kind = InteractionKind::Error;
            message.priority = InteractionPriority::High;
            message.title = "Needs attention".to_string();
            message.body = if reason.is_empty() {
                lines
                    .last()
                    .cloned()
                    .unwrap_or_else(|| "The agent appears to have hit an error.".to_string())
            } else {
                reason.to_string()
            };
            message.actions = Some(vec![InteractionAction {
                label: "Open log".to_string(),
                payload: "open_terminal".to_string(),
                style: Some(InteractionActionStyle::Default),
            }]);
        }
        PaneStatus::Done => {
            message.kind = InteractionKind::Done;
            message.priority = InteractionPriority::Normal;
            message.title = "Completed".to_string();
            message.body = if title.is_empty() {
                if reason.is_empty() {
                    "Task completed.".to_string()
                } else {
                    reason.to_string()
                }
            } else {
                format!("Finished {title}.")
            };
        }
        PaneStatus::Idle => {}
    }

    let feedback_message = phase_feedback_message(
        pane,
        &lines,
        status,
        reason,
        now,
        fingerprint,
        history_message.source.clone(),
    );

    vec![history_message, message, feedback_message]
}

fn pane_status_key(status: &PaneStatus) -> &'static str {
    match status {
        PaneStatus::Running => "running",
        PaneStatus::Waiting => "waiting",
        PaneStatus::Idle => "idle",
        PaneStatus::Failed => "failed",
        PaneStatus::Done => "done",
    }
}

fn limit_string(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn parse_interaction_role(value: Option<&str>) -> InteractionRole {
    match value {
        Some("user") => InteractionRole::User,
        Some("system") => InteractionRole::System,
        _ => InteractionRole::Agent,
    }
}

fn parse_interaction_kind(value: Option<&str>, fallback: &InteractionKind) -> InteractionKind {
    match value {
        Some("summary") => InteractionKind::Summary,
        Some("question") => InteractionKind::Question,
        Some("permission_request") => InteractionKind::PermissionRequest,
        Some("progress") => InteractionKind::Progress,
        Some("error") => InteractionKind::Error,
        Some("done") => InteractionKind::Done,
        Some("notification") => InteractionKind::Notification,
        Some("status") => InteractionKind::Status,
        _ => fallback.clone(),
    }
}

fn parse_interaction_priority(value: Option<&str>) -> InteractionPriority {
    match value {
        Some("low") => InteractionPriority::Low,
        Some("high") => InteractionPriority::High,
        _ => InteractionPriority::Normal,
    }
}

fn normalize_interaction_message(
    value: &serde_json::Value,
    pane_id: &str,
    now: &str,
    fallback: &InteractionMessage,
) -> Option<InteractionMessage> {
    let object = value.as_object()?;
    let title = object
        .get("title")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(|value| limit_string(value.trim(), 80))
        .unwrap_or_else(|| fallback.title.clone());
    let body = object
        .get("body")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(|value| limit_string(value.trim(), 800))
        .unwrap_or_else(|| fallback.body.clone());
    let kind = parse_interaction_kind(
        object.get("kind").and_then(|value| value.as_str()),
        &fallback.kind,
    );
    let actions = object
        .get("actions")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let action = item.as_object()?;
                    let label = action.get("label")?.as_str()?;
                    let payload = action.get("payload")?.as_str()?;
                    Some(InteractionAction {
                        label: limit_string(label, 32),
                        payload: limit_string(payload, 120),
                        style: match action.get("style").and_then(|value| value.as_str()) {
                            Some("destructive") => Some(InteractionActionStyle::Destructive),
                            _ => Some(InteractionActionStyle::Default),
                        },
                    })
                })
                .take(4)
                .collect::<Vec<_>>()
        })
        .filter(|items| !items.is_empty())
        .or_else(|| fallback.actions.clone());

    Some(InteractionMessage {
        id: object
            .get("id")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .unwrap_or_else(|| {
                format!(
                    "{pane_id}:{}:{}",
                    interaction_kind_key(&kind),
                    tail_hash(&format!("{title}\n{body}"))
                )
            }),
        pane_id: pane_id.to_string(),
        role: parse_interaction_role(object.get("role").and_then(|value| value.as_str())),
        kind,
        priority: parse_interaction_priority(
            object.get("priority").and_then(|value| value.as_str()),
        ),
        title,
        body,
        actions,
        source: fallback.source.clone(),
        created_at: object
            .get("createdAt")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .unwrap_or(now)
            .to_string(),
    })
}

fn interaction_kind_key(kind: &InteractionKind) -> &'static str {
    match kind {
        InteractionKind::Summary => "summary",
        InteractionKind::Status => "status",
        InteractionKind::Question => "question",
        InteractionKind::PermissionRequest => "permission_request",
        InteractionKind::Progress => "progress",
        InteractionKind::Error => "error",
        InteractionKind::Done => "done",
        InteractionKind::Notification => "notification",
    }
}

fn interaction_messages_for_pane(
    pane: &BasePane,
    tail: &str,
    status: &PaneStatus,
    reason: &str,
    now: &str,
) -> Vec<InteractionMessage> {
    let fingerprint = tail_hash(&activity_fingerprint(tail));
    let cache_key = format!("{}:{}:{fingerprint}", pane.id, pane_status_key(status));
    let fallback = local_interaction_messages(pane, tail, status, reason, now);

    if let Some(cached) = MESSAGE_CACHE
        .lock()
        .expect("message cache mutex poisoned")
        .get(&cache_key)
        .cloned()
    {
        return cached;
    }

    spawn_deepseek_interpretation(
        cache_key,
        pane.clone(),
        tail.to_string(),
        status.clone(),
        reason.to_string(),
        now.to_string(),
        fallback.clone(),
    );
    fallback
}

fn spawn_deepseek_interpretation(
    cache_key: String,
    pane: BasePane,
    tail: String,
    status: PaneStatus,
    reason: String,
    now: String,
    fallback: Vec<InteractionMessage>,
) {
    let api_key = env::var("AGENT_MONITOR_DEEPSEEK_API_KEY")
        .or_else(|_| env::var("DEEPSEEK_API_KEY"))
        .unwrap_or_default();
    if api_key.is_empty() {
        return;
    }

    {
        let mut pending = PENDING_INTERPRETATIONS
            .lock()
            .expect("pending interpretations mutex poisoned");
        if pending.contains_key(&cache_key) {
            return;
        }
        pending.insert(cache_key.clone(), ());
    }

    thread::spawn(move || {
        let result =
            interpret_with_deepseek(&api_key, &pane, &tail, &status, &reason, &now, &fallback);
        if let Some(messages) = result.filter(|messages| !messages.is_empty()) {
            MESSAGE_CACHE
                .lock()
                .expect("message cache mutex poisoned")
                .insert(cache_key.clone(), messages);
        }
        PENDING_INTERPRETATIONS
            .lock()
            .expect("pending interpretations mutex poisoned")
            .remove(&cache_key);
    });
}

fn interpret_with_deepseek(
    api_key: &str,
    pane: &BasePane,
    tail: &str,
    status: &PaneStatus,
    reason: &str,
    now: &str,
    fallback: &[InteractionMessage],
) -> Option<Vec<InteractionMessage>> {
    let base_url = env::var("AGENT_MONITOR_DEEPSEEK_BASE_URL")
        .or_else(|_| env::var("DEEPSEEK_BASE_URL"))
        .unwrap_or_else(|_| "https://api.deepseek.com".to_string());
    let model = env::var("AGENT_MONITOR_DEEPSEEK_MODEL")
        .or_else(|_| env::var("DEEPSEEK_MODEL"))
        .unwrap_or_else(|_| "deepseek-v4-flash".to_string());
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .ok()?;
    let first_fallback = fallback.first()?;
    let body = json!({
        "model": model,
        "response_format": { "type": "json_object" },
        "messages": [
            {
                "role": "system",
                "content": "You convert terminal logs from coding agents into concise product-facing interaction messages.\nReturn only JSON with a messages array. Do not include markdown.\nDo not expose secrets, tokens, raw stack traces, or long logs.\nReturn exactly 3 messages in this order: recent work summary, current state, phase feedback.\nThe first message must be kind summary with title Recent work and must summarize what the agent recently completed or attempted.\nThe second message should describe current state: Working, Waiting, Completed, Failed, or Idle.\nThe third message should be phase feedback: newest checkpoint, blocker, completion feedback, or next useful step.\nMessages must follow: role agent|system, kind summary|status|question|permission_request|progress|error|done|notification, priority low|normal|high, title, body, actions."
            },
            {
                "role": "user",
                "content": json!({
                    "pane": {
                        "id": pane.id,
                        "session": pane.session,
                        "command": pane.command,
                        "title": pane.title,
                        "status": pane_status_key(status),
                        "reason": reason,
                    },
                    "recentLog": meaningful_tail_lines(tail, 18).join("\n"),
                }).to_string()
            }
        ]
    });

    let response = client
        .post(format!(
            "{}/chat/completions",
            base_url.trim_end_matches('/')
        ))
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .ok()?;
    if !response.status().is_success() {
        return None;
    }

    let value = response.json::<serde_json::Value>().ok()?;
    let content = value
        .get("choices")?
        .as_array()?
        .first()?
        .get("message")?
        .get("content")?
        .as_str()?;
    let parsed = serde_json::from_str::<serde_json::Value>(content).ok()?;
    let messages = parsed
        .get("messages")?
        .as_array()?
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            let fallback_message = fallback
                .get(index.min(fallback.len().saturating_sub(1)))
                .unwrap_or(first_fallback);
            normalize_interaction_message(message, &pane.id, now, fallback_message)
        })
        .take(3)
        .collect::<Vec<_>>();
    Some(messages)
}

fn deepseek_api_key() -> String {
    env::var("AGENT_MONITOR_DEEPSEEK_API_KEY")
        .or_else(|_| env::var("DEEPSEEK_API_KEY"))
        .unwrap_or_default()
}

fn deepseek_base_url() -> String {
    env::var("AGENT_MONITOR_DEEPSEEK_BASE_URL")
        .or_else(|_| env::var("DEEPSEEK_BASE_URL"))
        .unwrap_or_else(|_| "https://api.deepseek.com".to_string())
}

fn deepseek_model() -> String {
    env::var("AGENT_MONITOR_DEEPSEEK_MODEL")
        .or_else(|_| env::var("DEEPSEEK_MODEL"))
        .unwrap_or_else(|_| "deepseek-v4-flash".to_string())
}

fn normalize_refined_text(original: &str, value: &serde_json::Value) -> String {
    let Some(text) = value
        .get("text")
        .and_then(|item| item.as_str())
        .map(str::trim)
    else {
        return original.to_string();
    };
    if text.is_empty() || text.len() > 4000 {
        original.to_string()
    } else {
        text.to_string()
    }
}

fn refine_text_with_deepseek(text: &str) -> serde_json::Value {
    let original = text.trim();
    if original.is_empty() {
        return json!({ "ok": true, "text": original, "changed": false });
    }

    let api_key = deepseek_api_key();
    if api_key.is_empty() {
        return json!({
            "ok": true,
            "text": original,
            "changed": false,
            "fallback": true,
            "error": "DeepSeek API key is not configured"
        });
    }

    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return json!({
                "ok": true,
                "text": original,
                "changed": false,
                "fallback": true,
                "error": error.to_string()
            });
        }
    };

    let body = json!({
        "model": deepseek_model(),
        "response_format": { "type": "json_object" },
        "messages": [
            {
                "role": "system",
                "content": "You clean up speech-to-text drafts before they are sent to a coding agent.\nReturn only JSON: {\"text\":\"...\"}.\nPreserve the user's intent, language, tone, and commands.\nAdd punctuation and paragraph breaks when useful.\nFix likely technical terms such as Claude Code, Codex, tmux, SwiftUI, Xcode, TestFlight, DeepSeek, API, WebSocket, React, Rust, iOS, macOS, zsh, cargo, xcodebuild.\nDo not add new instructions, explanations, markdown, quotes, greetings, or summaries.\nIf the draft already looks correct, return it unchanged."
            },
            {
                "role": "user",
                "content": json!({ "text": original }).to_string()
            }
        ]
    });

    let response = match client
        .post(format!(
            "{}/chat/completions",
            deepseek_base_url().trim_end_matches('/')
        ))
        .bearer_auth(api_key)
        .json(&body)
        .send()
    {
        Ok(response) => response,
        Err(error) => {
            return json!({
                "ok": true,
                "text": original,
                "changed": false,
                "fallback": true,
                "error": error.to_string()
            });
        }
    };

    if !response.status().is_success() {
        return json!({
            "ok": true,
            "text": original,
            "changed": false,
            "fallback": true,
            "error": format!("DeepSeek HTTP {}", response.status())
        });
    }

    let value = match response.json::<serde_json::Value>() {
        Ok(value) => value,
        Err(error) => {
            return json!({
                "ok": true,
                "text": original,
                "changed": false,
                "fallback": true,
                "error": error.to_string()
            });
        }
    };
    let content = value
        .get("choices")
        .and_then(|choices| choices.as_array())
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_str());

    let Some(content) = content else {
        return json!({
            "ok": true,
            "text": original,
            "changed": false,
            "fallback": true,
            "error": "DeepSeek returned empty content"
        });
    };

    let parsed = match serde_json::from_str::<serde_json::Value>(content) {
        Ok(value) => value,
        Err(error) => {
            return json!({
                "ok": true,
                "text": original,
                "changed": false,
                "fallback": true,
                "error": error.to_string()
            });
        }
    };
    let refined = normalize_refined_text(original, &parsed);
    json!({ "ok": true, "text": refined, "changed": refined != original })
}

fn track_pane_activity(pane_id: &str, tail: &str) -> bool {
    let hash = tail_hash(&activity_fingerprint(tail));
    let mut activity = PANE_ACTIVITY.lock().expect("pane activity mutex poisoned");
    let now = Instant::now();

    match activity.get_mut(pane_id) {
        Some(previous) if previous.tail_hash == hash => false,
        Some(previous) => {
            previous.tail_hash = hash;
            previous.changed_at = now;
            true
        }
        None => {
            activity.insert(
                pane_id.to_string(),
                PaneActivity {
                    tail_hash: hash,
                    changed_at: now,
                },
            );
            false
        }
    }
}

fn build_snapshot() -> Snapshot {
    let now = now_iso();
    let system = collect_system_stats();
    let device = collect_device_info();
    let panes = match list_panes() {
        Ok(panes) => panes,
        Err(error) => {
            return Snapshot {
                ok: false,
                now,
                panes: Vec::new(),
                system,
                device,
                error: Some(error),
            };
        }
    };

    let history_updates = panes
        .iter()
        .filter_map(|pane| {
            agent_kind_for_pane(pane, "").map(|agent| (pane.path.clone(), agent.to_string()))
        })
        .collect::<Vec<_>>();
    remember_project_history_entries(history_updates, &now);

    let panes = panes
        .into_iter()
        .map(|pane| {
            let tail = capture_pane(&pane.id);
            let changed_recently = track_pane_activity(&pane.id, &tail);
            let activity_age_secs = agent_kind_for_pane(&pane, &tail)
                .and_then(|agent| session_activity_age(agent, &pane.path));
            let inferred = infer_status(&pane, &tail, changed_recently, activity_age_secs);
            let (status, reason) = match hook_status_for_pane(&pane) {
                // A completion hook (codex-notify / claude-stop) latches Done/
                // Idle/Waiting at turn-end, but the agent may have started a new
                // turn since. If the pane is live-working right now, trust that
                // over the stale hook so a running task isn't shown as done.
                Some(_) if agent_actively_working(&tail) => inferred,
                // Stale claude-notification: Claude finished its turn and is back
                // at the idle input prompt (shows "/clear to save … tokens"),
                // but the Waiting hook was never cleared. The stranded status
                // blocks the pending-message flush (Idle/Done only). We lock
                // this override behind a strong idle signal that is absent
                // during mid-task y-n prompts so we never flush into one.
                Some((PaneStatus::Waiting, _)) if claude_idle_ready(&tail) => inferred,
                Some(hooked) => hooked,
                None => inferred,
            };
            PANE_STATUS_CACHE
                .lock()
                .expect("pane status cache mutex poisoned")
                .insert(pane.id.clone(), status.clone());
            let messages = interaction_messages_for_pane(&pane, &tail, &status, &reason, &now);

            Pane {
                id: pane.id,
                target: pane.target,
                session: pane.session,
                window_index: pane.window_index,
                window_name: pane.window_name,
                pane_index: pane.pane_index,
                command: pane.command,
                path: pane.path,
                active: pane.active,
                pid: pane.pid,
                title: pane.title,
                tail,
                status,
                reason,
                updated_at: now.clone(),
                activity_age_secs,
                messages,
            }
        })
        .collect();

    let mut panes: Vec<Pane> = panes;
    append_herdr_panes(&mut panes, &now);
    append_dsh_panes(&mut panes, &now);

    Snapshot {
        ok: true,
        now,
        panes,
        system,
        device,
        error: None,
    }
}

/// Append agents running inside herdr (`amux serve --herdr`).
///
/// herdr reports agent state natively, so these panes bypass the terminal
/// heuristics entirely — no spinner matching, no session-file scanning, no
/// hook events. A missing or broken herdr contributes nothing and never
/// disturbs the rmux panes above.
fn append_herdr_panes(panes: &mut Vec<Pane>, now: &str) {
    if !crate::serve::herdr::enabled() {
        return;
    }
    let bridged = crate::serve::herdr::collect();
    if bridged.is_empty() {
        crate::serve::herdr::note_empty_once();
        return;
    }
    for b in bridged {
        let status = herdr_status(&b.agent_status);
        let reason = format!("herdr: {}", b.agent_status);
        PANE_STATUS_CACHE
            .lock()
            .expect("pane status cache mutex poisoned")
            .insert(b.id.clone(), status.clone());

        // Reuse the same interaction-message builder the rmux panes use, so
        // the client renders herdr panes identically.
        let base = BasePane {
            id: b.id.clone(),
            target: b.id.clone(),
            session: b.session.clone(),
            window_index: String::new(),
            window_name: b.workspace_id.clone(),
            pane_index: String::new(),
            command: b.agent.clone(),
            path: b.cwd.clone(),
            active: false,
            pid: None,
            title: b.title.clone(),
        };
        let messages = interaction_messages_for_pane(&base, &b.tail, &status, &reason, now);

        panes.push(Pane {
            id: b.id,
            target: base.target,
            session: b.session,
            window_index: base.window_index,
            window_name: base.window_name,
            pane_index: base.pane_index,
            command: b.agent,
            path: b.cwd,
            active: false,
            pid: None,
            title: b.title,
            tail: b.tail,
            status,
            reason,
            updated_at: now.to_string(),
            activity_age_secs: None,
            messages,
        });
    }
}

/// Surface DeepSeek Harness sessions alongside the rmux panes.
///
/// Like herdr, dsh reports state structurally, so these panes skip the
/// terminal heuristics entirely. Enabled automatically when a `dsh web` server
/// answers on its port; a missing one contributes nothing.
fn append_dsh_panes(panes: &mut Vec<Pane>, now: &str) {
    if !crate::serve::dsh::available() {
        return;
    }
    for b in crate::serve::dsh::collect() {
        // dsh gives a boolean, not a state machine: a turn is either being
        // produced or it isn't. Waiting/Failed have no counterpart here.
        let status = if b.running {
            PaneStatus::Running
        } else {
            PaneStatus::Idle
        };
        let reason = format!("dsh: {} · {} 轮", b.preset, b.turns);
        PANE_STATUS_CACHE
            .lock()
            .expect("pane status cache mutex poisoned")
            .insert(b.id.clone(), status.clone());

        let tail = crate::serve::dsh::tail(&b.id);
        let base = BasePane {
            id: b.id.clone(),
            target: b.id.clone(),
            session: b.session.clone(),
            window_index: String::new(),
            window_name: String::new(),
            pane_index: String::new(),
            command: "dsh".to_string(),
            path: b.cwd.clone(),
            active: false,
            pid: None,
            title: b.title.clone(),
        };
        let messages = interaction_messages_for_pane(&base, &tail, &status, &reason, now);

        panes.push(Pane {
            id: b.id,
            target: base.target,
            session: b.session,
            window_index: base.window_index,
            window_name: base.window_name,
            pane_index: base.pane_index,
            command: base.command,
            path: b.cwd,
            active: false,
            pid: None,
            title: b.title,
            tail,
            status,
            reason,
            updated_at: now.to_string(),
            activity_age_secs: None,
            messages,
        });
    }
}

/// Map herdr's agent state onto amux's. `blocked` is the one amux spends the
/// most effort inferring from terminal text — herdr reports it directly.
fn herdr_status(state: &str) -> PaneStatus {
    match state {
        "working" => PaneStatus::Running,
        "blocked" => PaneStatus::Waiting,
        "done" => PaneStatus::Done,
        // `idle`, `unknown`, and anything new default to Idle: safe, because
        // Idle is what lets the pending queue flush.
        _ => PaneStatus::Idle,
    }
}

fn collect_system_stats() -> Option<SystemStats> {
    // Cross-platform CPU + memory via sysinfo (works on macOS, Linux, Windows).
    // A persistent System instance lets CPU% measure the delta since the last
    // snapshot (~2.5s), so readings are meaningful after the first tick.
    let mut sys = SYSINFO.lock().ok()?;
    sys.refresh_cpu_usage();
    sys.refresh_memory();

    let cpu = {
        let usage = sys.global_cpu_usage();
        if usage.is_finite() {
            Some((usage as f64).clamp(0.0, 100.0))
        } else {
            None
        }
    };
    let total = sys.total_memory() as f64;
    let memory = if total > 0.0 {
        Some((sys.used_memory() as f64 / total * 100.0).clamp(0.0, 100.0))
    } else {
        None
    };

    if cpu.is_none() && memory.is_none() {
        return None;
    }
    Some(SystemStats {
        cpu_usage: cpu,
        memory_usage: memory,
    })
}

fn collect_device_info() -> Option<DeviceInfo> {
    let info: &Option<DeviceInfo> = &DEVICE_INFO_CACHE;
    info.clone()
}

#[cfg(target_os = "macos")]
fn collect_device_info_uncached() -> Option<DeviceInfo> {
    let name = command_stdout("scutil", &["--get", "ComputerName"])
        .or_else(|| command_stdout("hostname", &[]))
        .and_then(clean_command_output);
    let model_identifier =
        command_stdout("sysctl", &["-n", "hw.model"]).and_then(clean_command_output);
    let reported_model_name = command_stdout("system_profiler", &["SPHardwareDataType"])
        .and_then(system_profiler_model_name);

    if name.is_none() && model_identifier.is_none() && reported_model_name.is_none() {
        return None;
    }

    let (kind, fallback_model_name) = device_kind_for_hints(&[
        reported_model_name.as_deref(),
        model_identifier.as_deref(),
        name.as_deref(),
    ]);
    Some(DeviceInfo {
        name,
        model_identifier,
        kind: kind.to_string(),
        model_name: reported_model_name.unwrap_or_else(|| fallback_model_name.to_string()),
    })
}

/// Non-macOS (Linux, Windows): hostname + OS name via sysinfo.
#[cfg(not(target_os = "macos"))]
fn collect_device_info_uncached() -> Option<DeviceInfo> {
    let name = sysinfo::System::host_name();
    let os_name = sysinfo::System::long_os_version().or_else(sysinfo::System::name);
    if name.is_none() && os_name.is_none() {
        return None;
    }
    let kind = if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    };
    Some(DeviceInfo {
        name,
        model_identifier: None,
        kind: kind.to_string(),
        model_name: os_name.unwrap_or_else(|| kind.to_string()),
    })
}

fn device_kind_for_hints(hints: &[Option<&str>]) -> (&'static str, &'static str) {
    let model = hints
        .iter()
        .filter_map(|hint| *hint)
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    if model.contains("mac_mini") || model.contains("macmini") || model.contains("mac mini") {
        return ("mac_mini", "Mac mini");
    }
    if model.contains("macbook") {
        return ("macbook", "MacBook");
    }
    if model.contains("macstudio") {
        return ("mac_studio", "Mac Studio");
    }
    if model.contains("imac") {
        return ("imac", "iMac");
    }
    if model.contains("macpro") {
        return ("mac_pro", "Mac Pro");
    }
    ("mac", "Mac")
}

fn system_profiler_model_name(output: String) -> Option<String> {
    output.lines().find_map(|line| {
        line.trim()
            .strip_prefix("Model Name:")
            .map(ToString::to_string)
            .and_then(clean_command_output)
    })
}

fn command_stdout(command: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(command).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn clean_command_output(output: String) -> Option<String> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

fn broadcast_snapshot(state: &AppState) -> Snapshot {
    let snapshot = build_snapshot();
    flush_pending_messages(state, &snapshot.panes);
    // Full build: fire status-change push notifications for phone-initiated turns.
    #[cfg(feature = "full")]
    crate::serve::full::push::notify_status_changes(&snapshot.panes);
    let _ = state.snapshots.send(json!({
        "type": "snapshot",
        "snapshot": snapshot,
    }));
    snapshot
}

fn request_pane_log_refresh(state: &AppState, pane_id: &str) {
    let _ = state.pane_log_refreshes.send(pane_id.to_string());
}

fn request_pane_log_refresh_burst(state: &AppState, pane_id: &str) {
    let burst_id = PANE_LOG_REFRESH_BURST_COUNTER.fetch_add(1, Ordering::Relaxed);
    {
        let mut bursts = PANE_LOG_REFRESH_BURST_IDS.lock().unwrap();
        bursts.insert(pane_id.to_string(), burst_id);
    }

    for delay_ms in PANE_LOG_REFRESH_BURST_DELAYS_MS {
        if *delay_ms == 0 {
            request_pane_log_refresh(state, pane_id);
            continue;
        }

        let state = state.clone();
        let pane_id = pane_id.to_string();
        let delay_ms = *delay_ms;
        let is_last_refresh = delay_ms == *PANE_LOG_REFRESH_BURST_DELAYS_MS.last().unwrap_or(&0);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            let is_current = PANE_LOG_REFRESH_BURST_IDS
                .lock()
                .unwrap()
                .get(&pane_id)
                .copied()
                == Some(burst_id);
            if is_current {
                request_pane_log_refresh(&state, &pane_id);
                if is_last_refresh {
                    PANE_LOG_REFRESH_BURST_IDS.lock().unwrap().remove(&pane_id);
                }
            }
        });
    }
}

async fn pane_command_response_after_command(
    state: &AppState,
    pane_id: &str,
    previous_tail: String,
) -> serde_json::Value {
    let mut tail = String::new();
    for delay_ms in PANE_COMMAND_TAIL_SETTLE_DELAYS_MS {
        if *delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(*delay_ms)).await;
        }

        tail = capture_pane_lines(pane_id, PANE_COMMAND_TAIL_LINE_COUNT);
        if tail != previous_tail {
            break;
        }
    }

    request_pane_log_refresh(state, pane_id);

    json!({
        "ok": true,
        "paneId": pane_id,
        "tail": tail,
        "capturedAt": now_iso(),
    })
}

fn schedule_snapshot_refresh_soon(state: &AppState) {
    let refresh_id = SNAPSHOT_REFRESH_COUNTER.fetch_add(1, Ordering::Relaxed);
    let state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(60)).await;
        if SNAPSHOT_REFRESH_COUNTER.load(Ordering::Relaxed) == refresh_id + 1 {
            let _ = broadcast_snapshot(&state);
        }
    });
}

fn spawn_snapshot_loop(state: AppState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(2500));
        loop {
            interval.tick().await;
            let _ = broadcast_snapshot(&state);
        }
    });
}

pub(crate) fn is_authed(state: &AppState, headers: &HeaderMap, query: &HashMap<String, String>) -> bool {
    if state.token.is_empty() {
        return true;
    }

    let bearer = format!("Bearer {}", state.token);
    query.get("token") == Some(&state.token)
        || headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            == Some(bearer.as_str())
}

pub(crate) fn json_response<T: Serialize>(status: StatusCode, value: T) -> Response<Body> {
    let body = serde_json::to_vec(&value).unwrap_or_else(|_| b"{\"error\":\"json\"}".to_vec());
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .body(Body::from(body))
        .expect("response builder")
}

async fn api_snapshot(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }

    json_response(StatusCode::OK, broadcast_snapshot(&state))
}

// -------------------------------------------------------- capabilities

/// Cached result of the capability probe. `herdr` detection spawns a process,
/// so this is computed once rather than on every client poll.
static CAPABILITIES: LazyLock<serde_json::Value> = LazyLock::new(|| {
    json!({
        // Optional integrations, each gated on the tool actually being present.
        "cronbox": crate::serve::cron::available(),
        "ccSwitch": crate::provider::is_installed(),
        "herdr": {
            "installed": crate::serve::herdr::installed(),
            // Installed but not enabled means serve needs restarting with
            // --herdr; the client can say so instead of silently showing
            // nothing.
            "enabled": crate::serve::herdr::enabled(),
        },
        // dsh needs no flag: it is included whenever its web server answers.
        // `relayPort` is where its UI is reachable from outside this machine —
        // the client builds a URL from it and its own host.
        "dsh": {
            "available": crate::serve::dsh::available(),
            "relayPort": crate::serve::dsh::relay_port(),
            // Browsers only allow a WebSocket from a plain-HTTP page on
            // loopback, so the client has to know which scheme to use.
            "relayTls": crate::serve::dsh::tls_enabled(),
        },
        // Compiled-in feature set: the control-center / screenshot / push
        // endpoints only exist in `--features full` builds.
        "full": cfg!(feature = "full"),
        "platform": std::env::consts::OS,
    })
});

/// Report which optional tools this machine has, so the client can hide
/// features instead of discovering they are missing by calling them and
/// handling the failure.
async fn api_capabilities(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    let mut body = CAPABILITIES.clone();
    // `enabled` is the one field that can differ from the cached probe if the
    // flag were toggled, so refresh it rather than serving a stale value.
    if let Some(h) = body.get_mut("herdr").and_then(|h| h.as_object_mut()) {
        h.insert(
            "enabled".to_string(),
            json!(crate::serve::herdr::enabled()),
        );
    }
    // The relay binds after the cached probe, so this would otherwise be null.
    if let Some(d) = body.get_mut("dsh").and_then(|d| d.as_object_mut()) {
        d.insert("relayPort".to_string(), json!(crate::serve::dsh::relay_port()));
        d.insert("relayTls".to_string(), json!(crate::serve::dsh::tls_enabled()));
    }
    json_response(StatusCode::OK, json!({ "ok": true, "capabilities": body }))
}

// ------------------------------------------------------------- USB devices

async fn api_usb_devices(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    let devices = crate::serve::usb::enumerate();
    let available = crate::serve::usb::available();
    json_response(
        StatusCode::OK,
        json!({ "ok": true, "available": available, "devices": devices }),
    )
}

async fn api_usb_screenshot(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    let Some(serial) = query.get("serial").filter(|s| !s.is_empty()) else {
        return json_response(StatusCode::BAD_REQUEST, json!({ "error": "serial is required" }));
    };
    match crate::serve::usb::screenshot(serial) {
        Ok((bytes, content_type)) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type)
            .header(header::CACHE_CONTROL, "no-store")
            .body(Body::from(bytes))
            .expect("response builder"),
        Err(error) => {
            let code = if error.contains("no USB device") {
                StatusCode::NOT_FOUND
            } else if error.contains("not supported") {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            json_response(code, json!({ "error": error }))
        }
    }
}

// ------------------------------------------------------------- files

/// The directories the client may browse: every live pane's cwd plus the
/// project history. Computed per request so a newly opened project shows up
/// without restarting the server.
fn browsable_roots() -> Vec<crate::serve::files::Root> {
    let pane_paths: Vec<String> = list_panes()
        .map(|panes| panes.into_iter().map(|p| p.path).collect())
        .unwrap_or_default();
    let history_paths: Vec<String> = load_project_history()
        .map(|entries| entries.into_iter().map(|e| e.path).collect())
        .unwrap_or_default();
    crate::serve::files::roots(&pane_paths, &history_paths)
}

/// Map a resolve failure to a status: outside-the-roots is a refusal (403),
/// a missing path is a 404.
fn files_error(error: String) -> Response<Body> {
    let code = if error.contains("outside") {
        StatusCode::FORBIDDEN
    } else if error.contains("no such path") {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::BAD_REQUEST
    };
    json_response(code, json!({ "ok": false, "error": error }))
}

/// `GET /api/sessions?path=<dir>&limit=<n>` — past conversations for a project,
/// per agent, newest first.
async fn api_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    let Some(path) = query.get("path").filter(|v| !v.is_empty()).cloned() else {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": "path is required" }),
        );
    };
    let limit = query.get("limit").and_then(|v| v.parse::<usize>().ok());

    // Reads and parses transcript files (Claude's can reach tens of MB), so
    // this must not run on the async runtime's thread.
    match tokio::task::spawn_blocking(move || crate::serve::sessions::list(&path, limit)).await {
        Ok(Ok(agents)) => json_response(StatusCode::OK, json!({ "ok": true, "agents": agents })),
        Ok(Err(error)) => json_response(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "agents": [], "error": error }),
        ),
        Err(error) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "ok": false, "agents": [], "error": error.to_string() }),
        ),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResumeSessionRequest {
    path: String,
    agent: String,
    session_id: String,
    suffix: String,
}

/// `POST /api/sessions/resume` — reopen a past conversation in its own
/// multiplexer session, leaving the project's primary session running.
async fn api_sessions_resume(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Json(body): Json<ResumeSessionRequest>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }

    let result = tokio::task::spawn_blocking(move || {
        crate::serve::sessions::resume(&body.path, &body.agent, &body.session_id, &body.suffix)
    })
    .await;

    match result {
        Ok(Ok(session)) => {
            // Let websocket clients pick up the new pane without waiting for
            // the next poll.
            schedule_snapshot_refresh_soon(&state);
            json_response(StatusCode::OK, json!({ "ok": true, "session": session }))
        }
        Ok(Err(error)) => json_response(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": error }),
        ),
        Err(error) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "ok": false, "error": error.to_string() }),
        ),
    }
}

async fn api_files_roots(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    json_response(StatusCode::OK, json!({ "ok": true, "roots": browsable_roots() }))
}

async fn api_files_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    let path = query.get("path").cloned().unwrap_or_default();
    let show_all = query.get("all").map(|v| v == "true").unwrap_or(false);
    let roots = browsable_roots();
    match crate::serve::files::resolve_within(&path, &roots) {
        Ok((dir, root)) => match crate::serve::files::list(&dir, &root, show_all) {
            Ok(listing) => json_response(
                StatusCode::OK,
                json!({ "ok": true, "listing": listing }),
            ),
            Err(error) => files_error(error),
        },
        Err(error) => files_error(error),
    }
}

async fn api_files_read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    let path = query.get("path").cloned().unwrap_or_default();
    let roots = browsable_roots();
    let (file, _) = match crate::serve::files::resolve_within(&path, &roots) {
        Ok(v) => v,
        Err(error) => return files_error(error),
    };
    match crate::serve::files::preview(&file) {
        Ok(crate::serve::files::Preview::Text { content, size }) => json_response(
            StatusCode::OK,
            json!({ "ok": true, "text": true, "content": content, "size": size }),
        ),
        // The client renders these straight from /api/files/download, which
        // already serves the correct content type.
        Ok(crate::serve::files::Preview::Media { size, kind }) => json_response(
            StatusCode::OK,
            json!({ "ok": true, "text": false, "media": kind, "size": size }),
        ),
        Ok(crate::serve::files::Preview::Binary { size, reason }) => json_response(
            StatusCode::OK,
            json!({ "ok": true, "text": false, "size": size, "reason": reason }),
        ),
        Err(error) => files_error(error),
    }
}

/// Stream a file back. Streaming rather than buffering because project trees
/// hold large artefacts (a release binary here is 48MB) that shouldn't be held
/// in memory to serve.
async fn api_files_download(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    let path = query.get("path").cloned().unwrap_or_default();
    let roots = browsable_roots();
    let (file, _) = match crate::serve::files::resolve_within(&path, &roots) {
        Ok(v) => v,
        Err(error) => return files_error(error),
    };
    let meta = match std::fs::metadata(&file) {
        Ok(m) if m.is_file() => m,
        Ok(_) => return files_error("path is a directory".to_string()),
        Err(e) => return files_error(format!("cannot stat file: {e}")),
    };
    let handle = match tokio::fs::File::open(&file).await {
        Ok(f) => f,
        Err(e) => return files_error(format!("cannot open file: {e}")),
    };

    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_string());
    // Quote-escape so a filename containing `"` can't break out of the header.
    let disposition = format!(
        "attachment; filename=\"{}\"",
        name.replace('\\', "").replace('"', "")
    );
    // Stream in chunks rather than reading the whole file into memory: project
    // trees hold large artefacts (a release binary here is 48MB).
    let stream = futures_util::stream::try_unfold(handle, |mut f| async move {
        use tokio::io::AsyncReadExt;
        let mut buf = vec![0u8; 64 * 1024];
        let n = f.read(&mut buf).await?;
        if n == 0 {
            Ok::<_, std::io::Error>(None)
        } else {
            buf.truncate(n);
            Ok(Some((axum::body::Bytes::from(buf), f)))
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, crate::serve::files::content_type_for(&file))
        .header(header::CONTENT_LENGTH, meta.len())
        .header(header::CONTENT_DISPOSITION, disposition)
        .body(Body::from_stream(stream))
        .expect("response builder")
}

// ------------------------------------------------------------- cronbox
/// Uniform failure shape for the cron endpoints. A missing CronBox is a 404
/// (nothing to manage) rather than a 500, so the client can hide the feature
/// instead of showing an error.
fn cron_error(error: String) -> Response<Body> {
    let missing = error.contains("not found");
    let code = if missing {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    json_response(code, json!({ "ok": false, "error": error }))
}

async fn api_cron_schedules(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    match crate::serve::cron::schedules() {
        Ok(schedules) => json_response(
            StatusCode::OK,
            json!({ "ok": true, "schedules": schedules }),
        ),
        Err(error) => cron_error(error),
    }
}

async fn api_cron_jobs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    let limit = query.get("limit").and_then(|v| v.parse::<usize>().ok());
    let status = query.get("status").map(String::as_str);
    let schedule_id = query.get("scheduleId").map(String::as_str);
    match crate::serve::cron::jobs(limit, status, schedule_id) {
        Ok(jobs) => json_response(StatusCode::OK, json!({ "ok": true, "jobs": jobs })),
        Err(error) => cron_error(error),
    }
}

async fn api_cron_running(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    match crate::serve::cron::running() {
        Ok(jobs) => json_response(StatusCode::OK, json!({ "ok": true, "jobs": jobs })),
        Err(error) => cron_error(error),
    }
}

async fn api_cron_log(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    let Some(id) = query.get("id").filter(|v| !v.is_empty()) else {
        return json_response(StatusCode::BAD_REQUEST, json!({ "error": "id is required" }));
    };
    match crate::serve::cron::job_log(id) {
        Ok((id, logs)) => json_response(
            StatusCode::OK,
            json!({ "ok": true, "id": id, "logs": logs }),
        ),
        Err(error) => cron_error(error),
    }
}

#[derive(Deserialize)]
struct CronActionRequest {
    action: String,
    id: String,
}

/// Mutations: enable / disable a schedule, cancel a job, or trigger a run.
/// All are delegated to the cronbox CLI so the running daemon picks them up.
async fn api_cron_action(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Json(body): Json<CronActionRequest>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    if body.id.is_empty() {
        return json_response(StatusCode::BAD_REQUEST, json!({ "error": "id is required" }));
    }
    let result = match body.action.as_str() {
        "enable" => crate::serve::cron::enable(&body.id),
        "disable" => crate::serve::cron::disable(&body.id),
        "cancel" => crate::serve::cron::cancel(&body.id),
        "trigger" => crate::serve::cron::trigger(&body.id),
        other => {
            return json_response(
                StatusCode::BAD_REQUEST,
                json!({ "error": format!("unknown action: {other}") }),
            )
        }
    };
    match result {
        Ok(message) => json_response(
            StatusCode::OK,
            json!({ "ok": true, "action": body.action, "message": message }),
        ),
        Err(error) => cron_error(error),
    }
}

async fn api_pane_context(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }

    let Some(pane_id) = query.get("paneId").filter(|value| !value.is_empty()) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({ "error": "paneId is required" }),
        );
    };

    let lines = context_line_count(query.get("lines"));
    let tail = capture_pane_lines(pane_id, lines);
    json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "paneId": pane_id,
            "lines": lines,
            "tail": tail,
            "capturedAt": now_iso(),
        }),
    )
}

/// Field-based Claude detection mirroring `agent_kind_for_pane`: the `cc_`/`cx_`
/// session prefix is authoritative, so a Claude pane whose terminal mentions
/// "codex" is still Claude (and its queue isn't stranded).
pub(crate) fn pane_is_claude(session: &str, command: &str, title: &str) -> bool {
    match session_agent_alias(session) {
        Some("cc") => return true,
        Some("cx") => return false,
        _ => {}
    }
    if command == "codex" {
        return false;
    }
    let hay = format!("{session}\n{command}\n{title}").to_lowercase();
    if hay.contains("codex") || hay.contains("gpt-") {
        return false;
    }
    command == "claude" || hay.contains("claude")
}

pub(crate) fn pane_is_codex(session: &str, command: &str, title: &str) -> bool {
    match session_agent_alias(session) {
        Some("cx") => return true,
        Some("cc") => return false,
        _ => {}
    }
    let hay = format!("{session}\n{command}\n{title}").to_lowercase();
    command == "codex" || hay.contains("codex") || hay.contains("gpt-")
}

/// The status `build_snapshot` last computed for a pane.
fn cached_pane_status(pane_id: &str) -> Option<PaneStatus> {
    PANE_STATUS_CACHE
        .lock()
        .expect("pane status cache mutex poisoned")
        .get(pane_id)
        .cloned()
}

/// Append a message to a pane's pending queue; returns the new queue length.
fn enqueue_pending(pane_id: &str, text: &str, enter: bool, vim_mode: bool) -> usize {
    let id = format!("pm{}", PENDING_ID_COUNTER.fetch_add(1, Ordering::Relaxed));
    let message = PendingMessage {
        id,
        text: text.to_string(),
        created_at: now_iso(),
        enter,
        vim_mode,
    };
    let mut queues = PENDING_MESSAGES.lock().expect("pending messages mutex poisoned");
    let queue = queues.entry(pane_id.to_string()).or_default();
    queue.push(message);
    queue.len()
}

/// Deliver one queued message per idle Claude pane (FIFO), honoring a short
/// post-flush cooldown so multi-queued messages become sequential turns.
fn flush_pending_messages(state: &AppState, panes: &[Pane]) {
    for pane in panes {
        if pane.id.is_empty() {
            continue;
        }
        // Only when the agent has finished its turn — never while Running or
        // Waiting on a y/n prompt.
        if !matches!(pane.status, PaneStatus::Idle | PaneStatus::Done) {
            continue;
        }
        // Identify by session/command/title only (never the tail), so a Claude
        // pane discussing Codex isn't misclassified and stranded.
        if !pane_is_claude(&pane.session, &pane.command, &pane.title) {
            continue;
        }
        if let Some(at) = PENDING_FLUSH_AT
            .lock()
            .expect("pending flush mutex poisoned")
            .get(&pane.id)
        {
            if at.elapsed() < Duration::from_secs(4) {
                continue;
            }
        }
        let message = {
            let mut queues = PENDING_MESSAGES.lock().expect("pending messages mutex poisoned");
            match queues.get_mut(&pane.id) {
                Some(list) if !list.is_empty() => Some(list.remove(0)),
                _ => None,
            }
        };
        let Some(message) = message else {
            continue;
        };

        // herdr panes go through its API; the rmux copy-mode / paste dance
        // below doesn't apply to them.
        if crate::serve::herdr::owns(&pane.id) {
            if let Err(error) = crate::serve::herdr::send(&pane.id, &message.text, message.enter) {
                eprintln!("[pending] herdr send failed for {}: {error}; re-queued", pane.id);
                PENDING_MESSAGES
                    .lock()
                    .expect("pending messages mutex poisoned")
                    .entry(pane.id.clone())
                    .or_default()
                    .insert(0, message);
                continue;
            }
            PENDING_FLUSH_AT
                .lock()
                .expect("pending flush mutex poisoned")
                .insert(pane.id.clone(), Instant::now());
            request_pane_log_refresh_burst(state, &pane.id);
            eprintln!("[pending] delivered queued message to {}", pane.id);
            continue;
        }

        // dsh sessions likewise: its API takes the whole prompt at once, so
        // there is no separate submit keystroke to replay.
        if crate::serve::dsh::owns(&pane.id) {
            if let Err(error) = crate::serve::dsh::send(&pane.id, &message.text) {
                eprintln!("[pending] dsh send failed for {}: {error}; re-queued", pane.id);
                PENDING_MESSAGES
                    .lock()
                    .expect("pending messages mutex poisoned")
                    .entry(pane.id.clone())
                    .or_default()
                    .insert(0, message);
                continue;
            }
            PENDING_FLUSH_AT
                .lock()
                .expect("pending flush mutex poisoned")
                .insert(pane.id.clone(), Instant::now());
            request_pane_log_refresh_burst(state, &pane.id);
            eprintln!("[pending] delivered queued message to {}", pane.id);
            continue;
        }

        exit_tmux_copy_mode(&pane.id);
        if message.vim_mode {
            let _ = send_key_parts(&pane.id, &["C-[", "i"]);
        }
        if let Err(error) = paste_text(&pane.id, &message.text) {
            eprintln!("[pending] paste failed for {}: {error}; re-queued", pane.id);
            PENDING_MESSAGES
                .lock()
                .expect("pending messages mutex poisoned")
                .entry(pane.id.clone())
                .or_default()
                .insert(0, message);
            continue;
        }
        if message.enter {
            let _ = send_key_parts(&pane.id, &["Enter"]);
        }
        PENDING_FLUSH_AT
            .lock()
            .expect("pending flush mutex poisoned")
            .insert(pane.id.clone(), Instant::now());
        request_pane_log_refresh_burst(state, &pane.id);
        eprintln!("[pending] delivered queued message to {}", pane.id);
    }
}

fn pending_list_json(pane_id: &str) -> serde_json::Value {
    let queues = PENDING_MESSAGES.lock().expect("pending messages mutex poisoned");
    let messages = queues.get(pane_id).cloned().unwrap_or_default();
    json!({
        "ok": true,
        "paneId": pane_id,
        "count": messages.len(),
        "messages": messages,
    })
}

async fn api_pending_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    let Some(pane_id) = query.get("paneId").filter(|value| !value.is_empty()) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({ "error": "paneId is required" }),
        );
    };
    json_response(StatusCode::OK, pending_list_json(pane_id))
}

async fn api_pending_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Json(body): Json<PendingUpdateRequest>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    if body.text.len() > 4000 {
        return json_response(StatusCode::BAD_REQUEST, json!({ "error": "text is too long" }));
    }
    let found = {
        let mut queues = PENDING_MESSAGES.lock().expect("pending messages mutex poisoned");
        queues
            .get_mut(&body.pane_id)
            .and_then(|list| list.iter_mut().find(|m| m.id == body.id))
            .map(|m| m.text = body.text.clone())
            .is_some()
    };
    if !found {
        return json_response(
            StatusCode::NOT_FOUND,
            json!({ "error": "pending message not found" }),
        );
    }
    json_response(StatusCode::OK, pending_list_json(&body.pane_id))
}

async fn api_pending_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Json(body): Json<PendingDeleteRequest>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    {
        let mut queues = PENDING_MESSAGES.lock().expect("pending messages mutex poisoned");
        if let Some(list) = queues.get_mut(&body.pane_id) {
            list.retain(|m| m.id != body.id);
        }
    }
    json_response(StatusCode::OK, pending_list_json(&body.pane_id))
}

async fn api_pending_clear(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }
    let Some(pane_id) = query.get("paneId").filter(|value| !value.is_empty()) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({ "error": "paneId is required" }),
        );
    };
    PENDING_MESSAGES
        .lock()
        .expect("pending messages mutex poisoned")
        .remove(pane_id);
    json_response(StatusCode::OK, pending_list_json(pane_id))
}

async fn api_send(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Json(body): Json<SendRequest>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }

    if body.pane_id.is_empty() {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({ "error": "paneId and text are required" }),
        );
    }

    if body.text.len() > 4000 {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({ "error": "text is too long" }),
        );
    }

    let requested_submit_key = match body.submit_key.as_deref() {
        Some("Enter") | Some("Tab") => body.submit_key.as_deref(),
        Some(_) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                json!({ "error": "invalid submitKey" }),
            );
        }
        None if body.enter != Some(false) => Some("Enter"),
        None => None,
    };

    // herdr panes aren't in `list_panes()` (that's rmux), so route them to the
    // herdr bridge before the rmux lookup below would fail to find them.
    if crate::serve::herdr::owns(&body.pane_id) {
        let enter = requested_submit_key == Some("Enter");
        return match crate::serve::herdr::send(&body.pane_id, &body.text, enter) {
            Ok(()) => {
                schedule_snapshot_refresh_soon(&state);
                json_response(StatusCode::OK, json!({ "ok": true, "backend": "herdr" }))
            }
            Err(error) => json_response(
                StatusCode::BAD_GATEWAY,
                json!({ "ok": false, "error": error }),
            ),
        };
    }

    // Same for dsh: its sessions live in its own server, not in rmux. There is
    // no separate submit key — a prompt is complete when it is sent.
    if crate::serve::dsh::owns(&body.pane_id) {
        return match crate::serve::dsh::send(&body.pane_id, &body.text) {
            Ok(()) => {
                schedule_snapshot_refresh_soon(&state);
                json_response(StatusCode::OK, json!({ "ok": true, "backend": "dsh" }))
            }
            Err(error) => json_response(
                StatusCode::BAD_GATEWAY,
                json!({ "ok": false, "error": error }),
            ),
        };
    }

    let target_pane = list_panes()
        .ok()
        .and_then(|panes| panes.into_iter().find(|pane| pane.id == body.pane_id));
    let is_claude = target_pane
        .as_ref()
        .map(|pane| pane_is_claude(&pane.session, &pane.command, &pane.title))
        .unwrap_or(false);
    let submit_key = target_pane
        .as_ref()
        .and_then(|pane| {
            if is_codex_pane(pane, "") {
                Some("Tab")
            } else {
                requested_submit_key
            }
        })
        .or(requested_submit_key);

    // Full build: record who kicked off this turn (phone vs computer) so
    // status-change push notifications only fire for phone-initiated turns.
    #[cfg(feature = "full")]
    if let Some(pane) = target_pane.as_ref() {
        crate::serve::full::push::mark_send_source(&headers, &pane.path);
    }

    // Pending queue (Claude Code only): if the agent is currently busy, hold the
    // message and let `flush_pending_messages` deliver it once the pane is idle.
    // Codex queues on its own, so it is never intercepted here.
    if is_claude
        && !body.force.unwrap_or(false)
        && matches!(cached_pane_status(&body.pane_id), Some(PaneStatus::Running))
    {
        let count = enqueue_pending(
            &body.pane_id,
            &body.text,
            submit_key.is_some(),
            body.vim_mode.unwrap_or(false),
        );
        schedule_snapshot_refresh_soon(&state);
        return json_response(
            StatusCode::OK,
            json!({
                "ok": true,
                "queued": true,
                "paneId": body.pane_id,
                "pendingCount": count,
            }),
        );
    }

    let previous_tail = capture_pane_lines(&body.pane_id, PANE_COMMAND_TAIL_LINE_COUNT);
    exit_tmux_copy_mode(&body.pane_id);

    if body.vim_mode.unwrap_or(false) {
        if let Err(error) = send_key_parts(&body.pane_id, &["C-[", "i"]) {
            return json_response(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": error }));
        }
    }

    if let Err(error) = paste_text(&body.pane_id, &body.text) {
        return json_response(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": error }));
    }

    if let Some(key) = submit_key {
        if let Err(error) = send_key_parts(&body.pane_id, &[key]) {
            return json_response(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": error }));
        }
    }

    request_pane_log_refresh_burst(&state, &body.pane_id);
    schedule_snapshot_refresh_soon(&state);
    json_response(
        StatusCode::OK,
        pane_command_response_after_command(&state, &body.pane_id, previous_tail).await,
    )
}

async fn api_refine_text(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Json(body): Json<RefineTextRequest>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }

    if body.text.len() > 4000 {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({ "error": "text is too long" }),
        );
    }

    let original = body.text;
    let fallback_text = original.trim().to_string();
    let result = tokio::task::spawn_blocking(move || refine_text_with_deepseek(&original)).await;
    match result {
        Ok(value) => json_response(StatusCode::OK, value),
        Err(error) => json_response(
            StatusCode::OK,
            json!({
                "ok": true,
                "text": fallback_text,
                "changed": false,
                "fallback": true,
                "error": error.to_string()
            }),
        ),
    }
}

async fn api_upload_image(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    body: axum::body::Bytes,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }

    if body.is_empty() {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({ "error": "image is required" }),
        );
    }

    if body.len() > 8 * 1024 * 1024 {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({ "error": "image is too large" }),
        );
    }

    let Some((extension, content_type)) = detect_image_upload(&body) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({ "error": "only jpg and png are supported" }),
        );
    };

    let pane_id = query.get("paneId").map(String::as_str).unwrap_or("unknown");
    let timestamp = now_iso().replace([':', '.'], "-");
    let filename = format!(
        "{timestamp}-{}{}",
        safe_upload_pane_name(pane_id),
        extension
    );
    let upload_dir = upload_output_dir();
    let file_path = upload_dir.join(filename);

    if !file_path.starts_with(&upload_dir) {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({ "error": "invalid upload path" }),
        );
    }

    if let Err(error) = fs::create_dir_all(&upload_dir) {
        return json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": format!("failed to create upload dir: {error}") }),
        );
    }

    if let Err(error) = fs::write(&file_path, &body) {
        return json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": format!("failed to write image: {error}") }),
        );
    }

    eprintln!(
        "[agent-monitor] uploaded image for {pane_id}: {} bytes -> {}",
        body.len(),
        file_path.display()
    );

    json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "path": file_path.to_string_lossy(),
            "size": body.len(),
            "contentType": content_type,
        }),
    )
}

async fn api_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }

    let pane_id = query.get("paneId").cloned().unwrap_or_default();
    let key = query.get("key").cloned().unwrap_or_default();
    let allowed = [
        "Enter",
        "Tab",
        "C-c",
        "C-d",
        "C-[",
        "Escape",
        "Up",
        "Down",
        "BSpace",
        "C-u",
        "VimClear",
        "VimBackspace",
    ];

    if pane_id.is_empty() || !allowed.contains(&key.as_str()) {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({ "error": "invalid paneId or key" }),
        );
    }

    // herdr keys go through its API. Its send-keys takes key *names* only, and
    // the vim composites below are rmux/tmux-specific, so map those to the
    // plain key herdr understands.
    if crate::serve::herdr::owns(&pane_id) {
        let herdr_key = match key.as_str() {
            "VimClear" | "VimBackspace" => "BSpace",
            "C-[" => "Escape",
            other => other,
        };
        return match crate::serve::herdr::send_key(&pane_id, herdr_key) {
            Ok(()) => {
                schedule_snapshot_refresh_soon(&state);
                json_response(StatusCode::OK, json!({ "ok": true, "backend": "herdr" }))
            }
            Err(error) => json_response(
                StatusCode::BAD_GATEWAY,
                json!({ "ok": false, "error": error }),
            ),
        };
    }

    let previous_tail = capture_pane_lines(&pane_id, PANE_COMMAND_TAIL_LINE_COUNT);
    exit_tmux_copy_mode(&pane_id);

    let result = match key.as_str() {
        "VimClear" => send_key_parts(&pane_id, &["C-[", "0", "D", "i"]),
        "VimBackspace" => send_key_parts(&pane_id, &["C-[", "i", "BSpace"]),
        _ => send_key_parts(&pane_id, &[key.as_str()]),
    };

    if let Err(error) = result {
        return json_response(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": error }));
    }

    request_pane_log_refresh_burst(&state, &pane_id);
    schedule_snapshot_refresh_soon(&state);
    json_response(
        StatusCode::OK,
        pane_command_response_after_command(&state, &pane_id, previous_tail).await,
    )
}

fn send_key_parts(pane_id: &str, parts: &[&str]) -> Result<(), String> {
    for part in parts {
        run_tmux(&[
            "send-keys".to_string(),
            "-t".to_string(),
            pane_id.to_string(),
            (*part).to_string(),
        ])?;
    }

    Ok(())
}

async fn api_cc_switch_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }

    match tokio::task::spawn_blocking(load_cc_switch_status).await {
        Ok(Ok(apps)) => json_response(
            StatusCode::OK,
            json!({
                "ok": true,
                "apps": apps,
            }),
        ),
        Ok(Err(error)) => json_response(
            StatusCode::OK,
            json!({
                "ok": false,
                "apps": [],
                "error": error,
            }),
        ),
        Err(error) => json_response(
            StatusCode::OK,
            json!({
                "ok": false,
                "apps": [],
                "error": error.to_string(),
            }),
        ),
    }
}

async fn api_cc_switch_switch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Json(body): Json<CcSwitchRequest>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }

    let app_type = body.app_type.trim().to_ascii_lowercase();
    if app_type != "claude" && app_type != "codex" {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": "appType must be claude or codex" }),
        );
    }

    let provider_id = body.provider_id.trim().to_string();
    if provider_id.is_empty() {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": "providerId is required" }),
        );
    }

    let result =
        tokio::task::spawn_blocking(move || switch_cc_provider(&app_type, &provider_id)).await;
    match result {
        Ok(Ok(apps)) => json_response(
            StatusCode::OK,
            json!({
                "ok": true,
                "apps": apps,
            }),
        ),
        Ok(Err(error)) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({
                "ok": false,
                "apps": [],
                "error": error,
            }),
        ),
        Err(error) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({
                "ok": false,
                "apps": [],
                "error": error.to_string(),
            }),
        ),
    }
}

async fn api_project_history(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }

    match tokio::task::spawn_blocking(load_project_history).await {
        Ok(Ok(projects)) => json_response(
            StatusCode::OK,
            json!({
                "ok": true,
                "projects": projects,
            }),
        ),
        Ok(Err(error)) => json_response(
            StatusCode::OK,
            json!({
                "ok": false,
                "projects": [],
                "error": error,
            }),
        ),
        Err(error) => json_response(
            StatusCode::OK,
            json!({
                "ok": false,
                "projects": [],
                "error": error.to_string(),
            }),
        ),
    }
}

async fn api_project_history_launch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Json(body): Json<LaunchProjectRequest>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }

    let path = body.path.trim().to_string();
    if path.is_empty() {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": "path is required" }),
        );
    }

    let agent = body.agent.trim().to_ascii_lowercase();
    if agent != "claude" && agent != "codex" {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": "agent must be claude or codex" }),
        );
    }

    let now = now_iso();
    let launch_result = tokio::task::spawn_blocking({
        let path = path.clone();
        let agent = agent.clone();
        move || launch_project_session(&path, &agent, &now)
    })
    .await;

    match launch_result {
        Ok(Ok(projects)) => {
            schedule_snapshot_refresh_soon(&state);
            json_response(
                StatusCode::OK,
                json!({
                    "ok": true,
                    "projects": projects,
                }),
            )
        }
        Ok(Err(error)) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({
                "ok": false,
                "projects": [],
                "error": error,
            }),
        ),
        Err(error) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({
                "ok": false,
                "projects": [],
                "error": error.to_string(),
            }),
        ),
    }
}

fn load_project_history() -> Result<Vec<ProjectHistoryEntry>, String> {
    {
        let cache = PROJECT_HISTORY_CACHE
            .lock()
            .map_err(|_| "project history cache lock is poisoned".to_string())?;
        if let Some(entries) = cache.as_ref() {
            return Ok(sorted_project_history(entries.clone()));
        }
    }

    let path = project_history_path();
    let entries = if path.exists() {
        let raw = fs::read_to_string(&path)
            .map_err(|error| format!("failed to read project history: {error}"))?;
        serde_json::from_str::<Vec<ProjectHistoryEntry>>(&raw)
            .map_err(|error| format!("failed to parse project history: {error}"))?
    } else {
        Vec::new()
    };
    let entries = sorted_project_history(entries);
    let mut cache = PROJECT_HISTORY_CACHE
        .lock()
        .map_err(|_| "project history cache lock is poisoned".to_string())?;
    *cache = Some(entries.clone());
    Ok(entries)
}

fn sorted_project_history(mut entries: Vec<ProjectHistoryEntry>) -> Vec<ProjectHistoryEntry> {
    entries.sort_by(|a, b| {
        b.last_seen_at
            .cmp(&a.last_seen_at)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    entries.truncate(80);
    entries
}

fn save_project_history(entries: &[ProjectHistoryEntry]) -> Result<(), String> {
    let path = project_history_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create project history directory: {error}"))?;
    }
    let data = serde_json::to_vec_pretty(entries)
        .map_err(|error| format!("failed to encode project history: {error}"))?;
    fs::write(&path, [data, b"\n".to_vec()].concat())
        .map_err(|error| format!("failed to write project history: {error}"))
}

fn remember_project_history_entries(updates: Vec<(String, String)>, now: &str) {
    if updates.is_empty() {
        return;
    }

    let mut entries = match load_project_history() {
        Ok(entries) => entries,
        Err(error) => {
            eprintln!("[agent-monitor] failed to load project history: {error}");
            Vec::new()
        }
    };

    let mut updates_by_path: HashMap<String, Vec<String>> = HashMap::new();
    for (path, agent) in updates {
        if !is_launchable_project_path(&path) {
            continue;
        }
        let agents = updates_by_path.entry(path).or_default();
        if !agents.iter().any(|item| item == &agent) {
            agents.push(agent);
        }
    }

    let mut changed = false;
    for (path, agents) in updates_by_path {
        let agent = entries
            .iter()
            .find(|entry| {
                entry.path == path && agents.iter().any(|agent| agent == &entry.last_agent)
            })
            .map(|entry| entry.last_agent.clone())
            .unwrap_or_else(|| {
                if agents.iter().any(|agent| agent == "claude") {
                    "claude".to_string()
                } else {
                    agents[0].clone()
                }
            });
        if upsert_project_history_entry(&mut entries, &path, &agent, now, false) {
            changed = true;
        }
    }

    if changed {
        persist_project_history(entries);
    }
}

fn persist_project_history(entries: Vec<ProjectHistoryEntry>) {
    let entries = sorted_project_history(entries);
    if let Err(error) = save_project_history(&entries) {
        eprintln!("[agent-monitor] failed to save project history: {error}");
        return;
    }

    if let Ok(mut cache) = PROJECT_HISTORY_CACHE.lock() {
        *cache = Some(entries);
    }
}

fn upsert_project_history_entry(
    entries: &mut Vec<ProjectHistoryEntry>,
    path: &str,
    agent: &str,
    now: &str,
    increment_launch_count: bool,
) -> bool {
    if let Some(entry) = entries.iter_mut().find(|entry| entry.path == path) {
        let next_name = project_name_from_path(path);
        let mut changed = false;
        if entry.name != next_name {
            entry.name = next_name;
            changed = true;
        }
        if entry.last_agent != agent {
            entry.last_agent = agent.to_string();
            entry.last_seen_at = now.to_string();
            changed = true;
        }
        if increment_launch_count {
            entry.launch_count = entry.launch_count.saturating_add(1);
            entry.last_seen_at = now.to_string();
            changed = true;
        }
        return changed;
    }

    entries.push(ProjectHistoryEntry {
        path: path.to_string(),
        name: project_name_from_path(path),
        last_agent: agent.to_string(),
        last_seen_at: now.to_string(),
        launch_count: u32::from(increment_launch_count),
    });
    true
}

fn is_launchable_project_path(path: &str) -> bool {
    let path = Path::new(path);
    path.is_absolute() && path.is_dir()
}

fn launch_project_session(
    path: &str,
    agent: &str,
    now: &str,
) -> Result<Vec<ProjectHistoryEntry>, String> {
    if !is_launchable_project_path(path) {
        return Err(format!("project path is not an existing directory: {path}"));
    }

    let command = agent_launch_command(agent)?;
    let session_name = project_session_name(agent, path)?;

    let existing_session = tmux_command()
        .args(["has-session", "-t", &session_name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("failed to query tmux session: {error}"))?;
    if existing_session.success() {
        return load_project_history();
    }

    let output = tmux_command()
        .args([
            "new-session",
            "-d",
            "-s",
            &session_name,
            "-c",
            path,
            &command,
        ])
        .output()
        .map_err(|error| format!("failed to launch {command}: {error}"))?;
    if output.status.success() {
        pin_window_size(&session_name);
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("tmux exited with {}", output.status)
        } else {
            stderr
        });
    }

    let mut entries = load_project_history()?;
    let _ = upsert_project_history_entry(&mut entries, path, agent, now, true);
    persist_project_history(entries);

    load_project_history()
}

fn load_cc_switch_status() -> Result<Vec<CcSwitchApp>, String> {
    let rows = load_cc_switch_provider_rows()?;
    Ok(cc_switch_apps_from_rows(rows))
}

fn load_cc_switch_provider_rows() -> Result<Vec<CcSwitchProviderRow>, String> {
    let db_path = cc_switch_db_path();
    if !db_path.exists() {
        return Err(format!("missing cc-switch db: {}", db_path.display()));
    }

    let sql = "select id, app_type, name, is_current, settings_config from providers where app_type in ('claude','codex') order by app_type, is_current desc, sort_index, name;";
    let output = Command::new("/usr/bin/sqlite3")
        .arg("-readonly")
        .arg("-cmd")
        .arg(".timeout 5000")
        .arg("-json")
        .arg(&db_path)
        .arg(sql)
        .output()
        .map_err(|error| format!("failed to run sqlite3: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("sqlite3 exited with {}", output.status)
        } else {
            stderr
        });
    }

    serde_json::from_slice::<Vec<CcSwitchProviderRow>>(&output.stdout)
        .map_err(|error| format!("failed to parse cc-switch providers: {error}"))
}

fn cc_switch_apps_from_rows(rows: Vec<CcSwitchProviderRow>) -> Vec<CcSwitchApp> {
    ["claude", "codex"]
        .iter()
        .map(|app_type| {
            let providers = rows
                .iter()
                .filter(|row| row.app_type == *app_type)
                .map(cc_switch_provider_from_row)
                .collect::<Vec<_>>();
            let active_provider_id = providers
                .iter()
                .find(|provider| provider.is_current)
                .map(|provider| provider.id.clone());
            CcSwitchApp {
                app_type: (*app_type).to_string(),
                title: cc_switch_app_title(app_type),
                active_provider_id,
                providers,
            }
        })
        .collect()
}

fn cc_switch_provider_from_row(row: &CcSwitchProviderRow) -> CcSwitchProvider {
    let config = parse_cc_switch_config(row).ok();
    CcSwitchProvider {
        id: row.id.clone(),
        app_type: row.app_type.clone(),
        name: row.name.clone(),
        is_current: row.is_current != 0,
        base_url: config
            .as_ref()
            .and_then(|value| cc_switch_base_url(&row.app_type, value)),
        has_api_key: config
            .as_ref()
            .map(|value| cc_switch_has_api_key(&row.app_type, value))
            .unwrap_or(false),
    }
}

fn parse_cc_switch_config(row: &CcSwitchProviderRow) -> Result<serde_json::Value, String> {
    if row.settings_config.trim().is_empty() {
        return Err(format!(
            "provider has empty settings_config: {} ({})",
            row.name, row.id
        ));
    }
    serde_json::from_str::<serde_json::Value>(&row.settings_config).map_err(|error| {
        format!(
            "provider has invalid settings_config JSON: {} ({}) - {error}",
            row.name, row.id
        )
    })
}

fn cc_switch_base_url(app_type: &str, config: &serde_json::Value) -> Option<String> {
    match app_type {
        "claude" => config
            .pointer("/env/ANTHROPIC_BASE_URL")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string),
        "codex" => config
            .get("config")
            .and_then(serde_json::Value::as_str)
            .and_then(extract_codex_base_url),
        _ => None,
    }
}

fn cc_switch_has_api_key(app_type: &str, config: &serde_json::Value) -> bool {
    match app_type {
        "claude" => {
            config
                .pointer("/env/ANTHROPIC_API_KEY")
                .and_then(serde_json::Value::as_str)
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false)
                || config
                    .pointer("/env/ANTHROPIC_AUTH_TOKEN")
                    .and_then(serde_json::Value::as_str)
                    .map(|value| !value.trim().is_empty())
                    .unwrap_or(false)
        }
        "codex" => config
            .pointer("/auth/OPENAI_API_KEY")
            .and_then(serde_json::Value::as_str)
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false),
        _ => false,
    }
}

fn extract_codex_base_url(config: &str) -> Option<String> {
    let marker = "base_url";
    for line in config.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with(marker) {
            continue;
        }
        let (_, value) = trimmed.split_once('=')?;
        let value = value.trim().trim_matches('"').trim_matches('\'');
        let value = value.trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

fn cc_switch_app_title(app_type: &str) -> String {
    match app_type {
        "claude" => "Claude Code".to_string(),
        "codex" => "Codex".to_string(),
        _ => app_type.to_string(),
    }
}

fn switch_cc_provider(app_type: &str, provider_id: &str) -> Result<Vec<CcSwitchApp>, String> {
    let _guard = CC_SWITCH_LOCK
        .lock()
        .map_err(|_| "cc-switch operation lock is poisoned".to_string())?;
    let rows = load_cc_switch_provider_rows()?;
    let target = rows
        .iter()
        .find(|row| row.app_type == app_type && row.id == provider_id)
        .ok_or_else(|| format!("provider not found: {app_type}/{provider_id}"))?;
    let validated = validate_cc_switch_provider_for_switch(target)?;
    let rollback = capture_cc_switch_db_rollback_state(app_type, &rows)?;
    let settings_update = prepare_cc_switch_settings_update(app_type, provider_id)?;

    if let Err(error) =
        update_cc_switch_db_for_provider(app_type, provider_id, &validated.normalized_config)
    {
        cleanup_cc_switch_settings_update(&settings_update);
        return Err(error);
    }
    if let Err(error) = verify_cc_switch_db_active_provider(app_type, provider_id) {
        let rollback_message = rollback_cc_switch_db(app_type, &rollback)
            .map(|_| "cc-switch db rolled back".to_string())
            .unwrap_or_else(|rollback_error| {
                format!("cc-switch db rollback failed: {rollback_error}")
            });
        cleanup_cc_switch_settings_update(&settings_update);
        return Err(format!("{error}; {rollback_message}"));
    }

    if let Err(error) = commit_cc_switch_settings_update(&settings_update) {
        let rollback_message = rollback_cc_switch_db(app_type, &rollback)
            .map(|_| "cc-switch db rolled back".to_string())
            .unwrap_or_else(|rollback_error| {
                format!("cc-switch db rollback failed: {rollback_error}")
            });
        cleanup_cc_switch_settings_update(&settings_update);
        return Err(format!("{error}; {rollback_message}"));
    }

    restart_cc_switch_app()?;
    load_cc_switch_status()
}

fn update_cc_switch_db_for_provider(
    app_type: &str,
    provider_id: &str,
    normalized_config: &str,
) -> Result<(), String> {
    let db_path = cc_switch_db_path();
    let escaped_app_type = escape_sql(app_type);
    let escaped_provider_id = escape_sql(provider_id);
    let escaped_config = escape_sql(normalized_config);
    let sql = format!(
        "PRAGMA busy_timeout = 5000;\
         BEGIN IMMEDIATE;\
         UPDATE providers SET is_current = CASE WHEN id = '{escaped_provider_id}' THEN 1 ELSE 0 END WHERE app_type = '{escaped_app_type}';\
         INSERT INTO proxy_live_backup (app_type, original_config, backed_up_at) VALUES ('{escaped_app_type}', '{escaped_config}', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))\
         ON CONFLICT(app_type) DO UPDATE SET original_config = excluded.original_config, backed_up_at = excluded.backed_up_at;\
         COMMIT;"
    );
    let output = Command::new("/usr/bin/sqlite3")
        .arg(&db_path)
        .arg(sql)
        .output()
        .map_err(|error| format!("failed to update cc-switch db: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("sqlite3 exited with {}", output.status)
        } else {
            stderr
        });
    }

    Ok(())
}

fn verify_cc_switch_db_active_provider(app_type: &str, provider_id: &str) -> Result<(), String> {
    let active_provider_ids = load_cc_switch_provider_rows()?
        .into_iter()
        .filter(|row| row.app_type == app_type && row.is_current != 0)
        .map(|row| row.id)
        .collect::<Vec<_>>();

    if active_provider_ids.len() == 1
        && active_provider_ids.first() == Some(&provider_id.to_string())
    {
        return Ok(());
    }

    Err(format!(
        "cc-switch db active provider mismatch for {app_type}: expected {provider_id}, got {}",
        if active_provider_ids.is_empty() {
            "none".to_string()
        } else {
            active_provider_ids.join(",")
        }
    ))
}

fn validate_cc_switch_provider_for_switch(
    row: &CcSwitchProviderRow,
) -> Result<ValidatedCcSwitchProvider, String> {
    let config = parse_cc_switch_config(row)?;
    let _ =
        cc_switch_base_url(&row.app_type, &config).ok_or_else(|| match row.app_type.as_str() {
            "claude" => format!(
                "provider missing ANTHROPIC_BASE_URL: {} ({})",
                row.name, row.id
            ),
            "codex" => format!("provider missing base_url: {} ({})", row.name, row.id),
            _ => format!("unsupported app type: {}", row.app_type),
        })?;

    Ok(ValidatedCcSwitchProvider {
        normalized_config: config.to_string(),
    })
}

fn prepare_cc_switch_settings_update(
    app_type: &str,
    provider_id: &str,
) -> Result<PreparedCcSwitchSettingsUpdate, String> {
    let settings_path = cc_switch_settings_path();
    if !settings_path.exists() {
        return Err(format!(
            "missing cc-switch settings: {}",
            settings_path.display()
        ));
    }

    let raw = fs::read_to_string(&settings_path)
        .map_err(|error| format!("failed to read cc-switch settings: {error}"))?;
    let mut settings = serde_json::from_str::<serde_json::Value>(&raw)
        .map_err(|error| format!("failed to parse cc-switch settings: {error}"))?;
    let Some(object) = settings.as_object_mut() else {
        return Err("cc-switch settings must be a JSON object".to_string());
    };

    let key = match app_type {
        "claude" => "currentProviderClaude",
        "codex" => "currentProviderCodex",
        _ => return Err(format!("unsupported app type: {app_type}")),
    };
    object.insert(
        key.to_string(),
        serde_json::Value::String(provider_id.to_string()),
    );

    let formatted = serde_json::to_string_pretty(&settings)
        .map_err(|error| format!("failed to encode cc-switch settings: {error}"))?;
    let tmp_path = cc_switch_settings_tmp_path(&settings_path);
    fs::write(&tmp_path, format!("{formatted}\n"))
        .map_err(|error| format!("failed to write temp cc-switch settings: {error}"))?;

    Ok(PreparedCcSwitchSettingsUpdate {
        settings_path,
        tmp_path,
    })
}

fn commit_cc_switch_settings_update(update: &PreparedCcSwitchSettingsUpdate) -> Result<(), String> {
    fs::rename(&update.tmp_path, &update.settings_path)
        .map_err(|error| format!("failed to replace cc-switch settings: {error}"))
}

fn cleanup_cc_switch_settings_update(update: &PreparedCcSwitchSettingsUpdate) {
    let _ = fs::remove_file(&update.tmp_path);
}

fn cc_switch_settings_tmp_path(settings_path: &std::path::Path) -> PathBuf {
    let file_name = settings_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("settings.json");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    settings_path.with_file_name(format!(
        ".{file_name}.agent-monitor-{}-{nonce}.tmp",
        std::process::id()
    ))
}

fn capture_cc_switch_db_rollback_state(
    app_type: &str,
    rows: &[CcSwitchProviderRow],
) -> Result<CcSwitchDbRollbackState, String> {
    let active_provider_ids = rows
        .iter()
        .filter(|row| row.app_type == app_type && row.is_current != 0)
        .map(|row| row.id.clone())
        .collect::<Vec<_>>();
    let proxy_backup_config = load_cc_switch_proxy_backup(app_type)?;

    Ok(CcSwitchDbRollbackState {
        active_provider_ids,
        proxy_backup_config,
    })
}

fn load_cc_switch_proxy_backup(app_type: &str) -> Result<Option<String>, String> {
    let db_path = cc_switch_db_path();
    let escaped_app_type = escape_sql(app_type);
    let sql = format!(
        "select original_config from proxy_live_backup where app_type = '{escaped_app_type}' limit 1;"
    );
    let output = Command::new("/usr/bin/sqlite3")
        .arg("-readonly")
        .arg("-cmd")
        .arg(".timeout 5000")
        .arg("-json")
        .arg(&db_path)
        .arg(sql)
        .output()
        .map_err(|error| format!("failed to read cc-switch proxy backup: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("sqlite3 exited with {}", output.status)
        } else {
            stderr
        });
    }

    let rows = serde_json::from_slice::<Vec<CcSwitchProxyBackupRow>>(&output.stdout)
        .map_err(|error| format!("failed to parse cc-switch proxy backup: {error}"))?;
    Ok(rows.first().map(|row| row.original_config.clone()))
}

fn rollback_cc_switch_db(app_type: &str, rollback: &CcSwitchDbRollbackState) -> Result<(), String> {
    let db_path = cc_switch_db_path();
    let escaped_app_type = escape_sql(app_type);
    let active_sql = if rollback.active_provider_ids.is_empty() {
        format!("UPDATE providers SET is_current = 0 WHERE app_type = '{escaped_app_type}';")
    } else {
        let ids = rollback
            .active_provider_ids
            .iter()
            .map(|id| format!("'{}'", escape_sql(id)))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "UPDATE providers SET is_current = CASE WHEN id IN ({ids}) THEN 1 ELSE 0 END WHERE app_type = '{escaped_app_type}';"
        )
    };
    let backup_sql = if let Some(config) = &rollback.proxy_backup_config {
        let escaped_config = escape_sql(config);
        format!(
            "INSERT INTO proxy_live_backup (app_type, original_config, backed_up_at) VALUES ('{escaped_app_type}', '{escaped_config}', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))\
             ON CONFLICT(app_type) DO UPDATE SET original_config = excluded.original_config, backed_up_at = excluded.backed_up_at;"
        )
    } else {
        format!("DELETE FROM proxy_live_backup WHERE app_type = '{escaped_app_type}';")
    };
    let sql = format!("PRAGMA busy_timeout = 5000;BEGIN IMMEDIATE;{active_sql}{backup_sql}COMMIT;");
    let output = Command::new("/usr/bin/sqlite3")
        .arg(&db_path)
        .arg(sql)
        .output()
        .map_err(|error| format!("failed to rollback cc-switch db: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("sqlite3 exited with {}", output.status)
        } else {
            stderr
        });
    }

    Ok(())
}

fn restart_cc_switch_app() -> Result<(), String> {
    if cc_switch_skip_restart() {
        return Ok(());
    }

    stop_cc_switch_app()?;

    if cc_switch_proxy_is_listening() {
        return Err("CC Switch stopped, but proxy port 15721 is still occupied".to_string());
    }

    let app_path = cc_switch_app_path();
    let mut last_error = None;
    for attempt in 1..=2 {
        if let Err(error) = open_cc_switch_app(&app_path) {
            last_error = Some(error);
        } else if wait_for_cc_switch_ready(Duration::from_secs(60)) {
            return Ok(());
        } else {
            last_error =
                Some("CC Switch proxy port 15721 did not become ready in time".to_string());
        }

        if attempt == 1 {
            let _ = stop_cc_switch_app();
        }
    }

    Err(last_error.unwrap_or_else(|| "failed to restart CC Switch".to_string()))
}

fn stop_cc_switch_app() -> Result<(), String> {
    let _ = Command::new("/usr/bin/osascript")
        .args(["-e", "tell application \"CC Switch\" to quit"])
        .output();

    if wait_for_cc_switch_stopped(Duration::from_secs(8)) {
        return Ok(());
    }

    let _ = Command::new("/usr/bin/pkill")
        .args(["-TERM", "-x", "cc-switch"])
        .output();
    if wait_for_cc_switch_stopped(Duration::from_secs(10)) {
        return Ok(());
    }

    let _ = Command::new("/usr/bin/pkill")
        .args(["-KILL", "-x", "cc-switch"])
        .output();
    if wait_for_cc_switch_stopped(Duration::from_secs(5)) {
        return Ok(());
    }

    Err("CC Switch did not exit in time".to_string())
}

fn open_cc_switch_app(app_path: &str) -> Result<(), String> {
    let mut command = Command::new("/usr/bin/open");
    if app_path.contains('/') || app_path.ends_with(".app") {
        command.arg(app_path);
    } else {
        command.arg("-a").arg(app_path);
    }

    let status = command
        .status()
        .map_err(|error| format!("failed to open CC Switch: {error}"))?;
    if !status.success() {
        return Err(format!("open exited with {status}"));
    }

    Ok(())
}

fn wait_for_cc_switch_stopped(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !cc_switch_process_is_running() {
            return true;
        }
        thread::sleep(Duration::from_millis(250));
    }
    !cc_switch_process_is_running()
}

fn wait_for_cc_switch_ready(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cc_switch_process_is_running() && cc_switch_proxy_is_listening() {
            return true;
        }
        thread::sleep(Duration::from_millis(500));
    }

    false
}

fn cc_switch_process_is_running() -> bool {
    Command::new("/usr/bin/pgrep")
        .args(["-x", "cc-switch"])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn cc_switch_proxy_is_listening() -> bool {
    let Ok(output) = Command::new("/usr/sbin/lsof")
        .args(["-nP", "-iTCP:15721", "-sTCP:LISTEN"])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .skip(1)
        .any(|line| line.split_whitespace().next() == Some("cc-switch"))
}

fn escape_sql(value: &str) -> String {
    value.replace('\'', "''")
}

async fn api_kill_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    Json(body): Json<KillSessionRequest>,
) -> Response<Body> {
    if !is_authed(&state, &headers, &query) {
        return json_response(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }

    if let Some(pane_id) = body.pane_id.filter(|value| !value.is_empty()) {
        eprintln!("[agent-monitor] closing tmux pane {pane_id}");
        if let Err(error) = run_tmux(&["kill-pane".to_string(), "-t".to_string(), pane_id]) {
            return json_response(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": error }));
        }

        broadcast_snapshot(&state);
        return json_response(StatusCode::OK, json!({ "ok": true }));
    }

    let Some(session) = body.session.filter(|value| !value.is_empty()) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({ "error": "paneId or session is required" }),
        );
    };

    eprintln!("[agent-monitor] rejected session-level kill for {session}");
    json_response(
        StatusCode::BAD_REQUEST,
        json!({ "error": "session-level kill is disabled; refresh the client and close a pane instead" }),
    )
}

async fn snapshot_ws(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    if !is_authed(&state, &headers, &query) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    ws.on_upgrade(move |socket| async move {
        handle_snapshot_socket(socket, state).await;
    })
    .into_response()
}

async fn handle_snapshot_socket(socket: WebSocket, state: AppState) {
    let (mut sender, mut receiver) = socket.split();
    let hello = json!({
        "type": "hello",
        "snapshot": build_snapshot(),
    });
    if sender
        .send(Message::Text(hello.to_string().into()))
        .await
        .is_err()
    {
        return;
    }

    let mut snapshots = state.snapshots.subscribe();
    loop {
        tokio::select! {
            message = snapshots.recv() => {
                let Ok(message) = message else { break };
                if sender.send(Message::Text(message.to_string().into())).await.is_err() {
                    break;
                }
            }
            incoming = receiver.next() => {
                if incoming.is_none() {
                    break;
                }
            }
        }
    }
}

async fn pane_log_ws(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    if !is_authed(&state, &headers, &query) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    ws.on_upgrade(move |socket| async move {
        handle_pane_log_socket(socket, query, state.pane_log_refreshes.subscribe()).await;
    })
    .into_response()
}

async fn handle_pane_log_socket(
    socket: WebSocket,
    query: HashMap<String, String>,
    mut refreshes: broadcast::Receiver<String>,
) {
    let pane_id = query.get("paneId").cloned().unwrap_or_default();
    if pane_id.is_empty() {
        let (mut sender, _) = socket.split();
        let _ = sender
            .send(Message::Text(
                json!({ "type": "error", "error": "paneId is required" })
                    .to_string()
                    .into(),
            ))
            .await;
        return;
    }

    let line_count = pane_log_line_count(query.get("lines"));
    let (mut sender, mut receiver) = socket.split();

    async fn send_pane_tail(
        sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
        pane_id: &str,
        tail: &str,
    ) -> Result<(), axum::Error> {
        sender
            .send(Message::Text(
                json!({
                    "type": "paneLog",
                    "paneId": pane_id,
                    "tail": tail,
                    "capturedAt": now_iso(),
                })
                .to_string()
                .into(),
            ))
            .await
    }

    async fn capture_and_send_if_changed(
        sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
        pane_id: &str,
        line_count: usize,
        last_tail: &mut String,
    ) -> Result<(), axum::Error> {
        let next_tail = capture_pane_lines(pane_id, line_count);
        if next_tail == *last_tail {
            return Ok(());
        }

        *last_tail = next_tail;
        send_pane_tail(sender, pane_id, last_tail).await
    }

    let mut last_tail = capture_pane_lines(&pane_id, line_count);
    if send_pane_tail(&mut sender, &pane_id, &last_tail)
        .await
        .is_err()
    {
        return;
    }

    let mut interval = tokio::time::interval(Duration::from_millis(350));
    loop {
        tokio::select! {
            _ = interval.tick() => {
                if capture_and_send_if_changed(&mut sender, &pane_id, line_count, &mut last_tail).await.is_err() {
                    break;
                }
            }
            incoming = receiver.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) if text.contains("refresh") => {
                        if capture_and_send_if_changed(&mut sender, &pane_id, line_count, &mut last_tail).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Binary(data))) if data.windows(7).any(|item| item == b"refresh") => {
                        if capture_and_send_if_changed(&mut sender, &pane_id, line_count, &mut last_tail).await.is_err() {
                            break;
                        }
                    }
                    Some(_) => {}
                    None => break,
                }
            }
            refresh = refreshes.recv() => {
                match refresh {
                    Ok(refresh_pane_id) if refresh_pane_id == pane_id => {
                        if capture_and_send_if_changed(&mut sender, &pane_id, line_count, &mut last_tail).await.is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

async fn terminal_ws(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    if !is_authed(&state, &headers, &query) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    ws.on_upgrade(move |socket| async move {
        handle_terminal_socket(socket, query).await;
    })
    .into_response()
}

async fn handle_terminal_socket(mut socket: WebSocket, query: HashMap<String, String>) {
    let pane_id = query.get("paneId").cloned().unwrap_or_default();
    let requested_cols = query
        .get("cols")
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(96);
    let requested_rows = query
        .get("rows")
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(28);

    let snapshot = build_snapshot();
    let Some(pane) = snapshot
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .cloned()
    else {
        let _ = socket
            .send(Message::Text(
                json!({ "type": "error", "error": "pane not found" })
                    .to_string()
                    .into(),
            ))
            .await;
        let _ = socket.close().await;
        return;
    };

    let _ = run_tmux(&[
        "select-window".to_string(),
        "-t".to_string(),
        format!("{}:{}", pane.session, pane.window_index),
    ]);
    let _ = run_tmux(&["select-pane".to_string(), "-t".to_string(), pane.id.clone()]);

    let pty_system = native_pty_system();
    let pair = match pty_system.openpty(PtySize {
        rows: requested_rows.clamp(8, 80),
        cols: requested_cols.clamp(20, 240),
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(pair) => pair,
        Err(error) => {
            send_terminal_error(&mut socket, format!("failed to open pty: {error}")).await;
            return;
        }
    };

    let mut command = CommandBuilder::new(tmux_program_path());
    command.arg("attach-session");
    command.arg("-t");
    command.arg(&pane.session);
    if !pane.path.is_empty() {
        command.cwd(&pane.path);
    }
    command.env("TERM", "xterm-256color");
    sanitize_tmux_command_builder(&mut command);

    let mut child = match pair.slave.spawn_command(command) {
        Ok(child) => child,
        Err(error) => {
            send_terminal_error(&mut socket, format!("failed to attach tmux: {error}")).await;
            return;
        }
    };
    drop(pair.slave);

    let mut reader = match pair.master.try_clone_reader() {
        Ok(reader) => reader,
        Err(error) => {
            let _ = child.kill();
            send_terminal_error(&mut socket, format!("failed to read pty: {error}")).await;
            return;
        }
    };

    let mut writer = match pair.master.take_writer() {
        Ok(writer) => writer,
        Err(error) => {
            let _ = child.kill();
            send_terminal_error(&mut socket, format!("failed to write pty: {error}")).await;
            return;
        }
    };

    let (event_tx, mut event_rx) = mpsc::channel::<TerminalEvent>(128);
    std::thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    let _ = event_tx.blocking_send(TerminalEvent::Exit);
                    break;
                }
                Ok(count) => {
                    let data = String::from_utf8_lossy(&buffer[..count]).into_owned();
                    if event_tx.blocking_send(TerminalEvent::Data(data)).is_err() {
                        break;
                    }
                }
                Err(_) => {
                    let _ = event_tx.blocking_send(TerminalEvent::Exit);
                    break;
                }
            }
        }
    });

    if socket
        .send(Message::Text(json!({ "type": "ready" }).to_string().into()))
        .await
        .is_err()
    {
        let _ = child.kill();
        return;
    }

    loop {
        tokio::select! {
            event = event_rx.recv() => {
                match event {
                    Some(TerminalEvent::Data(first)) => {
                        let data = collect_terminal_output(first, &mut event_rx).await;
                        if socket.send(Message::Text(json!({ "type": "data", "data": data }).to_string().into())).await.is_err() {
                            break;
                        }
                    }
                    Some(TerminalEvent::Exit) | None => {
                        let _ = socket.send(Message::Text(json!({ "type": "exit", "exitCode": 0, "signal": null }).to_string().into())).await;
                        break;
                    }
                }
            }
            incoming = socket.next() => {
                let Some(Ok(message)) = incoming else { break };
                match message {
                    Message::Text(text) => {
                        if let Ok(message) = serde_json::from_str::<TerminalMessage>(&text) {
                            handle_terminal_message(message, &pane_id, &pair.master, &mut writer);
                        }
                    }
                    Message::Binary(bytes) => {
                        if let Ok(text) = String::from_utf8(bytes.to_vec()) {
                            if let Ok(message) = serde_json::from_str::<TerminalMessage>(&text) {
                                handle_terminal_message(message, &pane_id, &pair.master, &mut writer);
                            }
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
    }

    let _ = child.kill();
}

async fn collect_terminal_output(
    first: String,
    event_rx: &mut mpsc::Receiver<TerminalEvent>,
) -> String {
    let mut output = first;
    let delay = tokio::time::sleep(Duration::from_millis(16));
    tokio::pin!(delay);

    loop {
        tokio::select! {
            _ = &mut delay => break,
            event = event_rx.recv(), if output.len() < 32_000 => {
                match event {
                    Some(TerminalEvent::Data(data)) => output.push_str(&data),
                    Some(TerminalEvent::Exit) | None => break,
                }
            }
        }
    }

    output
}

fn handle_terminal_message(
    message: TerminalMessage,
    pane_id: &str,
    master: &Box<dyn portable_pty::MasterPty + Send>,
    writer: &mut Box<dyn Write + Send>,
) {
    match message.message_type.as_deref() {
        Some("input") => {
            if let Some(data) = message.data {
                exit_tmux_copy_mode(pane_id);
                let _ = writer.write_all(data.as_bytes());
                let _ = writer.flush();
            }
        }
        Some("resize") => {
            if let (Some(cols), Some(rows)) = (message.cols, message.rows) {
                let _ = master.resize(PtySize {
                    cols: cols.clamp(20, 240),
                    rows: rows.clamp(8, 80),
                    pixel_width: 0,
                    pixel_height: 0,
                });
            }
        }
        Some("scroll") => {
            if let Some(lines) = message.lines {
                scroll_tmux_pane(pane_id, lines);
            }
        }
        _ => {}
    }
}

async fn send_terminal_error(socket: &mut WebSocket, error: String) {
    let _ = socket
        .send(Message::Text(
            json!({ "type": "error", "error": error })
                .to_string()
                .into(),
        ))
        .await;
    let _ = socket.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(session: &str) -> BasePane {
        BasePane {
            id: "%1".into(),
            target: "t".into(),
            session: session.into(),
            window_index: "0".into(),
            window_name: "w".into(),
            pane_index: "0".into(),
            command: "node".into(),
            path: "/work/proj".into(),
            active: true,
            pid: Some(1),
            title: "".into(),
        }
    }

    #[test]
    fn fresh_session_file_means_running() {
        let (s, _) = infer_status(&pane("cx_proj_1a2b3c4d"), "", false, Some(2.0));
        assert_eq!(s, PaneStatus::Running);
    }

    #[test]
    fn stale_session_file_means_idle() {
        let (s, _) = infer_status(&pane("cx_proj_1a2b3c4d"), "just some quiet output", false, Some(600.0));
        assert_eq!(s, PaneStatus::Idle);
    }

    #[test]
    fn no_file_falls_back_to_terminal_change() {
        // Unknown file age → use the terminal change signal.
        let (running, _) = infer_status(&pane("cx_proj_1a2b3c4d"), "output", true, None);
        assert_eq!(running, PaneStatus::Running);
        let (idle, _) = infer_status(&pane("cx_proj_1a2b3c4d"), "output", false, None);
        assert_eq!(idle, PaneStatus::Idle);
    }

    #[test]
    fn permission_prompt_always_waiting() {
        // Even with a fresh file, an on-screen prompt wins (needs the user).
        let (s, _) = infer_status(&pane("cc_proj_1a2b3c4d"), "Do you want to proceed? (y/n)", false, Some(1.0));
        assert_eq!(s, PaneStatus::Waiting);
    }

    #[test]
    fn error_output_means_failed() {
        let (s, _) = infer_status(&pane("cx_proj_1a2b3c4d"), "thread panicked\nerror: boom", false, Some(1.0));
        assert_eq!(s, PaneStatus::Failed);
    }

    #[test]
    fn session_prefix_wins_over_tail_content() {
        // A Claude pane whose terminal is full of "codex"/"gpt-" must still be
        // classified as claude, so status looks at the right session file.
        let cc = pane("cc_proj_1a2b3c4d");
        assert_eq!(agent_kind_for_pane(&cc, "talking about codex and gpt-5"), Some("claude"));
        let cx = pane("cx_proj_1a2b3c4d");
        assert_eq!(agent_kind_for_pane(&cx, "mentions claude a lot"), Some("codex"));
    }

    #[test]
    fn provider_session_prefix_is_agent_identity() {
        let cc = pane("cc-glm_proj_1a2b3c4d");
        assert_eq!(
            agent_kind_for_pane(&cc, "talking about codex and gpt-5"),
            Some("claude")
        );
        assert!(pane_is_claude(&cc.session, &cc.command, &cc.title));

        let cx = pane("cx-openai_proj_1a2b3c4d");
        assert_eq!(
            agent_kind_for_pane(&cx, "mentions claude a lot"),
            Some("codex")
        );
        assert!(!pane_is_claude(&cx.session, &cx.command, &cx.title));
        assert!(is_codex_pane(&cx, ""));
    }

    #[test]
    fn hook_status_overrides_terminal_inference() {
        let _guard = crate::state::TEST_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("AMUX_STATE_DIR", tmp.path());
        crate::state::record_status(
            Some("%1".to_string()),
            Some("cc-glm_proj_1a2b3c4d".to_string()),
            crate::state::HookState::Done,
            Some("test".to_string()),
            None,
            Some("hook says complete".to_string()),
        )
        .unwrap();

        let pane = pane("cc-glm_proj_1a2b3c4d");
        let (status, reason) = hook_status_for_pane(&pane).unwrap();
        assert_eq!(status, PaneStatus::Done);
        assert_eq!(reason, "hook says complete");

        std::env::remove_var("AMUX_STATE_DIR");
    }

    #[test]
    fn live_work_signal_detected() {
        // codex spinner
        assert!(agent_actively_working(
            "some scrollback\n• Working (11m 14s • esc to interrupt)\n› prompt"
        ));
        // claude interruptible spinner
        assert!(agent_actively_working(
            "✻ Thinking (5s · esc to interrupt)\n"
        ));
        // claude streaming spinner: "✽ Baking… (3m 13s · ↓ 10.9k tokens)"
        assert!(agent_actively_working(
            "scrollback\n✽ Baking… (3m 13s · ↓ 10.9k tokens)\n"
        ));
        // regression: a FINISHED subagent result line carries "· ↓ … tokens" but
        // no "… (" and no closing paren — must NOT read as live work.
        assert!(!agent_actively_working(
            "✻ Churned for 1m 23s\n❯\n  -- INSERT --\n  ⏺ main\n-purpose  Research FlyingFox + Hummingbird                 43m 27s · ↓ 2.2k tokens"
        ));
        // the two markers must be on the SAME line, not merely both present
        assert!(!agent_actively_working(
            "done… (earlier)\n❯ \nsubagent used 2.2k tokens)"
        ));
        // a finished / idle pane must NOT read as working
        assert!(!agent_actively_working(
            "❯ \n──────\nnew task? /clear to save 131.9k tokens"
        ));
        // claude's post-turn form "✻ Cooked for 30s" is done, NOT working
        assert!(!agent_actively_working(
            "✻ Cooked for 30s\n── recap: ...\n❯"
        ));
        // mentioning the words without the live spinner pairing is not "working"
        assert!(!agent_actively_working(
            "I was working on the parser earlier. Done now.\n❯"
        ));
    }

    #[test]
    fn claude_idle_ready_detection() {
        // end-of-turn idle prompt: composer rendered, no y-n markers
        assert!(claude_idle_ready(
            "❯ \n──────\n  -- INSERT -- ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents"
        ));
        // regression (iotex): a quiet pane with NO "/clear to save" hint must
        // still count as idle — the composer is the signal, not the hint.
        assert!(claude_idle_ready(
            "✻ Churned for 1m 23s\n※ recap: ...\n──── ai-wallet-mcp-app ──\n❯\u{a0}\n────\n  -- INSERT -- ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents\n  ⏺ main\n-purpose  Research FlyingFox                 43m 27s · ↓ 2.2k tokens"
        ));
        // a mid-task permission prompt: y-n marker present → NOT idle-ready
        assert!(!claude_idle_ready(
            "Do you want to proceed? [Yes/No]\n❯ \n-- INSERT --"
        ));
        // numbered trust/permission choice → NOT idle-ready
        assert!(!claude_idle_ready(
            "❯ 1. Yes, I trust this folder\n   2. No, exit\n-- INSERT --"
        ));
        // actively streaming → NOT idle-ready even though composer may linger
        assert!(!claude_idle_ready(
            "✽ Baking… (42s · ↓ 5k tokens)\n❯ \n-- INSERT --"
        ));
        // no composer at all (e.g. codex pane) → not idle-ready
        assert!(!claude_idle_ready("› some codex prompt\n  gpt-5.6 high"));
    }
}
