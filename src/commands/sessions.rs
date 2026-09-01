use crate::config::Agent;
use crate::tmux;
use anyhow::{Context, Result};
use std::path::PathBuf;

/// A parsed amux-managed session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedSession {
    pub name: String,
    pub alias: String,
}

/// Return managed sessions: those whose name begins with `<alias>_` or
/// `<alias>-<provider>_` for a known alias and match the
/// `[<alias>-<provider>_]<slug>_<8hex>` shape.
pub fn managed_sessions(all: &[String], agents: &[Agent]) -> Vec<ManagedSession> {
    let mut out = Vec::new();
    for name in all {
        for a in agents {
            let Some(after_alias) = name.strip_prefix(&a.alias) else {
                continue;
            };
            // Determine the "slug_hash" portion after alias[_|-provider_]
            let slug_hash = if let Some(r) = after_alias.strip_prefix('_') {
                // Pattern: <alias>_<slug>_<hash>
                r
            } else if let Some(r) = after_alias.strip_prefix('-') {
                // Pattern: <alias>-<provider>_<slug>_<hash>
                // Find the first '_' after the provider name
                let Some(underscore) = r.find('_') else { continue };
                if underscore == 0 { continue; }
                &r[underscore + 1..]
            } else {
                continue;
            };
            // slug_hash must be `<slug>_<8hex>`, optionally followed by the
            // `-<suffix>` an `amux new` session carries.
            if let Some(idx) = slug_hash.rfind('_') {
                let tail = &slug_hash[idx + 1..];
                let hash = tail.split('-').next().unwrap_or(tail);
                if hash.len() == 8 && hash.chars().all(|c| c.is_ascii_hexdigit()) && idx > 0 {
                    out.push(ManagedSession { name: name.clone(), alias: a.alias.clone() });
                    break;
                }
            }
        }
    }
    out
}

pub fn list(agents: &[Agent]) -> Result<()> {
    let all = tmux::list_session_names()?;
    let managed = managed_sessions(&all, agents);
    if managed.is_empty() {
        println!("No amux sessions.");
        return Ok(());
    }
    println!("{:<28} {}", "SESSION", "AGENT");
    for s in managed {
        println!("{:<28} {}", s.name, s.alias);
    }
    Ok(())
}

pub fn kill(name: &str) -> Result<()> {
    tmux::kill_session(name)?;
    println!("killed {name}");
    Ok(())
}

/// Extract the directory slug from a session name.
/// E.g. `cc_myproject_1a2b3c4d` → "myproject", `cc-openai_my_proj_deadbeef` → "my_proj".
fn session_slug(name: &str) -> Option<&str> {
    // slug is everything between the first `_` (after alias[-provider]) and the last `_<hash>`.
    let first = name.find('_')?;
    let last = name.rfind('_')?;
    if last <= first + 1 {
        return None;
    }
    Some(&name[first + 1..last])
}

/// Fuzzy-match a managed session by its directory slug and attach to it.
/// Unique match → attach directly. Multiple → numbered picker.
pub fn goto(query: &str, agents: &[Agent]) -> Result<()> {
    let all = tmux::list_session_names()?;
    let managed = managed_sessions(&all, agents);

    let q = query.to_lowercase();
    let matches: Vec<&ManagedSession> = managed
        .iter()
        .filter(|s| {
            session_slug(&s.name)
                .map(|slug| slug.to_lowercase().contains(&q))
                .unwrap_or(false)
        })
        .collect();

    match matches.as_slice() {
        [] => {
            anyhow::bail!("no session matches '{query}'");
        }
        [only] => tmux::attach_or_switch(&only.name),
        many => {
            println!("Multiple sessions match '{query}':");
            for (i, s) in many.iter().enumerate() {
                println!("  {}) {}", i + 1, s.name);
            }
            print!("Select [1-{}]: ", many.len());
            use std::io::Write;
            std::io::stdout().flush().ok();

            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            let idx: usize = line.trim().parse().unwrap_or(0);
            if idx >= 1 && idx <= many.len() {
                tmux::attach_or_switch(&many[idx - 1].name)
            } else {
                anyhow::bail!("invalid selection");
            }
        }
    }
}

/// Parsed info from a session name: the agent alias and optional provider.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct SessionEntry {
    agent: String,
    directory: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
}

fn default_save_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".amux").join("sessions.json"))
}

/// Extract agent alias and optional provider from a session name.
/// E.g. `cc-openai_myproject_1a2b3c4d` → ("cc", Some("openai"))
fn parse_session_alias(name: &str) -> Option<(&str, Option<&str>)> {
    // Find the first '_' which separates the alias (or alias-provider) from the slug
    let underscore = name.find('_')?;
    let prefix = &name[..underscore];
    if let Some(dash) = prefix.find('-') {
        let alias = &prefix[..dash];
        let provider = &prefix[dash + 1..];
        if !alias.is_empty() && !provider.is_empty() {
            return Some((alias, Some(provider)));
        }
    }
    if !prefix.is_empty() {
        return Some((prefix, None));
    }
    None
}

