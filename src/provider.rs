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

/// Map an agent name to the CC Switch app_type.
pub fn agent_app_type(agent_name: &str) -> &'static str {
    match agent_name {
        "codex" | "cx" => "codex",
        _ => "claude",
    }
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

    eprintln!("Using provider: {provider_name}");

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

    // Write profile to ~/.codex/amux-<slug>.config.toml
    let slug: String = provider_name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect();
    let profile_name = format!("amux-{slug}");

    let codex_home = dirs::home_dir()
        .context("cannot determine home directory")?
        .join(".codex");
    let profile_path = codex_home.join(format!("{profile_name}.config.toml"));

    std::fs::write(&profile_path, config_toml)
        .with_context(|| format!("writing {}", profile_path.display()))?;

    // Extract auth env vars
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

    Ok(ProviderSettings {
        extra_argv: vec!["-p".into(), profile_name],
        env_vars,
    })
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
}
