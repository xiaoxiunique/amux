//! An MCP relay: one Streamable-HTTP endpoint that fronts many stdio MCP
//! servers so every agent session connects to *one* URL instead of spawning
//! its own copy of every server.
//!
//! The failure this exists for: three agents (opencode, Claude Code, Codex)
//! each start their own subprocess for every configured MCP server, per
//! session. On this machine that was 269 processes and ~1.3GB for a handful of
//! servers. Here the servers run once, amux owns them, and a per-server switch
//! keeps the long tail off until it is wanted.
//!
//! Lazy on purpose. `tools/list` is answered from a cached catalog persisted to
//! disk, so advertising a server's tools never starts it; the process is
//! spawned only when a `tools/call` actually targets it, and killed again once
//! it has been idle. The catalog is refreshed by the one spawn that discovers
//! it, so a server that is enabled but never called costs nothing.
//!
//! Transport, both directions:
//! - Downstream (agents → amux): Streamable HTTP. A JSON-RPC request is a POST,
//!   answered with `application/json`. GET/DELETE get 405/204 — the spec lets a
//!   server that offers no server→client SSE stream say exactly that.
//! - Upstream (amux → servers): stdio, newline-delimited JSON-RPC.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// The MCP revision amux speaks. Echoed back to the client on `initialize`.
pub(crate) const PROTOCOL_VERSION: &str = "2025-06-18";

/// A tool name longer than this is rejected by some clients (OpenAI's tool
/// schema caps names at 64 chars), so the server prefix is trimmed to fit.
const MAX_TOOL_NAME: usize = 64;

/// How long an upstream may sit unused before amux kills it. The point of the
/// relay is that idle servers do not run.
const IDLE_EVICT: Duration = Duration::from_secs(300);

/// The separator between a server name and one of its tool names.
///
/// A server name must not contain it; tool names may, so routing splits on the
/// first one.
const NS: &str = "__";

fn config_path() -> Option<PathBuf> {
    crate::config::config_dir().map(|dir| dir.join("mcp.json"))
}

fn cache_path() -> Option<PathBuf> {
    crate::config::config_dir().map(|dir| dir.join("mcp-tools.json"))
}

/// One upstream stdio server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Whether the relay advertises this server's tools at all. Off by default:
    /// the whole point is that the long tail does not run until asked for.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Config {
    #[serde(default)]
    pub servers: BTreeMap<String, ServerConfig>,
}

