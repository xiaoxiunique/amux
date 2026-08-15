mod cli;
mod commands;
mod config;
mod provider;
mod serve;
mod session;
mod state;
mod tmux;
mod tui;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command};

fn main() -> Result<()> {
    let parsed = Cli::parse();
    let agents = config::resolve_agents()?;

    match parsed.command {
        Some(Command::Run {
            agent,
            provider,
            mut args,
        }) => {
            let a = config::find(&agents, &agent)
                .ok_or_else(|| anyhow::anyhow!("unknown agent '{agent}'"))?
                .clone();

            // Smart provider detection: if no --provider but first arg is a known
            // provider name, treat it as the provider.
            let app_type = provider::agent_app_type(&a.name);
            let provider = provider.or_else(|| {
                if let Some(first) = args.first() {
                    if !first.starts_with('-') && provider::is_known_provider(first, app_type) {
                        return Some(args.remove(0));
                    }
                }
                None
            });

            commands::run::run(&a, &args, provider.as_deref(), &agents)
        }
        Some(Command::Init) => {
            let path = commands::init::rc_path()
                .ok_or_else(|| anyhow::anyhow!("cannot determine shell rc path"))?;
            commands::init::install_to(&path, &agents)?;
            println!("Installed shell aliases into {}", path.display());

            // rmux keybindings (requires fzf)
            let has_fzf = std::process::Command::new("fzf")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success());

            if !has_fzf {
                eprintln!("Skipped rmux keybindings: fzf not found (brew install fzf)");
            } else if let Some(tp) = commands::init::mux_conf_path() {
                match commands::init::install_block(&tp, &commands::init::render_mux_block()) {
                    Ok(()) => println!("Installed rmux keybindings into {}", tp.display()),
                    Err(e) => eprintln!("Skipped rmux keybindings: {e}"),
                }
            }

            // ghostty keybindings
            if let Some(gp) = commands::init::ghostty_config_path() {
                match commands::init::install_block(&gp, &commands::init::render_ghostty_block()) {
                    Ok(()) => println!("Installed Ghostty keybindings into {}", gp.display()),
                    Err(e) => eprintln!("Skipped Ghostty keybindings: {e}"),
                }
            } else {
                eprintln!("Skipped Ghostty keybindings: config file not found");
            }

            match commands::init::install_agent_hooks() {
                Ok(messages) => {
                    for message in messages {
                        println!("{message}");
                    }
                }
                Err(e) => eprintln!("Skipped agent hooks: {e}"),
            }

            println!(
                "Reload your shell (e.g. `source {}`) to use aliases.",
                path.display()
            );
            Ok(())
        }
        Some(Command::Ls) => commands::sessions::list(&agents),
        Some(Command::Kill { name }) => commands::sessions::kill(&name),
        Some(Command::Config) => {
            if let Some(p) = config::config_path() {
                println!("config path: {}", p.display());
            }
            for a in &agents {
                println!("{:<10} {:<5} {}", a.name, a.alias, a.command.join(" "));
            }
            Ok(())
        }
        Some(Command::Serve {
            port,
            host,
            token,
            foreground,
            open,
            herdr,
            dsh_port,
        }) => serve::serve(
            port,
            host.as_deref(),
            token.as_deref(),
            foreground,
            open,
            herdr,
            dsh_port,
        ),
        Some(Command::Hook {
            state,
            pane,
            session,
            source,
            task_id,
            message,
        }) => {
            let state = state::HookState::parse(&state)?;
            let event = state::record_status(pane, session, state, source, task_id, message)?;
            println!("{}", serde_json::to_string(&event)?);
            Ok(())
        }
        Some(Command::Stop) => serve::stop(),
        Some(Command::InstallCli) => commands::init::install_cli(&agents),
        Some(Command::Install { china }) => commands::install::install_agents(&agents, china),
        Some(Command::New { first, second }) => {
            // `amux new <name>` uses the default agent; `amux new <agent> <name>`
            // picks one. A lone argument is always the name — except when it
            // names an agent, which is almost certainly a forgotten name rather
            // than a session someone meant to call "cx".
            let (agent_name, suffix) = match &second {
                Some(name) => (first.as_str(), name.as_str()),
                None => {
                    if config::find(&agents, &first).is_some() {
                        anyhow::bail!(
                            "'{first}' is an agent — give the session a name too, \
                             e.g. `amux new {first} debug`"
                        );
                    }
                    ("claude", first.as_str())
                }
            };
            let a = config::find(&agents, agent_name)
                .ok_or_else(|| anyhow::anyhow!("unknown agent '{agent_name}'"))?
                .clone();
            commands::new::new_session(&a, suffix, &agents)
        }
        Some(Command::Sessions { limit }) => commands::list::list_sessions(limit),
        Some(Command::Save { file }) => {
            commands::sessions::save(file.as_deref(), &agents)
        }
        Some(Command::Restore { file }) => {
            commands::sessions::restore(file.as_deref(), &agents)
        }
        Some(Command::Goto(args)) => {
            let query = args.first().and_then(|s| s.to_str()).unwrap_or("");
            // `amux <session-id>` resumes that conversation wherever it lives;
            // `amux <name>` stays the directory fuzzy-match. Ids are hex+dash,
            // project names aren't, so the two can't collide.
            if commands::session_ids::looks_like_session_id(query) {
                commands::list::resume_by_id(query, &agents)
            } else {
                commands::sessions::goto(query, &agents)
            }
        }
        None => tui::run_tui(&agents),
    }
}
