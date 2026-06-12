use crate::config::Agent;
use crate::{provider, session, tmux};
use anyhow::{Context, Result};
use std::process::Command;

/// Launch-or-reattach the agent's session for the current directory.
/// Extra args are appended to the agent's command.
/// If `provider` is given, a CC Switch settings file is generated and injected.
pub fn run(agent: &Agent, extra: &[String], provider: Option<&str>) -> Result<()> {
    let cwd = std::env::current_dir()
        .context("cannot read current directory")?
        .canonicalize()
        .context("cannot canonicalize current directory")?;

    let mut argv = agent.command.clone();

    // Inject provider settings
    if let Some(p) = provider {
        let settings_path = provider::resolve_and_write_settings(p)?;
        argv.push("--settings".into());
        argv.push(settings_path);
    }

    argv.extend_from_slice(extra);

    if !tmux::is_available() {
        eprintln!("tmux not found; running '{}' directly", agent.name);
        let status = Command::new(&argv[0]).args(&argv[1..]).status()?;
        std::process::exit(status.code().unwrap_or(1));
    }

    // Session name includes provider for isolation
    let alias = match provider {
        Some(p) => format!("{}-{}", agent.alias, p),
        None => agent.alias.clone(),
    };
    let name = session::session_name(&alias, &cwd);
    if !tmux::has_session(&name) {
        tmux::new_session_detached(&name, &cwd.to_string_lossy())?;
        tmux::send_command(&name, &tmux::shell_join(&argv))?;
    }
    tmux::attach_or_switch(&name)
}
