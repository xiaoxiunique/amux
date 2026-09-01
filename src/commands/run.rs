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
            resume = super::session_ids::resume_args_with(
                &agent.name,
                &id,
                provider_name.is_some(),
            );
        }
    }

    // argv = command + resume + provider + extra
    // (codex's `-p` must follow `resume <id>`; this ordering satisfies that.)
    let mut argv = agent.command.clone();
    argv.extend(resume);
    argv.extend(provider_argv);
    argv.extend_from_slice(extra);

    launch(agent, &cwd, &name, argv, env_vars, session_exists, tmux_ok, agents)
}

/// Attach to a specific past conversation, in its own directory.
///
/// Unlike [`run`], the directory and session id come from the caller rather
/// than the process cwd and the usual "newest for here" resolution — this
/// backs `amux <id>`, which is meant to work from anywhere.
pub fn run_in(
    agent: &Agent,
    cwd: &std::path::Path,
    session_id: &str,
    agents: &[Agent],
) -> Result<()> {
    let cwd = cwd
        .canonicalize()
        .with_context(|| format!("cannot canonicalize {}", cwd.display()))?;
    let name = session::session_name(&agent.alias, &cwd);

    let tmux_ok = tmux::is_available();
    let session_exists = tmux_ok && tmux::has_session(&name);

    // Pin the resume target to the requested id. A live session for this
    // directory is re-attached as-is: the agent is already running in it, and
    // relaunching would abandon that process.
    let mut argv = agent.command.clone();
    if !session_exists {
        argv.extend(super::session_ids::resume_args(&agent.name, session_id));
        // Remember it, so a later plain `cc`/`cx` here resumes the same thread.
        super::session_ids::store_id(&name, session_id);
    }

    launch(agent, &cwd, &name, argv, Vec::new(), session_exists, tmux_ok, agents)
}

/// Start (or reattach) a session under an explicit name, with a fresh agent.
///
/// Backs `amux new`. No resume args: this is a deliberately independent second
/// workspace for the same directory, not a continuation of the directory's
/// newest conversation. Its conversation *is* recorded once it exists, so the
/// session survives being rebuilt.
pub fn launch_new(
    agent: &Agent,
    cwd: &std::path::Path,
    name: &str,
    session_exists: bool,
    tmux_ok: bool,
    agents: &[Agent],
) -> Result<()> {
    launch(
        agent,
        cwd,
        name,
        agent.command.clone(),
        Vec::new(),
        session_exists,
        tmux_ok,
        agents,
    )
}

/// Answer the prompts codex shows before it will open a conversation.
///
/// There are two, and they need *different* answers — which is why this reads
/// the screen instead of sending a blind Enter:
///
///   - "Update available … 1. Update now / 2. Skip". Enter takes the default,
///     **1**, which shells out to `bun install -g @openai/codex`. Answering
///     this one by reflex upgrades codex behind the user's back, mid-session.
///   - "Do you trust the contents of this directory? … 1. Yes, continue".
///     Here the default is right, so Enter.
///
/// Codex re-asks the trust question on every launch even for trusted dirs, and
/// the update prompt appears only when a release is out — so the order and the
/// presence of each varies. Poll briefly and answer whatever is on screen.
pub(crate) fn dismiss_codex_prompts(name: &str) {
    use std::time::Duration;

    let mut answered = false;
    // ~8s of polling. Codex takes a couple of seconds to boot and redraws
    // between the two prompts, and the update one only exists when a release
    // is out — so a fixed sleep either fires too early or wastes time.
    for _ in 0..16 {
        std::thread::sleep(Duration::from_millis(500));
        let screen = tmux::capture_pane(name);

        if screen.contains("Update available") {
            // Explicitly "2" (Skip). Never Enter: the default is "1. Update
            // now", which shells out to a global package install.
            let _ = tmux::send_text(name, "2");
            answered = true;
            continue;
        }
        if screen.contains("Do you trust") {
            let _ = tmux::send_enter(name);
            answered = true;
            continue;
        }

        // Only leave early once a prompt has actually been dealt with.
        // Returning on any non-empty screen would fire on the shell prompt
        // that is still there before codex has even started drawing.
        if answered {
            return;
        }
    }
}

/// Create-or-attach the multiplexer session and hand the terminal over.
#[allow(clippy::too_many_arguments)]
fn launch(
    agent: &Agent,
    cwd: &std::path::Path,
    name: &str,
    argv: Vec<String>,
    env_vars: Vec<(String, String)>,
    session_exists: bool,
    tmux_ok: bool,
    agents: &[Agent],
) -> Result<()> {
    if !tmux_ok {
        eprintln!("tmux not found; running '{}' directly", agent.name);
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        cmd.current_dir(cwd);
        for (k, v) in &env_vars {
            cmd.env(k, v);
        }
        let status = cmd.status()?;
        std::process::exit(status.code().unwrap_or(1));
    }

    if !session_exists {
        tmux::new_session_detached(name, &cwd.to_string_lossy())?;
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
        tmux::send_command(name, &shell_cmd)?;

        if agent.name == "codex" {
            dismiss_codex_prompts(name);
        }
    } else {
        // Session is alive: the running agent is writing a rollout for this
        // cwd. Record its id so a later relaunch resumes this exact
        // conversation (agents create the rollout lazily on first interaction,
        // so this re-attach path — not launch time — is where we reliably
        // learn it).
        //
        // Claim the newest rollout no *other* session already holds. Recording
        // the plain newest would hand a second session in the same directory
        // its neighbour's conversation; recording nothing — which is what
        // `amux new` sessions used to do — meant a named session lost its work
        // the first time it had to be rebuilt.
        if let Some(id) =
            super::session_ids::current_unclaimed_id(&agent.name, cwd, name)
        {
            super::session_ids::store_id(name, &id);
        }
    }

    // Auto-save session list before attaching (exec replaces the process)
    super::sessions::auto_save(agents);
    tmux::attach_or_switch(name)
}
