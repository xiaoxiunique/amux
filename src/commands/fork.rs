use crate::config::Agent;
use crate::{serve, tmux};
use anyhow::{Context, Result};
use std::path::PathBuf;

const DEFAULT_SUFFIX: &str = "fork";
const CONTEXT_LINES: usize = 400;
const MAX_CONTEXT_CHARS: usize = 40_000;

pub fn fork(source_arg: Option<&str>, name: Option<&str>, agents: &[Agent]) -> Result<()> {
    let (source, suffix) = resolve_source_and_suffix(source_arg, name)?;
    let target = fork_detached(&source, &suffix, agents)?;
    tmux::attach_or_switch(&target)
}

/// Create a fork without attaching. Used by the TUI so the screen can keep
/// drawing while the new agent starts and receives its context primer.
pub fn fork_detached(source: &str, suffix: &str, agents: &[Agent]) -> Result<String> {
    let suffix = serve::sessions::sanitize_suffix(&suffix).map_err(|e| anyhow::anyhow!(e))?;
    let target = unique_target_name(&source, &suffix);

    if tmux::has_session(&target) {
        return Ok(target);
    }

    let (alias, provider) = crate::commands::sessions::parse_session_alias(&source)
        .ok_or_else(|| anyhow::anyhow!("could not parse source session name: {source}"))?;
    let agent = agents
        .iter()
        .find(|agent| agent.alias == alias)
        .ok_or_else(|| anyhow::anyhow!("unknown agent alias '{alias}' in {source}"))?;
    let cwd = PathBuf::from(tmux::session_cwd(&source)?);

    let mut argv = agent.command.clone();
    let mut env_vars = Vec::new();
    if let Some(provider) = provider {
        let app_type = crate::provider::agent_app_type(&agent.name);
        let settings = crate::provider::resolve_settings(provider, app_type)?;
        argv.extend(settings.extra_argv);
        env_vars = settings.env_vars;
    }

    println!("forking {source} -> {target}");
    crate::commands::run::create_detached(agent, &cwd, &target, &argv, &env_vars)
        .with_context(|| format!("creating fork session {target}"))?;

    let primer = fork_primer(&source, agent, &cwd);
    tmux::paste_text(&target, &primer)?;
    tmux::send_enter(&target)?;

    crate::commands::sessions::auto_save(agents);
    Ok(target)
}

fn resolve_source_and_suffix(
    source_arg: Option<&str>,
    name: Option<&str>,
) -> Result<(String, String)> {
    match (source_arg, name) {
        (Some(source), Some(name)) => Ok((source.to_string(), name.to_string())),
        (Some(value), None) if tmux::has_session(value) => {
            Ok((value.to_string(), DEFAULT_SUFFIX.to_string()))
        }
        (Some(value), None) => {
            let source = tmux::current_session_name()
                .ok_or_else(|| anyhow::anyhow!("source session required outside a multiplexer"))?;
            Ok((source, value.to_string()))
        }
        (None, Some(name)) => {
            let source = tmux::current_session_name()
                .ok_or_else(|| anyhow::anyhow!("source session required outside a multiplexer"))?;
            Ok((source, name.to_string()))
        }
        (None, None) => {
            let source = tmux::current_session_name()
                .ok_or_else(|| anyhow::anyhow!("source session required outside a multiplexer"))?;
            Ok((source, DEFAULT_SUFFIX.to_string()))
        }
    }
}

fn unique_target_name(source: &str, suffix: &str) -> String {
    let base = format!("{source}-{suffix}");
    if !tmux::has_session(&base) {
        return base;
    }
    for n in 2..100 {
        let candidate = format!("{base}-{n}");
        if !tmux::has_session(&candidate) {
            return candidate;
        }
    }
    base
}

fn fork_primer(source: &str, agent: &Agent, cwd: &std::path::Path) -> String {
    let mut context = tmux::capture_pane_history(source, CONTEXT_LINES);
    if context.chars().count() > MAX_CONTEXT_CHARS {
        let chars: Vec<char> = context.chars().collect();
        context = chars[chars.len().saturating_sub(MAX_CONTEXT_CHARS)..]
            .iter()
            .collect();
    }
    let id = crate::commands::session_ids::load_id(source)
        .or_else(|| crate::commands::session_ids::current_id(&agent.name, cwd));
    let id_line = id
        .map(|id| format!("Source agent conversation id: {id}\n"))
        .unwrap_or_default();

    format!(
        "This is an amux fork from `{source}`. Treat the transcript below as background context only. This is a new independent conversation: do not write to or depend on the source session.\n\n{id_line}Recent visible terminal context from the source session:\n\n```text\n{context}\n```\n\nAcknowledge that this fork is ready, then wait for my next instruction."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_target_name_appends_suffix() {
        let source = "cx_proj_deadbeef";
        assert_eq!(format!("{source}-debug"), "cx_proj_deadbeef-debug");
    }

    #[test]
    fn primer_marks_the_new_conversation_independent() {
        let agent = Agent {
            name: "codex".into(),
            alias: "cx".into(),
            command: vec!["codex".into()],
        };
        let text = fork_primer(
            "cx_proj_deadbeef",
            &agent,
            std::path::Path::new("/tmp/proj"),
        );
        assert!(text.contains("new independent conversation"));
        assert!(text.contains("cx_proj_deadbeef"));
    }
}
