mod cli;
mod commands;
mod config;
mod session;
mod tmux;
mod tui;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command};

fn main() -> Result<()> {
    let parsed = Cli::parse();
    let agents = config::resolve_agents()?;

    match parsed.command {
        Some(Command::Run { agent, args }) => {
            let a = config::find(&agents, &agent)
                .ok_or_else(|| anyhow::anyhow!("unknown agent '{agent}'"))?
                .clone();
            commands::run::run(&a, &args)
        }
        Some(Command::Init) => {
            let path = commands::init::rc_path()
                .ok_or_else(|| anyhow::anyhow!("cannot determine shell rc path"))?;
            commands::init::install_to(&path, &agents)?;
            println!("Installed shell aliases into {}", path.display());

            // tmux keybindings (requires fzf)
            let has_fzf = std::process::Command::new("fzf")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success());

            if !has_fzf {
                eprintln!("Skipped tmux keybindings: fzf not found (brew install fzf)");
            } else if let Some(tp) = commands::init::tmux_conf_path() {
                match commands::init::install_block(&tp, &commands::init::render_tmux_block()) {
                    Ok(()) => println!("Installed tmux keybindings into {}", tp.display()),
                    Err(e) => eprintln!("Skipped tmux keybindings: {e}"),
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
        None => tui::run_tui(&agents),
    }
}
