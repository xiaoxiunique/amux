use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use std::io::Write;
use std::path::PathBuf;

/// Resolved provider settings ready to inject into an agent command.
pub struct ProviderSettings {
    /// Extra args to append to the agent command.
    pub extra_argv: Vec<String>,
    /// Environment variables to set when launching the agent.
    pub env_vars: Vec<(String, String)>,
}

/// DB path for CC Switch provider configs.
fn db_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".cc-switch").join("cc-switch.db"))
}

/// Open the CC Switch DB read-only. Returns None if DB doesn't exist.
fn open_db() -> Option<Connection> {
    let p = db_path()?;
    if !p.exists() {
        return None;
    }
    Connection::open_with_flags(&p, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
}

/// True when the CC Switch database exists on this machine, i.e. `--provider`
/// and the cc-switch endpoints have something to work with. Lets the server
/// advertise the feature instead of the client discovering it by failing.
pub fn is_installed() -> bool {
    db_path().is_some_and(|p| p.exists())
}

/// Resolve a provider name/id to a DB provider ID for the given app_type.
fn resolve_provider_id(name: &str, app_type: &str, conn: &Connection) -> Result<String> {
    // Try exact ID match
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM providers WHERE app_type=?1 AND id=?2",
            [app_type, name],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if count > 0 {
        return Ok(name.to_string());
    }

    // Try name match (case-insensitive)
    let id: Option<String> = conn
        .query_row(
            "SELECT id FROM providers WHERE app_type=?1 AND LOWER(name)=LOWER(?2) LIMIT 1",
            [app_type, name],
            |r| r.get(0),
        )
        .ok();
    if let Some(id) = id {
        return Ok(id);
    }

    // List available providers for a helpful error message
    let mut stmt = conn
        .prepare("SELECT name FROM providers WHERE app_type=?1 ORDER BY name")
        .ok();
    let names: Vec<String> = stmt
        .as_mut()
        .and_then(|s| {
            s.query_map([app_type], |r| r.get(0))
                .ok()
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();

    if names.is_empty() {
        bail!("unknown provider: {name} (no {app_type} providers found in CC Switch DB)");
    } else {
        bail!(
            "unknown provider: {name}\navailable {app_type} providers: {}",
            names.join(", ")
        );
    }
}

/// Check whether `name` is a known provider for the given app_type.
/// Returns false silently if DB doesn't exist.
pub fn is_known_provider(name: &str, app_type: &str) -> bool {
    let Some(conn) = open_db() else {
        return false;
    };

    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM providers WHERE app_type=?1 AND (LOWER(name)=LOWER(?2) OR id=?2)",
            [app_type, name],
            |r| r.get(0),
        )
        .unwrap_or(0);
    count > 0
}

/// One provider a session could be launched against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderChoice {
    pub name: String,
    /// The one CC Switch has active, i.e. what a session gets by default.
    pub is_current: bool,
}

/// Providers configured for `app_type`, current one first.
///
/// Empty when CC Switch is not installed, so a caller can simply skip offering
/// the choice rather than having to ask whether it exists.
pub fn list(app_type: &str) -> Vec<ProviderChoice> {
    let Some(conn) = open_db() else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT name, is_current FROM providers WHERE app_type=?1 ORDER BY is_current DESC, name",
    ) else {
        return Vec::new();
    };
    let rows = stmt.query_map([app_type], |r| {
        Ok(ProviderChoice {
            name: r.get(0)?,
            is_current: r.get::<_, i64>(1).unwrap_or(0) != 0,
        })
    });
    rows.map(|rs| rs.flatten().collect()).unwrap_or_default()
}

/// Whether picking a provider means anything for this agent.
///
/// [`resolve_settings`] only knows how to configure Claude and Codex; every
/// other agent falls through to the Claude branch and would be handed a
/// `--settings` it does not understand. The database does carry `pi` providers,
/// but nothing here can apply them yet.
pub fn selectable(agent_name: &str) -> bool {
    matches!(agent_name, "claude" | "cc" | "codex" | "cx")
}

/// Map an agent name to the CC Switch app_type.
pub fn agent_app_type(agent_name: &str) -> &'static str {
    match agent_name {
        "codex" | "cx" => "codex",
        _ => "claude",
    }
}

