use crate::config::Agent;
use crate::{provider, session, tmux};
use anyhow::{Context, Result};
use std::process::Command;

/// Launch-or-reattach the agent's session for the current directory.
/// Extra args are appended to the agent's command.
/// If `provider` is given, a CC Switch provider is resolved and injected.
/// Automatically saves the session list before attaching.
pub fn run(agent: &Agent, extra: &[String], provider_name: Option<&str>, agents: &[Agent]) -> Result<()> {
    let cwd = std::env::current_dir()
        .context("cannot read current directory")?
        .canonicalize()
        .context("cannot canonicalize current directory")?;

    let mut argv = agent.command.clone();
    let mut env_vars: Vec<(String, String)> = Vec::new();

    // Inject provider settings
    if let Some(p) = provider_name {
        let app_type = provider::agent_app_type(&agent.name);
        let settings = provider::resolve_settings(p, app_type)?;
        argv.extend(settings.extra_argv);
        env_vars = settings.env_vars;
    }

    argv.extend_from_slice(extra);

    if !tmux::is_available() {
        eprintln!("tmux not found; running '{}' directly", agent.name);
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        for (k, v) in &env_vars {
            cmd.env(k, v);
        }
        let status = cmd.status()?;
        std::process::exit(status.code().unwrap_or(1));
    }

    // Session name includes provider for isolation
    let alias = match provider_name {
        Some(p) => format!("{}-{}", agent.alias, p),
        None => agent.alias.clone(),
    };
    let name = session::session_name(&alias, &cwd);
    if !tmux::has_session(&name) {
        tmux::new_session_detached(&name, &cwd.to_string_lossy())?;
        // Build the command with env var prefixes for tmux send-keys
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
    }
    // Auto-save session list before attaching (exec replaces the process)
    super::sessions::auto_save(agents);
    tmux::attach_or_switch(&name)
}
