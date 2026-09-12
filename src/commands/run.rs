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
    let base = session::session_name(&agent.alias, &cwd);

    let tmux_ok = tmux::is_available();
    let live = |n: &str| tmux_ok && tmux::has_session(n);

    // Which session should hold this conversation?
    //
    // The directory's primary session is its home only while it is free, or
    // already on this very thread. Once it is busy with a *different* one,
    // taking it over would abandon the agent running there — so the request
    // used to be dropped instead, silently re-attaching the caller to whatever
    // was already open. Give the conversation a session of its own, named the
    // way `amux new` names a second workspace for a directory.
    let (name, session_exists) = if !live(&base) || already_open(agent, &cwd, &base, session_id) {
        let exists = live(&base);
        (base, exists)
    } else {
        let side = format!("{base}-{}", super::list::short_id(session_id));
        let exists = live(&side);
        if !exists {
            println!("{base} is on another conversation — opening {side} alongside it");
        }
        (side, exists)
    };

    // Pin the resume target to the requested id.
    let mut argv = agent.command.clone();
    if !session_exists {
        argv.extend(super::session_ids::resume_args(&agent.name, session_id));
        // Remember it, so relaunching this session resumes the same thread.
        super::session_ids::store_id(&name, session_id);
    }

    launch(agent, &cwd, &name, argv, Vec::new(), session_exists, tmux_ok, agents)
}

/// Whether the directory's primary session is already holding `session_id`.
///
/// The recorded id is the authoritative answer — every attach writes it. Only
/// when nothing is recorded (a session predating the store, or one rebuilt
/// after it was cleared) does the directory's newest conversation stand in,
/// since a live session is the process writing it. Answering "no" wrongly
/// starts a second agent on a transcript that already has a writer.
fn already_open(agent: &Agent, cwd: &std::path::Path, base: &str, session_id: &str) -> bool {
    match super::session_ids::load_id(base) {
        Some(id) => id == session_id,
        None => super::session_ids::current_id(&agent.name, cwd).as_deref() == Some(session_id),
    }
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

/// Start a session and leave it running in the background.
///
/// Split out of [`launch`] so callers that must *not* hand over the terminal
/// can reuse it — the TUI creates sessions without leaving the screen, which
/// is the whole point of doing it from there rather than dropping to a shell.
pub(crate) fn create_detached(
    agent: &Agent,
    cwd: &std::path::Path,
    name: &str,
    argv: &[String],
    env_vars: &[(String, String)],
) -> Result<()> {
    tmux::new_session_detached(name, &cwd.to_string_lossy())?;

    let shell_cmd = if env_vars.is_empty() {
        tmux::shell_launch(argv)
    } else {
        let env_prefix: String = env_vars
            .iter()
            .map(|(k, v)| format!("{}={}", k, tmux::shell_quote(v)))
            .collect::<Vec<_>>()
            .join(" ");
        format!("{} {}", env_prefix, tmux::shell_launch(argv))
    };
    tmux::send_command(name, &shell_cmd)?;

    if agent.name == "codex" {
        dismiss_codex_prompts(name);
    }
    Ok(())
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
        create_detached(agent, cwd, name, &argv, &env_vars)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::session_ids;
    use crate::config::Agent;

    fn claude() -> Agent {
        Agent {
            name: "claude".into(),
            alias: "cc".into(),
            command: vec!["claude".into()],
        }
    }

    /// Claude's escaping of a cwd into its project directory name.
    fn project_dir(home: &std::path::Path, cwd: &std::path::Path) -> std::path::PathBuf {
        let escaped: String = cwd
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        home.join(".claude").join("projects").join(escaped)
    }

    /// The question `amux <id>` gets wrong in both directions if this lies: a
    /// "yes" for a session busy with another thread silently drops the id (the
    /// caller lands back in whatever was already open), while a "no" for the
    /// session already holding it starts a second agent on one transcript.
    #[test]
    fn a_busy_primary_session_is_not_mistaken_for_the_requested_one() {
        let _home_guard = crate::test_home::lock();
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());

        let cwd = tmp.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();

        // The recorded id is the answer when there is one.
        session_ids::store_id("cc_proj_1a2b3c4d", "conv-a");
        assert!(already_open(&claude(), &cwd, "cc_proj_1a2b3c4d", "conv-a"));
        assert!(!already_open(&claude(), &cwd, "cc_proj_1a2b3c4d", "conv-b"));

        // With nothing recorded, the directory's newest transcript stands in —
        // a live session is the process writing it.
        let dir = project_dir(tmp.path(), &cwd);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("conv-newest.jsonl"), "{}\n").unwrap();
        assert!(already_open(&claude(), &cwd, "cc_proj_unrecorded", "conv-newest"));
        assert!(!already_open(&claude(), &cwd, "cc_proj_unrecorded", "conv-older"));

        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }

    /// The side session must stay an amux-managed name, or `amux ls` and the
    /// monitor would both lose track of it.
    #[test]
    fn the_side_session_name_is_still_managed() {
        let base = session::session_name("cc", std::path::Path::new("/tmp/proj"));
        let side = format!("{base}-{}", super::super::list::short_id("38977f06-5144-4e3c"));
        assert_eq!(side, format!("{base}-38977f06"));
        let managed = super::super::sessions::managed_sessions(&[side.clone()], &[claude()]);
        assert_eq!(managed.len(), 1, "{side} must be recognized as managed");
    }
}