/// Whether a bare first argument may be silently reinterpreted as a provider
/// for this agent.
///
/// Only Claude and Codex have a settings/profile mechanism to inject into.
/// Handing any other agent a `--settings <path>` it does not understand would
/// break the launch, and the collision is realistic: `oc ds` reads as "opencode
/// in the ds provider" but means "opencode, with `ds` as an argument". An
/// explicit `--provider` is still honoured — that is the user asking for it.
pub fn infers_provider_from_first_arg(agent_name: &str) -> bool {
    matches!(agent_name, "claude" | "cc" | "codex" | "cx")
}

/// Resolve provider and prepare injection settings.
pub fn resolve_settings(name: &str, app_type: &str) -> Result<ProviderSettings> {
    let Some(p) = db_path() else {
        bail!("cannot determine home directory");
    };
    if !p.exists() {
        bail!(
            "CC Switch is not installed (DB not found: {})\n\
             Install CC Switch from https://github.com/nicepkg/cc-switch to use --provider",
            p.display()
        );
    }

    let conn = Connection::open_with_flags(&p, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {}", p.display()))?;

    let id = resolve_provider_id(name, app_type, &conn)?;

    let settings_config: String = conn
        .query_row(
            "SELECT settings_config FROM providers WHERE app_type=?1 AND id=?2",
            [app_type, &id],
            |r| r.get(0),
        )
        .with_context(|| format!("reading settings for provider {id}"))?;

    let provider_name: String = conn
        .query_row(
            "SELECT name FROM providers WHERE app_type=?1 AND id=?2",
            [app_type, &id],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| name.to_string());

    if settings_config.is_empty() {
        bail!("provider {provider_name} has empty settings_config");
    }

    match app_type {
        "codex" => resolve_codex_settings(&provider_name, &settings_config),
        _ => resolve_claude_settings(&settings_config),
    }
}

/// Claude: settings_config is a JSON blob → write to temp file → --settings <path>
fn resolve_claude_settings(settings_config: &str) -> Result<ProviderSettings> {
    let blob = neutralize_conflicting_auth_token(settings_config)?;

    let mut tmp = tempfile::Builder::new()
        .prefix("amux-provider-")
        .suffix(".json")
        .tempfile()
        .context("creating temp settings file")?;
    tmp.write_all(blob.as_bytes())
        .context("writing settings")?;
    let path = tmp.into_temp_path().keep().context("persisting temp file")?;

    Ok(ProviderSettings {
        extra_argv: vec!["--settings".into(), path.to_string_lossy().into_owned()],
        env_vars: vec![],
    })
}

/// The CC Switch proxy exports BOTH `ANTHROPIC_AUTH_TOKEN` and `ANTHROPIC_API_KEY`
/// into the shell (as `PROXY_MANAGED` stubs). A provider's settings_config sets
/// only the ONE credential it uses, so when Claude Code loads `--settings` the
/// other stub survives — and Claude refuses to choose reliably when both are set
/// ("Both ANTHROPIC_AUTH_TOKEN and ANTHROPIC_API_KEY set · auth may not work").
///
/// Rewrite the settings `env` so the unused credential is blanked to an empty
/// string (which the settings layer applies over the inherited shell env),
/// leaving exactly one non-empty credential. No-op when the blob isn't JSON or
/// the provider already sets both / neither.
fn neutralize_conflicting_auth_token(settings_config: &str) -> Result<String> {
    // Non-JSON blobs (shouldn't happen with cc-switch, but stay safe) pass through.
    let Ok(mut v): std::result::Result<serde_json::Value, _> =
        serde_json::from_str(settings_config)
    else {
        return Ok(settings_config.to_string());
    };
    let Some(env) = v.get_mut("env").and_then(|e| e.as_object_mut()) else {
        return Ok(settings_config.to_string());
    };
    let has_token = env.contains_key("ANTHROPIC_AUTH_TOKEN");
    let has_key = env.contains_key("ANTHROPIC_API_KEY");
    if has_token && !has_key {
        env.insert("ANTHROPIC_API_KEY".to_string(), serde_json::Value::String(String::new()));
    } else if has_key && !has_token {
        env.insert(
            "ANTHROPIC_AUTH_TOKEN".to_string(),
            serde_json::Value::String(String::new()),
        );
    }
    Ok(serde_json::to_string(&v)?)
}

/// Point a codex provider at the environment variable amux injects.
///
/// Codex ignores `env_key` when a provider sets `requires_openai_auth = true`
/// and authenticates from `~/.codex/auth.json` instead (verified against codex
/// 0.146.1). CC Switch writes every codex provider that way while handing the
/// key to amux for env injection, so left alone the provider's own key never
/// reaches Codex and whatever sits in `auth.json` — from some other provider —
/// is used instead, a 401 whenever they differ. Rewrite the selected provider
/// to read the env var. A provider carrying its own credential
/// (`experimental_bearer_token`, e.g. CC Switch's proxy) already authenticates
/// itself and is left untouched, as is one not using OpenAI auth. A config that
/// will not parse passes through unchanged rather than being dropped.
fn codex_config_for_env_auth(config_toml: &str, env_vars: &[(String, String)]) -> String {
    if env_vars.is_empty() {
        return config_toml.to_string();
    }
    let Ok(mut root) = config_toml.parse::<toml::Value>() else {
        return config_toml.to_string();
    };
    let Some(id) = root
        .get("model_provider")
        .and_then(|v| v.as_str())
        .map(str::to_string)
    else {
        return config_toml.to_string();
    };
    let Some(provider) = root
        .get_mut("model_providers")
        .and_then(|v| v.as_table_mut())
        .and_then(|providers| providers.get_mut(&id))
        .and_then(|v| v.as_table_mut())
    else {
        return config_toml.to_string();
    };
    let uses_openai_auth = provider
        .get("requires_openai_auth")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !uses_openai_auth || provider.contains_key("experimental_bearer_token") {
        return config_toml.to_string();
    }
    let env_name = provider
        .get("env_key")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            env_vars
                .iter()
                .map(|(k, _)| k.clone())
                .find(|k| k.as_str() == "OPENAI_API_KEY")
        })
        .or_else(|| env_vars.first().map(|(k, _)| k.clone()));
    let Some(env_name) = env_name else {
        return config_toml.to_string();
    };
    provider.insert("requires_openai_auth".into(), toml::Value::Boolean(false));
    provider.insert("env_key".into(), toml::Value::String(env_name));
    toml::to_string(&root).unwrap_or_else(|_| config_toml.to_string())
}

