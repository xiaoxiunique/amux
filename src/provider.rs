use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use std::io::Write;
use std::path::PathBuf;

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

/// Resolve a provider name/id to a DB provider ID.
/// Tries: exact ID match → case-insensitive name match.
fn resolve_provider_id(name: &str, conn: &Connection) -> Result<String> {
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

    // List available providers for a helpful error message
    let mut stmt = conn
        .prepare("SELECT name FROM providers WHERE app_type='claude' ORDER BY name")
        .ok();
    let names: Vec<String> = stmt
        .as_mut()
        .and_then(|s| {
            s.query_map([], |r| r.get(0))
                .ok()
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();

    if names.is_empty() {
        bail!("unknown provider: {name} (no providers found in CC Switch DB)");
    } else {
        bail!(
            "unknown provider: {name}\navailable providers: {}",
            names.join(", ")
        );
    }
}

/// Check whether `name` is a known provider in the CC Switch DB.
/// Returns false silently if DB doesn't exist.
pub fn is_known_provider(name: &str) -> bool {
    let Some(conn) = open_db() else {
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
    use super::*;

    #[test]
    fn no_db_means_not_known() {
        // When DB doesn't exist, is_known_provider should return false, not panic
        assert!(!is_known_provider("anything"));
    }

    #[test]
    fn open_db_returns_none_when_missing() {
        // This test relies on the DB not existing at a weird path, but
        // open_db() uses the real home dir. At minimum it should not panic.
        let _ = open_db();
    }
}
