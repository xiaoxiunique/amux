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
            command: vec!["claude".into(), "--dangerously-skip-permissions".into()],
        },
        Agent {
            name: "codex".into(),
            alias: "cx".into(),
            command: vec!["codex".into(), "--yolo".into()],
        },
        Agent {
            name: "opencode".into(),
            alias: "oc".into(),
            // `--auto` is opencode's counterpart to the two flags above:
            // auto-approve anything not explicitly denied.
            //
            // No `--mini`. It used to be required: opencode's full TUI draws
            // into its own viewport (alternate screen by default, and even on
            // the main screen it repaints in place), so the terminal's
            // scrollback stays empty and `capture-pane` returns one screenful.
            // The monitor now reads the conversation from opencode's own store
            // (`session_ids::opencode_history`), so history no longer depends
            // on the terminal buffer and the full interface can be used.
            //
            // No `--continue` here. Resuming is amux's job — it passes
            // `--session <id>` for the conversation this session actually
            // owns. `--continue` picks the directory's *latest*, so two
            // opencode sessions in one directory would both reopen the same
            // thread, and the second would appear to have lost its work.
            command: vec!["opencode".into(), "--auto".into()],
        },
        Agent {
            // opencode v2 is the same CLI, renamed binary (`opencode2`), a
            // different session store (`session_v2`) and its own TUI as a thin
            // client of a shared background service. A separate entry rather
            // than an override so a directory can run v1 and v2 side by side —
            // `oc_amux_…` and `oc2_amux_…` are distinct sessions. Its history
            // does not carry over from v1 (different message schema), which is
            // why both exist at once.
            name: "opencode2".into(),
            alias: "oc2".into(),
            command: vec!["opencode2".into(), "--auto".into()],
        },
        Agent {
            name: "pi".into(),
            alias: "p".into(),
            // No auto-approve flag here, because pi has no approval gate to
            // switch off: its read/bash/edit/write tools run directly, which is
            // exactly the state the other three need a flag to reach.
            //
            // `--approve` is a different thing — it trusts a directory's own
            // extension and skill files, which are executable code shipped with
            // the project rather than the agent. That stays opt-in (`p -a`).
            //
            // Nothing resumes here either: pi records a session id and its cwd,
            // so `session_ids` pins the exact conversation the way it does for
            // claude and codex, instead of the blunt `--continue` opencode needs.
            command: vec!["pi".into()],
        },
        Agent {
            name: "commandcode".into(),
            alias: "cmd".into(),
            // Use the full binary name rather than `cmd`: on Windows `cmd` is
            // the shell, while Command Code documents `command-code` as the
            // cross-platform entrypoint. `--trust` skips its per-project trust
            // prompt and `--yolo` is its unattended/bypass permission mode.
            command: vec!["command-code".into(), "--trust".into(), "--yolo".into()],
        },
        Agent {
            // DeepSeek Harness (`dsh`) booted with its TUI profile. A profile
            // is a plugin-bundle stack, so the profile name is part of the
            // command — the user's own `dsh-tui` profile, not the shipped
            // `tui` one (which here carries a broken manifest).
            name: "dsh".into(),
            alias: "dsh".into(),
            command: vec!["dsh".into(), "--profile".into(), "dsh-tui".into()],
        },
    ]
}

/// amux's config directory: `$XDG_CONFIG_HOME/amux`, else `~/.config/amux`
/// (XDG layout on all platforms, including macOS).
pub fn config_dir() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("amux"));
        }
    }
    dirs::home_dir().map(|h| h.join(".config").join("amux"))
}

/// Default config path: `<config_dir>/config.toml`.
pub fn config_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("config.toml"))
}

/// Where the auto-mode supervisor prompt can be overridden.
///
/// Read fresh at each decision; a missing or empty file means the built-in
/// default is used, so deleting it is how you go back.
pub fn auto_prompt_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("auto-prompt.md"))
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
        // Only gemini is new; claude replaced its builtin in place.
        assert_eq!(merged.len(), builtin_agents().len() + 1);
    }

    #[test]
    fn builtins_cover_the_shipped_agents() {
        let b = builtin_agents();
        let by_alias = |a: &str| b.iter().find(|x| x.alias == a).cloned();
        assert_eq!(by_alias("cc").unwrap().name, "claude");
        assert_eq!(by_alias("cx").unwrap().name, "codex");
        let oc = by_alias("oc").expect("opencode ships as a builtin");
        assert_eq!(oc.name, "opencode");
        // The full interface: `--mini` is gone now that history no longer
        // depends on the terminal's scrollback (the monitor reads opencode's
        // own store instead).
        assert!(!oc.command.contains(&"--mini".to_string()));
        assert!(oc.command.contains(&"--auto".to_string()));
        // Resuming is amux's job (`--session <id>`), not a blanket --continue.
        assert!(!oc.command.contains(&"--continue".to_string()));
        let oc2 = by_alias("oc2").expect("opencode v2 ships as a builtin");
        assert_eq!(oc2.name, "opencode2");
        assert_eq!(oc2.command, vec!["opencode2", "--auto"]);
        let pi = by_alias("p").expect("pi ships as a builtin");
        assert_eq!(pi.name, "pi");
        // pi runs its tools without an approval gate, so the command carries no
        // auto-approve flag — and no `--continue`, because `session_ids` pins
        // the exact conversation instead.
        assert_eq!(pi.command, vec!["pi"]);
        let cmd = by_alias("cmd").expect("Command Code ships as a builtin");
        assert_eq!(cmd.name, "commandcode");
        assert_eq!(cmd.command, vec!["command-code", "--trust", "--yolo"]);
        let dsh = by_alias("dsh").expect("dsh ships as a builtin");
        assert_eq!(dsh.name, "dsh");
        assert_eq!(dsh.command, vec!["dsh", "--profile", "dsh-tui"]);
        validate(&b).unwrap();
        // find() resolves an agent by either name or alias.
        assert_eq!(find(&b, "oc").unwrap().name, "opencode");
        assert_eq!(find(&b, "opencode").unwrap().alias, "oc");
        assert_eq!(find(&b, "oc2").unwrap().name, "opencode2");
        assert_eq!(find(&b, "opencode2").unwrap().alias, "oc2");
        assert_eq!(find(&b, "p").unwrap().name, "pi");
        assert_eq!(find(&b, "pi").unwrap().alias, "p");
        assert_eq!(find(&b, "cmd").unwrap().name, "commandcode");
        assert_eq!(find(&b, "commandcode").unwrap().alias, "cmd");
    }

    #[test]
    fn validate_rejects_bad_alias_and_empty_command() {
        let bad_alias = vec![Agent { name: "x".into(), alias: "a b".into(), command: vec!["x".into()] }];
        assert!(validate(&bad_alias).is_err());
        let empty_cmd = vec![Agent { name: "x".into(), alias: "x".into(), command: vec![] }];
        assert!(validate(&empty_cmd).is_err());
    }
}