/// Codex: settings_config is JSON with {auth, config} →
///   write config TOML to ~/.codex/amux-<name>.config.toml → -p amux-<name>
///   extract auth env vars for injection
fn resolve_codex_settings(provider_name: &str, settings_config: &str) -> Result<ProviderSettings> {
    // Parse the JSON: { "auth": {"KEY": "val"}, "config": "toml string" }
    let v: serde_json::Value =
        serde_json::from_str(settings_config).context("parsing codex settings_config JSON")?;

    let config_toml = v["config"].as_str().unwrap_or("");
    if config_toml.is_empty() {
        bail!("codex provider {provider_name} has empty config");
    }

    // Extract auth env vars. These are what amux exports into the launch, so
    // they also decide whether the profile has to be rewritten to read them.
    let mut env_vars = Vec::new();
    if let Some(auth) = v["auth"].as_object() {
        for (k, v) in auth {
            if let Some(val) = v.as_str() {
                if !val.is_empty() {
                    env_vars.push((k.clone(), val.to_string()));
                }
            }
        }
    }
    let config_toml = codex_config_for_env_auth(config_toml, &env_vars);

    let codex_home = dirs::home_dir()
        .context("cannot determine home directory")?
        .join(".codex");

    // A Codex `-p` profile is a whole config, not a layer on top of
    // `config.toml`, so whatever MCP servers the main config carries — the amux
    // relay, say — are otherwise lost. Worse, CC Switch stores the user's whole
    // config in the provider, so the profile's own stale `[mcp_servers]` would
    // win and spawn one local server per session for every entry. Keep the
    // provider's model settings; take the MCP servers from the real config.
    let main_config = std::fs::read_to_string(codex_home.join("config.toml")).unwrap_or_default();
    let config_toml = codex_profile_config(&config_toml, &main_config);

    // Write profile to ~/.codex/amux-<slug>.config.toml
    let slug: String = provider_name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect();
    let profile_name = format!("amux-{slug}");

    let profile_path = codex_home.join(format!("{profile_name}.config.toml"));

    std::fs::write(&profile_path, &config_toml)
        .with_context(|| format!("writing {}", profile_path.display()))?;

    Ok(ProviderSettings {
        extra_argv: vec!["-p".into(), profile_name],
        env_vars,
    })
}

