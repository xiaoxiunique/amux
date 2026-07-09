use crate::config::Agent;
use crate::{provider, session, tmux};
use anyhow::{Context, Result};
use std::process::Command;

/// Launch-or-reattach the agent's session for the current directory.
/// Extra args are appended to the agent's command.
/// If `provider` is given, a CC Switch provider is resolved and injected.
///
/// Resume is pinned to the exact agent session id recorded for this amux
/// session (via `session_ids`), so re-launching continues the *same*
/// conversation instead of the directory's newest rollout. Automatically saves
/// the session list before attaching.
pub fn run(agent: &Agent, extra: &[String], provider_name: Option<&str>, agents: &[Agent]) -> Result<()> {
    let cwd = std::env::current_dir()
        .context("cannot read current directory")?
        .canonicalize()
        .context("cannot canonicalize current directory")?;

    // Resolve provider settings (extra args + env) if requested.
    let mut provider_argv: Vec<String> = Vec::new();
    let mut env_vars: Vec<(String, String)> = Vec::new();
    if let Some(p) = provider_name {
        let app_type = provider::agent_app_type(&agent.name);
        let settings = provider::resolve_settings(p, app_type)?;
        provider_argv = settings.extra_argv;
        env_vars = settings.env_vars;
    }

    // Session name includes provider for isolation.
    let alias = match provider_name {
        Some(p) => format!("{}-{}", agent.alias, p),
        None => agent.alias.clone(),
    };
    let name = session::session_name(&alias, &cwd);

    let tmux_ok = tmux::is_available();
    let session_exists = tmux_ok && tmux::has_session(&name);

    // Resolve which conversation to resume when we're about to (re)launch the
    // agent (no live tmux session to re-attach to):
    //   1. the amux-tracked session id, if its file still exists (precise), else
    //   2. the newest session for this cwd (matches the old `--last`, but
    //      explicit — and absent when the dir has no history, so a brand-new
    //      dir starts clean instead of erroring).
    let mut resume: Vec<String> = Vec::new();
    if !session_exists {
        let target = super::session_ids::load_id(&name)
            .filter(|id| super::session_ids::session_file_exists(&agent.name, &cwd, id))
            .or_else(|| super::session_ids::current_id(&agent.name, &cwd));
        if let Some(id) = target {
            resume = super::session_ids::resume_args(&agent.name, &id);
        }
    }

    // argv = command + resume + provider + extra
    // (codex's `-p` must follow `resume <id>`; this ordering satisfies that.)
    let mut argv = agent.command.clone();
    argv.extend(resume);
    argv.extend(provider_argv);
    argv.extend_from_slice(extra);

    if !tmux_ok {
        eprintln!("tmux not found; running '{}' directly", agent.name);
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        for (k, v) in &env_vars {
            cmd.env(k, v);
        }
        let status = cmd.status()?;
        std::process::exit(status.code().unwrap_or(1));
    }

    if !session_exists {
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

        // Auto-confirm codex's directory-trust prompt. Codex re-prompts on every
        // launch even for trusted dirs (regression: config/--yolo don't suppress
        // it), so send an Enter — it selects the default "Yes, continue". The
        // key is buffered in the pty until codex reads it; if no prompt appears
        // it lands on the empty composer and is a harmless no-op.
        if agent.name == "codex" {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            let _ = tmux::send_enter(&name);
        }
    } else {
        // Session is alive: the running agent is writing the newest rollout for
        // this cwd. Record its id so a later relaunch resumes this exact session
        // (agents create the rollout lazily on first interaction, so this
        // re-attach path — not launch time — is where we reliably learn the id).
        if let Some(id) = super::session_ids::current_id(&agent.name, &cwd) {
            super::session_ids::store_id(&name, &id);
        }
    }

    // Auto-save session list before attaching (exec replaces the process)
    super::sessions::auto_save(agents);
    tmux::attach_or_switch(&name)
}
