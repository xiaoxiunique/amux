use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Agent {
    pub name: String,
    pub alias: String,
    pub command: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default, rename = "agent")]
    agents: Vec<Agent>,
}

pub fn builtin_agents() -> Vec<Agent> {
    vec![
        Agent {
            name: "claude".into(),
            alias: "cc".into(),
            command: vec!["claude".into(), "--dangerously-skip-permissions".into(), "--continue".into()],
        },
        Agent {
            name: "codex".into(),
            alias: "cx".into(),
            command: vec!["codex".into(), "--yolo".into(), "resume".into(), "--last".into()],
        },
    ]
}

/// Default config path: `$XDG_CONFIG_HOME/amux/config.toml`, else
/// `~/.config/amux/config.toml` (XDG layout on all platforms, including macOS).
pub fn config_path() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("amux").join("config.toml"));
        }
    }
    dirs::home_dir().map(|h| h.join(".config").join("amux").join("config.toml"))
}

pub fn parse_config(toml_str: &str) -> Result<Vec<Agent>> {
    let cf: ConfigFile = toml::from_str(toml_str).context("invalid amux config")?;
    Ok(cf.agents)
}

/// File agents override builtins with the same `name`; new names are appended.
pub fn merge(builtin: Vec<Agent>, from_file: Vec<Agent>) -> Vec<Agent> {
    let mut out = builtin;
    for a in from_file {
        if let Some(slot) = out.iter_mut().find(|x| x.name == a.name) {
            *slot = a;
        } else {
            out.push(a);
        }
    }
    out
}

pub fn validate(agents: &[Agent]) -> Result<()> {
    for a in agents {
        let ok_alias = !a.alias.is_empty()
            && a.alias.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !ok_alias {
            bail!("agent '{}' has invalid alias '{}'", a.name, a.alias);
        }
        if a.command.is_empty() {
            bail!("agent '{}' has empty command", a.name);
        }
    }
    Ok(())
}

/// Resolve the full agent list: builtins overlaid with the config file (if any).
pub fn resolve_agents() -> Result<Vec<Agent>> {
    let from_file = match config_path() {
        Some(p) if p.exists() => {
            let text = std::fs::read_to_string(&p)
                .with_context(|| format!("reading {}", p.display()))?;
            parse_config(&text)?
        }
        _ => Vec::new(),
    };
    let merged = merge(builtin_agents(), from_file);
    validate(&merged)?;
    Ok(merged)
}

pub fn find<'a>(agents: &'a [Agent], name: &str) -> Option<&'a Agent> {
    agents.iter().find(|a| a.name == name || a.alias == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_present() {
        let a = builtin_agents();
        assert!(find(&a, "claude").is_some());
        assert!(find(&a, "cx").is_some());
    }

    #[test]
    fn parse_extra_agent() {
        let toml = r#"
            [[agent]]
            name = "gemini"
            alias = "cg"
            command = ["gemini", "chat"]
        "#;
        let parsed = parse_config(toml).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].alias, "cg");
    }

    #[test]
    fn merge_overrides_and_appends() {
        let file = vec![
            Agent { name: "claude".into(), alias: "cc".into(), command: vec!["claude".into()] },
            Agent { name: "gemini".into(), alias: "cg".into(), command: vec!["gemini".into()] },
        ];
        let merged = merge(builtin_agents(), file);
        // claude overridden (command now single element), gemini appended
        assert_eq!(find(&merged, "claude").unwrap().command, vec!["claude".to_string()]);
        assert!(find(&merged, "gemini").is_some());
        assert_eq!(merged.len(), 3);
    }

    #[test]
    fn validate_rejects_bad_alias_and_empty_command() {
        let bad_alias = vec![Agent { name: "x".into(), alias: "a b".into(), command: vec!["x".into()] }];
        assert!(validate(&bad_alias).is_err());
        let empty_cmd = vec![Agent { name: "x".into(), alias: "x".into(), command: vec![] }];
        assert!(validate(&empty_cmd).is_err());
    }
}