/// Map an alias back to the agent name.
fn alias_to_name<'a>(agents: &'a [Agent], alias: &str) -> Option<&'a str> {
    agents.iter().find(|a| a.alias == alias).map(|a| a.name.as_str())
}

/// Save the current session list to the default path. Silent on success.
/// Called automatically after `amux run` to keep the snapshot fresh.
pub fn auto_save(agents: &[Agent]) {
    let _ = save_silent(None, agents);
}

fn save_silent(file: Option<&std::path::Path>, agents: &[Agent]) -> Result<usize> {
    let path = match file {
        Some(p) => p.to_path_buf(),
        None => default_save_path()
            .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?,
    };

    let all = tmux::list_session_names()?;
    let managed = managed_sessions(&all, agents);

    if managed.is_empty() {
        return Ok(0);
    }

    let mut entries = Vec::new();
    for s in &managed {
        let cwd = tmux::session_cwd(&s.name)?;
        let (alias, provider) = parse_session_alias(&s.name)
            .unwrap_or((&s.alias, None));
        let agent_name = alias_to_name(agents, alias).unwrap_or(alias);
        entries.push(SessionEntry {
            agent: agent_name.to_string(),
            directory: cwd,
            provider: provider.map(|p| p.to_string()),
        });
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(&entries)?;
    std::fs::write(&path, &json)
        .with_context(|| format!("writing {}", path.display()))?;

    Ok(entries.len())
}

pub fn save(file: Option<&std::path::Path>, agents: &[Agent]) -> Result<()> {
    let path = match file {
        Some(p) => p.to_path_buf(),
        None => default_save_path()
            .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?,
    };
    let count = save_silent(Some(&path), agents)?;
    if count == 0 {
        println!("No amux sessions to save.");
    } else {
        println!("Saved {} session(s) to {}", count, path.display());
    }
    Ok(())
}

pub fn restore(file: Option<&std::path::Path>, agents: &[Agent]) -> Result<()> {
    let path = match file {
        Some(p) => p.to_path_buf(),
        None => default_save_path()
            .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?,
    };

    let json = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let entries: Vec<SessionEntry> = serde_json::from_str(&json)
        .with_context(|| format!("parsing {}", path.display()))?;

    if entries.is_empty() {
        println!("No sessions in {}.", path.display());
        return Ok(());
    }

    let mut created = 0;
    let mut skipped = 0;
    let mut errors = 0;
    let mut attach_to: Option<String> = None;

    for entry in &entries {
        let agent = match agents.iter().find(|a| a.name == entry.agent) {
            Some(a) => a,
            None => {
                eprintln!("  skip: unknown agent '{}'", entry.agent);
                errors += 1;
                continue;
            }
        };

        let cwd = PathBuf::from(&entry.directory);
        if !cwd.exists() {
            eprintln!("  skip {}: directory not found: {}", entry.agent, entry.directory);
            errors += 1;
            continue;
        }

        // Build alias (same logic as run.rs)
        let alias = match &entry.provider {
            Some(p) => format!("{}-{}", agent.alias, p),
            None => agent.alias.clone(),
        };
        let name = crate::session::session_name(&alias, &cwd);

        if tmux::has_session(&name) {
            skipped += 1;
            continue;
        }

        // Same conversation resolution `amux run` does. Without it a restore
        // brings back the *shape* of the workspace — right agent, right
        // directory — but every session opens a blank conversation, which is
        // the opposite of what restoring is for.
        let resume = super::session_ids::load_id(&name)
            .filter(|id| super::session_ids::session_file_exists(&agent.name, &cwd, id))
            .or_else(|| super::session_ids::current_id(&agent.name, &cwd))
            .map(|id| {
                super::session_ids::resume_args_with(
                    &agent.name,
                    &id,
                    entry.provider.is_some(),
                )
            })
            .unwrap_or_default();

        // Resolve provider settings if needed
        let mut argv = agent.command.clone();
        argv.extend(resume);
        let mut env_vars: Vec<(String, String)> = Vec::new();
        if let Some(p) = &entry.provider {
            let app_type = crate::provider::agent_app_type(&agent.name);
            let settings = crate::provider::resolve_settings(p, app_type)?;
            argv.extend(settings.extra_argv);
            env_vars = settings.env_vars;
        }

        tmux::new_session_detached(&name, &cwd.to_string_lossy())?;
        let shell_cmd = if env_vars.is_empty() {
            tmux::shell_join(&argv)
        } else {
            let env_prefix: String = env_vars
                .iter()
                .map(|(k, v)| format!("{}={}", k, tmux::shell_quote(v)))
                .collect::<Vec<_>>()
                .join(" ");
            format!("{} {}", env_prefix, tmux::shell_join(&argv))
        };
        tmux::send_command(&name, &shell_cmd)?;
        created += 1;
        if attach_to.is_none() {
            attach_to = Some(name.clone());
        }
    }

    println!("Restored: {created} created, {skipped} already running, {errors} errors");

    // Attach to the first newly created session
    if let Some(name) = attach_to {
        tmux::attach_or_switch(&name)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agents() -> Vec<Agent> {
        vec![
            Agent { name: "claude".into(), alias: "cc".into(), command: vec!["claude".into()] },
            Agent { name: "codex".into(), alias: "cx".into(), command: vec!["codex".into()] },
        ]
    }

    /// A restore that opens blank conversations defeats the point: the whole
    /// reason to save and restore is to get the *work* back, not just a set of
    /// correctly-named empty shells.
    ///
    /// Both `run` and `restore` build their resume args from the same helper,
    /// so pinning the helper's output is what keeps them from drifting apart
    /// again. Deliberately touches no global state — `HOME`-mutating tests
    /// race each other under the parallel runner.
    #[test]
    fn restore_and_run_resolve_the_same_conversation() {
        use crate::commands::session_ids::resume_args_with;

        assert_eq!(
            resume_args_with("claude", "abc-123", false),
            vec!["--resume".to_string(), "abc-123".to_string()]
        );
        // With an explicit provider the codex patch stays suppressed here too —
        // restore resolves providers just like run does, so re-supplying one
        // would point the session at the proxy with the wrong key.
        assert_eq!(
            resume_args_with("codex", "abc-123", true),
            vec!["resume".to_string(), "abc-123".to_string()]
        );
    }

    #[test]
    fn detects_managed_and_ignores_others() {
        let all = vec![
            "cc_myproject_1a2b3c4d".to_string(),
            "cx_api_deadbeef".to_string(),
            "random_session".to_string(),
            "cc_nohash".to_string(),
        ];
        let m = managed_sessions(&all, &agents());
        let names: Vec<_> = m.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"cc_myproject_1a2b3c4d"));
        assert!(names.contains(&"cx_api_deadbeef"));
        assert!(!names.contains(&"random_session"));
        assert!(!names.contains(&"cc_nohash"));
    }

    #[test]
    fn detects_amux_new_suffixed_sessions() {
        // `amux new` appends `-<suffix>` to the normal name; those sessions
        // must still show up in `amux ls`.
        let all = vec![
            "cc_myproject_1a2b3c4d".to_string(),
            "cc_myproject_1a2b3c4d-debug".to_string(),
            "cc_myproject_1a2b3c4d-2".to_string(),
            "cx-glm_api_deadbeef-试验".to_string(),
            // A suffix must not rescue a name whose hash is malformed.
            "cc_myproject_notahash-debug".to_string(),
        ];
        let names: Vec<_> = managed_sessions(&all, &agents())
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert!(names.contains(&"cc_myproject_1a2b3c4d".to_string()));
        assert!(names.contains(&"cc_myproject_1a2b3c4d-debug".to_string()));
        assert!(names.contains(&"cc_myproject_1a2b3c4d-2".to_string()));
        assert!(names.contains(&"cx-glm_api_deadbeef-试验".to_string()));
        assert!(!names.contains(&"cc_myproject_notahash-debug".to_string()));
    }

    #[test]
    fn parse_session_alias_works() {
        assert_eq!(parse_session_alias("cc_myproject_1a2b3c4d"), Some(("cc", None)));
        assert_eq!(parse_session_alias("cc-openai_myproject_1a2b3c4d"), Some(("cc", Some("openai"))));
        assert_eq!(parse_session_alias("cx-deepseek_api_deadbeef"), Some(("cx", Some("deepseek"))));
        assert_eq!(parse_session_alias("_weird"), None);
    }

    #[test]
    fn session_slug_extracts_directory() {
        assert_eq!(session_slug("cc_myproject_1a2b3c4d"), Some("myproject"));
        assert_eq!(session_slug("cc-openai_my_proj_deadbeef"), Some("my_proj"));
        assert_eq!(session_slug("cx_api_deadbeef"), Some("api"));
        assert_eq!(session_slug("cc_nohash"), None);
    }

    #[test]
    fn detects_provider_sessions() {
        let all = vec![
            "cc-openai_myproject_1a2b3c4d".to_string(),
            "cx-anthropic_api_deadbeef".to_string(),
            "cc_myproject_1a2b3c4d".to_string(),
        ];
        let m = managed_sessions(&all, &agents());
        let names: Vec<_> = m.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"cc-openai_myproject_1a2b3c4d"));
        assert!(names.contains(&"cx-anthropic_api_deadbeef"));
        assert!(names.contains(&"cc_myproject_1a2b3c4d"));
    }
}