/// Build a Codex profile body: the provider's own settings without its copy of
/// the MCP servers, plus the servers the user actually configured.
fn codex_profile_config(provider_config: &str, main_config: &str) -> String {
    let Ok(mut provider) = provider_config.parse::<toml::Value>() else {
        return provider_config.to_string();
    };
    let Some(table) = provider.as_table_mut() else {
        return provider_config.to_string();
    };
    table.remove("mcp_servers");
    if let Ok(main) = main_config.parse::<toml::Value>() {
        if let Some(mcp) = main.get("mcp_servers") {
            table.insert("mcp_servers".into(), mcp.clone());
        }
    }
    toml::to_string(&provider).unwrap_or_else(|_| provider_config.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_db_means_not_known() {
        assert!(!is_known_provider("anything", "claude"));
        assert!(!is_known_provider("anything", "codex"));
    }

    #[test]
    fn agent_app_type_mapping() {
        assert_eq!(agent_app_type("claude"), "claude");
        assert_eq!(agent_app_type("cc"), "claude");
        assert_eq!(agent_app_type("codex"), "codex");
        assert_eq!(agent_app_type("cx"), "codex");
        assert_eq!(agent_app_type("gemini"), "claude"); // fallback
    }

    #[test]
    fn only_claude_and_codex_infer_a_provider_from_the_first_arg() {
        assert!(infers_provider_from_first_arg("claude"));
        assert!(infers_provider_from_first_arg("cc"));
        assert!(infers_provider_from_first_arg("codex"));
        assert!(infers_provider_from_first_arg("cx"));
        // opencode takes no --settings/-p, so `oc ds` must pass `ds` through
        // as an ordinary argument rather than resolving it as a provider.
        assert!(!infers_provider_from_first_arg("opencode"));
        assert!(!infers_provider_from_first_arg("pi"));
        assert!(!infers_provider_from_first_arg("gemini"));
    }

    #[test]
    fn open_db_does_not_panic() {
        let _ = open_db();
    }

    #[test]
    fn neutralizer_blanks_unused_credential() {
        // glm-style: only AUTH_TOKEN set → API_KEY must be blanked
        let glm = r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"tok","ANTHROPIC_BASE_URL":"u"}}"#;
        let out = neutralize_conflicting_auth_token(glm).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let env = v["env"].as_object().unwrap();
        assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "tok"); // preserved
        assert_eq!(env["ANTHROPIC_API_KEY"], ""); // blanked

        // deepseek-style: only API_KEY set → AUTH_TOKEN must be blanked
        let ds = r#"{"env":{"ANTHROPIC_API_KEY":"k","ANTHROPIC_MODEL":"m"}}"#;
        let out = neutralize_conflicting_auth_token(ds).unwrap();
        let env = serde_json::from_str::<serde_json::Value>(&out).unwrap()["env"]
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(env["ANTHROPIC_API_KEY"], "k"); // preserved
        assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], ""); // blanked

        // both set → untouched (no blanking)
        let both = r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"t","ANTHROPIC_API_KEY":"k"}}"#;
        let out = neutralize_conflicting_auth_token(both).unwrap();
        let env = serde_json::from_str::<serde_json::Value>(&out).unwrap()["env"]
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "t");
        assert_eq!(env["ANTHROPIC_API_KEY"], "k");

        // non-JSON → passed through unchanged
        assert_eq!(neutralize_conflicting_auth_token("garbage").unwrap(), "garbage");
    }

    /// CC Switch writes every codex provider as `requires_openai_auth = true`,
    /// which makes Codex read `~/.codex/auth.json` and ignore the env var amux
    /// injects — the wrong key whenever another provider sits in that file. The
    /// selected provider is rewritten to read the env var, and the rest of the
    /// config survives intact.
    #[test]
    fn a_codex_provider_using_openai_auth_is_pointed_at_the_env_var() {
        let cfg = r#"model_provider = "OpenAI"
model = "gpt-5.5"
model_instructions_file = "./prompts/instruction2.md"

[model_providers.OpenAI]
name = "OpenAI"
base_url = "https://sub.iotex.me"
wire_api = "responses"
requires_openai_auth = true

[mcp_servers.chrome-devtools]
command = "npx"
args = ["chrome-devtools-mcp@latest"]
"#;
        let out = codex_config_for_env_auth(cfg, &[("OPENAI_API_KEY".into(), "sk-x".into())]);
        let v: toml::Value = out.parse().expect("the rewrite must still be valid TOML");
        assert_eq!(v["model_provider"].as_str(), Some("OpenAI"));
        assert_eq!(v["model"].as_str(), Some("gpt-5.5"));
        assert_eq!(
            v["model_instructions_file"].as_str(),
            Some("./prompts/instruction2.md")
        );
        let provider = &v["model_providers"]["OpenAI"];
        assert_eq!(provider["requires_openai_auth"].as_bool(), Some(false));
        assert_eq!(provider["env_key"].as_str(), Some("OPENAI_API_KEY"));
        assert_eq!(provider["base_url"].as_str(), Some("https://sub.iotex.me"));
        assert_eq!(provider["name"].as_str(), Some("OpenAI"));
        assert_eq!(
            v["mcp_servers"]["chrome-devtools"]["command"].as_str(),
            Some("npx")
        );
    }

    /// A profile must not resurrect the provider's stale MCP servers, and must
    /// keep the ones the user actually configured — otherwise naming a provider
    /// silently drags every local MCP server back per session.
    #[test]
    fn a_codex_profile_takes_mcp_servers_from_the_main_config() {
        let provider = r#"model = "gpt-5.5"

[mcp_servers.chrome-devtools]
command = "npx"
args = ["chrome-devtools-mcp@latest"]
"#;
        let main = r#"[mcp_servers.amux]
url = "http://127.0.0.1:8787/mcp"
"#;
        let v: toml::Value = codex_profile_config(provider, main).parse().expect("valid TOML");
        assert_eq!(v["model"].as_str(), Some("gpt-5.5"));
        assert!(
            v["mcp_servers"].get("chrome-devtools").is_none(),
            "the provider's stale MCP server leaked into the profile"
        );
        assert_eq!(
            v["mcp_servers"]["amux"]["url"].as_str(),
            Some("http://127.0.0.1:8787/mcp")
        );
    }

    #[test]
    fn a_codex_profile_without_a_main_config_has_no_mcp_servers() {
        let provider = "[mcp_servers.x]\ncommand = \"npx\"\n";
        let v: toml::Value = codex_profile_config(provider, "").parse().unwrap();
        assert!(v.get("mcp_servers").is_none());
    }

    /// A provider carrying its own bearer token already authenticates itself;
    /// flipping it to an env var would drop that credential.
    #[test]
    fn a_codex_provider_with_a_bearer_token_is_left_alone() {
        let cfg = r#"model_provider = "crs"

[model_providers.crs]
base_url = "http://127.0.0.1:15721/v1"
requires_openai_auth = true
experimental_bearer_token = "PROXY_MANAGED"
"#;
        assert_eq!(
            codex_config_for_env_auth(cfg, &[("OPENAI_API_KEY".into(), "sk-x".into())]),
            cfg
        );
    }

    /// Nothing to authenticate with, or a provider not using OpenAI auth, is a
    /// no-op rather than a rewrite that would change the wrong thing.
    #[test]
    fn codex_rewrite_is_a_no_op_when_it_would_not_help() {
        let no_openai_auth = r#"model_provider = "p"

[model_providers.p]
base_url = "http://localhost:11434/v1"
requires_openai_auth = false
"#;
        assert_eq!(
            codex_config_for_env_auth(no_openai_auth, &[("OPENAI_API_KEY".into(), "sk-x".into())]),
            no_openai_auth
        );

        let no_auth_key = r#"model_provider = "p"

[model_providers.p]
requires_openai_auth = true
"#;
        assert_eq!(codex_config_for_env_auth(no_auth_key, &[]), no_auth_key);
    }
}