impl Config {
    fn load() -> Config {
        let Some(path) = config_path() else {
            return Config::default();
        };
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn save(&self) -> Result<()> {
        let Some(path) = config_path() else {
            anyhow::bail!("cannot determine the config directory");
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

/// A live upstream: the child, its stdio, and what it last told us it offers.
struct Upstream {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    tools: Vec<Value>,
    last_used: Instant,
}

impl Upstream {
    fn spawn(cfg: &ServerConfig) -> Result<Upstream> {
        let mut cmd = Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .envs(&cfg.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning MCP server {}", cfg.command))?;
        let stdin = child.stdin.take().context("MCP server stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("MCP server stdout")?);
        let mut up = Upstream {
            child,
            stdin,
            stdout,
            next_id: 1,
            tools: Vec::new(),
            last_used: Instant::now(),
        };
        up.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "amux", "version": env!("CARGO_PKG_VERSION") },
            }),
        )?;
        up.notify("notifications/initialized", json!({}))?;
        let listed = up.request("tools/list", json!({}))?;
        up.tools = listed
            .get("tools")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(up)
    }

    fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn send(&mut self, message: &Value) -> Result<()> {
        let mut line = serde_json::to_string(message)?;
        line.push('\n');
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.flush()?;
        Ok(())
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    /// One request, waiting for its matching reply. Notifications and any
    /// unrelated replies (a server may emit progress) are skipped.
    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        let want = Value::from(id);
        loop {
            let mut line = String::new();
            if self.stdout.read_line(&mut line)? == 0 {
                anyhow::bail!("MCP server closed its output");
            }
            let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if value.get("id") != Some(&want) {
                continue;
            }
            if let Some(error) = value.get("error") {
                anyhow::bail!("MCP server error: {error}");
            }
            return Ok(value.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}

struct Supervisor {
    config: Config,
    /// Server name → tool schemas discovered so far. Persisted, so a server
    /// that is advertised but idle is never spawned just to answer `tools/list`.
    cache: BTreeMap<String, Vec<Value>>,
    up: BTreeMap<String, Upstream>,
}

impl Supervisor {
    fn load() -> Supervisor {
        let config = Config::load();
        let cache = cache_path()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Supervisor { config, cache, up: BTreeMap::new() }
    }

    fn save_cache(&self) {
        if let Some(path) = cache_path() {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&path, serde_json::to_string_pretty(&self.cache).unwrap_or_default());
        }
    }

    fn running(&mut self, name: &str) -> bool {
        self.up.get_mut(name).map(Upstream::running).unwrap_or(false)
    }

    fn ensure_upstream(&mut self, name: &str) -> Result<()> {
        if self.running(name) {
            return Ok(());
        }
        self.up.remove(name);
        let cfg = self
            .config
            .servers
            .get(name)
            .cloned()
            .with_context(|| format!("unknown MCP server {name}"))?;
        let up = Upstream::spawn(&cfg)?;
        self.up.insert(name.to_string(), up);
        Ok(())
    }

    /// Kill upstreams that have been idle past the grace period.
    fn evict_idle(&mut self) {
        let now = Instant::now();
        let stale: Vec<String> = self
            .up
            .iter()
            .filter(|(_, up)| now.duration_since(up.last_used) >= IDLE_EVICT)
            .map(|(name, _)| name.clone())
            .collect();
        for name in stale {
            if let Some(mut up) = self.up.remove(&name) {
                let _ = up.child.kill();
            }
        }
    }

    /// Discover a server's tools, preferring the cache so an idle server stays
    /// down. Returns the schemas to advertise.
    fn tools_for(&mut self, name: &str) -> Vec<Value> {
        if !self.cache.contains_key(name) {
            if self.ensure_upstream(name).is_ok() {
                let tools = self.up.get(name).map(|up| up.tools.clone()).unwrap_or_default();
                self.cache.insert(name.to_string(), tools);
                self.save_cache();
            }
        }
        self.cache.get(name).cloned().unwrap_or_default()
    }

    /// The aggregated, namespaced catalog of every *enabled* server.
    fn tools_json(&mut self) -> Vec<Value> {
        self.evict_idle();
        let enabled: Vec<String> = self
            .config
            .servers
            .iter()
            .filter(|(_, cfg)| cfg.enabled)
            .map(|(name, _)| name.clone())
            .collect();
        let mut out = Vec::new();
        for name in enabled {
            for tool in self.tools_for(&name) {
                let original = tool
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                if original.is_empty() {
                    continue;
                }
                let mut tool = tool;
                if let Some(object) = tool.as_object_mut() {
                    object.insert("name".into(), json!(namespaced(&name, &original)));
                }
                out.push(tool);
            }
        }
        out
    }

    fn call(&mut self, tool: &str, arguments: Value) -> Result<Value> {
        let (server, tool_name) = tool
            .split_once(NS)
            .with_context(|| format!("tool {tool} is not namespaced"))?;
        let enabled = self
            .config
            .servers
            .get(server)
            .map(|cfg| cfg.enabled)
            .unwrap_or(false);
        if !enabled {
            anyhow::bail!("MCP server {server} is disabled");
        }
        self.ensure_upstream(server)?;
        let up = self.up.get_mut(server).context("upstream vanished")?;
        up.last_used = Instant::now();
        up.request("tools/call", json!({ "name": tool_name, "arguments": arguments }))
    }

    fn set_enabled(&mut self, name: &str, enabled: bool) -> Result<()> {
        let Some(cfg) = self.config.servers.get_mut(name) else {
            anyhow::bail!("unknown MCP server {name}");
        };
        cfg.enabled = enabled;
        if !enabled {
            if let Some(mut up) = self.up.remove(name) {
                let _ = up.child.kill();
            }
        }
        self.config.save()
    }
}

/// `server__tool`, with the server prefix trimmed when the pair would exceed a
/// client's tool-name limit.
fn namespaced(server: &str, tool: &str) -> String {
    let full = format!("{server}{NS}{tool}");
    if full.len() <= MAX_TOOL_NAME {
        return full;
    }
    // Keep the tool name intact — it is what the caller typed — and shorten the
    // prefix instead.
    let budget = MAX_TOOL_NAME.saturating_sub(NS.len() + tool.len());
    let prefix: String = server.chars().take(budget).collect();
    format!("{prefix}{NS}{tool}")
}

static SUPERVISOR: LazyLock<Mutex<Supervisor>> = LazyLock::new(|| Mutex::new(Supervisor::load()));

fn supervisor() -> MutexGuard<'static, Supervisor> {
    SUPERVISOR.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Re-read `mcp.json` after an external edit.
pub(crate) fn reload() {
    let mut sup = supervisor();
    let enabled: BTreeMap<String, bool> = sup
        .config
        .servers
        .iter()
        .map(|(name, cfg)| (name.clone(), cfg.enabled))
        .collect();
    let fresh = Config::load();
    for (name, cfg) in &fresh.servers {
        // A server that just turned off should stop running now.
        if cfg.enabled == false && enabled.get(name) == Some(&true) {
            if let Some(mut up) = sup.up.remove(name) {
                let _ = up.child.kill();
            }
        }
    }
    sup.config = fresh;
}

/// The configured servers for the API, without secrets beyond their command.
pub(crate) fn servers_json() -> Value {
    let mut sup = supervisor();
    // Snapshot the config first: `running` needs `&mut`, and the two borrows
    // cannot overlap.
    let rows: Vec<(String, String, Vec<String>, bool, Option<String>)> = sup
        .config
        .servers
        .iter()
        .map(|(name, cfg)| {
            (
                name.clone(),
                cfg.command.clone(),
                cfg.args.clone(),
                cfg.enabled,
                cfg.description.clone(),
            )
        })
        .collect();
    let items: Vec<Value> = rows
        .into_iter()
        .map(|(name, command, args, enabled, description)| {
            let running = sup.running(&name);
            json!({
                "name": name,
                "command": command,
                "args": args,
                "enabled": enabled,
                "description": description,
                "running": running,
            })
        })
        .collect();
    json!(items)
}

/// Flip one server on or off and persist it.
pub(crate) fn set_enabled(name: &str, enabled: bool) -> Result<()> {
    supervisor().set_enabled(name, enabled)
}

/// Handle one downstream JSON-RPC message. `None` for a notification (no reply
/// is sent); `Some` for a request, whose reply the HTTP layer serializes.
pub(crate) fn handle(message: Value) -> Option<Value> {
    let method = message.get("method").and_then(|m| m.as_str()).unwrap_or("");
    if method.starts_with("notifications/") {
        return None;
    }
    let id = message.get("id").cloned().unwrap_or(Value::Null);

    let outcome: Result<Value, (i64, String)> = match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "amux", "version": env!("CARGO_PKG_VERSION") },
        })),
        "ping" => Ok(json!({})),
        "tools/list" => {
            let tools = supervisor().tools_json();
            Ok(json!({ "tools": tools }))
        }
        "tools/call" => {
            let params = message.get("params").cloned().unwrap_or(json!({}));
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
            match supervisor().call(name, arguments) {
                Ok(result) => Ok(result),
                // A tool that failed is still a successful call: report it as
                // content so the model sees the error and can react, rather
                // than as a protocol error it cannot.
                Err(error) => Ok(json!({
                    "content": [{ "type": "text", "text": error.to_string() }],
                    "isError": true,
                })),
            }
        }
        other => Err((-32601, format!("method not found: {other}"))),
    };

    Some(match outcome {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespacing_keeps_the_tool_name() {
        assert_eq!(namespaced("chrome-devtools", "navigate_page"), "chrome-devtools__navigate_page");
        // An over-long pair drops prefix characters, never tool characters.
        let long_server = "s".repeat(80);
        let ns = namespaced(&long_server, "a_tool");
        assert!(ns.len() <= MAX_TOOL_NAME);
        assert!(ns.ends_with("__a_tool"));
        assert_eq!(ns.split_once(NS).unwrap().1, "a_tool");
    }

    #[test]
    fn initialize_answers_with_server_info() {
        let reply = handle(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }))
            .expect("initialize answers");
        assert_eq!(reply["result"]["serverInfo"]["name"], "amux");
        assert_eq!(reply["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(reply["id"], 1);
    }

    #[test]
    fn notifications_get_no_reply() {
        assert!(handle(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).is_none());
    }

    #[test]
    fn unknown_methods_are_a_jsonrpc_error() {
        let reply = handle(json!({ "jsonrpc": "2.0", "id": 7, "method": "resources/list" }))
            .expect("a request answers");
        assert_eq!(reply["error"]["code"], -32601);
        assert_eq!(reply["id"], 7);
    }

    /// Lazy is the whole point: calling a disabled or unknown server must not
    /// spawn it.
    #[test]
    fn calling_a_disabled_server_does_not_spawn_it() {
        let reply = handle(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "ghost__do", "arguments": {} },
        }))
        .expect("answers");
        assert_eq!(reply["result"]["isError"], true);
        assert!(supervisor().up.is_empty(), "a disabled server was spawned");
    }
}
