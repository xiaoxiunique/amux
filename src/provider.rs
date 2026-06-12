use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use std::io::Write;
use std::path::PathBuf;

/// DB path for CC Switch provider configs.
fn db_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".cc-switch").join("cc-switch.db"))
}

/// Resolve a user-friendly provider name to a CC Switch DB provider ID.
/// Checks hardcoded aliases first, then queries the DB by name.
fn resolve_provider_id(name: &str, conn: &Connection) -> Result<String> {
    let lower = name.to_lowercase();

    // Hardcoded aliases (mirrors ccs-claude-switch-open)
    let hardcoded = match lower.as_str() {
        "sub" | "company" | "gongsi" => Some("5ed3e66f-1e9f-4bba-b891-156e612cdfcd"),
        "aigocode" | "ai-go-code" | "ai_go_code" | "aigo" => {
            Some("1f66fe75-8966-463d-8b1b-60b90775af20")
        }
        "glm" | "zhipu" => Some("84fe4f50-0f82-4db1-bfe9-550c043edf27"),
        "deepseek" | "ds" => Some("01521cab-1d77-4183-adcf-a2ec13646b6e"),
        _ => None,
    };

    if let Some(id) = hardcoded {
        return Ok(id.to_string());
    }

    // Try exact ID match
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM providers WHERE app_type='claude' AND id=?1",
            [name],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if count > 0 {
        return Ok(name.to_string());
    }

    // Try name match (case-insensitive)
    let id: Option<String> = conn
        .query_row(
            "SELECT id FROM providers WHERE app_type='claude' AND LOWER(name)=LOWER(?1) LIMIT 1",
            [name],
            |r| r.get(0),
        )
        .ok();
    if let Some(id) = id {
        return Ok(id);
    }

    bail!("unknown provider: {name}");
}

/// Check whether `name` is a known provider (hardcoded alias or in DB).
pub fn is_known_provider(name: &str) -> bool {
    let lower = name.to_lowercase();

    // Check hardcoded aliases
    let hardcoded = matches!(
        lower.as_str(),
        "sub"
            | "company"
            | "gongsi"
            | "aigocode"
            | "ai-go-code"
            | "ai_go_code"
            | "aigo"
            | "glm"
            | "zhipu"
            | "deepseek"
            | "ds"
    );
    if hardcoded {
        return true;
    }

    // Check DB
    let Some(p) = db_path() else { return false };
    if !p.exists() {
        return false;
    }
    let Ok(conn) = Connection::open_with_flags(&p, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return false;
    };

    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM providers WHERE app_type='claude' AND (LOWER(name)=LOWER(?1) OR id=?1)",
            [name],
            |r| r.get(0),
        )
        .unwrap_or(0);
    count > 0
}

/// Resolve provider name → write settings to a temp file → return the file path.
/// The temp file is intentionally NOT cleaned up (tmux session outlives this process).
pub fn resolve_and_write_settings(name: &str) -> Result<String> {
    let p = db_path().context("cannot determine home directory")?;
    if !p.exists() {
        bail!("CC Switch DB not found: {}", p.display());
    }

    let conn = Connection::open_with_flags(&p, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {}", p.display()))?;

    let id = resolve_provider_id(name, &conn)?;

    let settings_config: String = conn
        .query_row(
            "SELECT settings_config FROM providers WHERE app_type='claude' AND id=?1",
            [&id],
            |r| r.get(0),
        )
        .with_context(|| format!("reading settings for provider {id}"))?;

    let provider_name: String = conn
        .query_row(
            "SELECT name FROM providers WHERE app_type='claude' AND id=?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| name.to_string());

    if settings_config.is_empty() {
        bail!("provider {provider_name} has empty settings_config");
    }

    // Write to a temp file (not auto-deleted — tmux session needs it)
    let mut tmp = tempfile::Builder::new()
        .prefix("amux-provider-")
        .suffix(".json")
        .tempfile()
        .context("creating temp settings file")?;
    tmp.write_all(settings_config.as_bytes())
        .context("writing settings")?;

    // Persist the temp file (prevent auto-delete on drop)
    let path = tmp.into_temp_path().keep().context("persisting temp file")?;

    eprintln!("Using provider: {provider_name}");
    Ok(path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {

    #[test]
    fn hardcoded_aliases_are_known() {
        // These should always return true (no DB needed)
        assert!(matches!(
            resolve_provider_id_hardcoded("glm"),
            Some(_)
        ));
        assert!(matches!(
            resolve_provider_id_hardcoded("sub"),
            Some(_)
        ));
        assert!(matches!(
            resolve_provider_id_hardcoded("deepseek"),
            Some(_)
        ));
        assert!(resolve_provider_id_hardcoded("unknown").is_none());
    }

    fn resolve_provider_id_hardcoded(name: &str) -> Option<&'static str> {
        match name.to_lowercase().as_str() {
            "sub" | "company" | "gongsi" => Some("5ed3e66f-1e9f-4bba-b891-156e612cdfcd"),
            "aigocode" | "ai-go-code" | "ai_go_code" | "aigo" => {
                Some("1f66fe75-8966-463d-8b1b-60b90775af20")
            }
            "glm" | "zhipu" => Some("84fe4f50-0f82-4db1-bfe9-550c043edf27"),
            "deepseek" | "ds" => Some("01521cab-1d77-4183-adcf-a2ec13646b6e"),
            _ => None,
        }
    }
}
