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
            println!("Installed amux aliases into {}", path.display());
            println!("Reload your shell (e.g. `source {}`) to use them.", path.display());
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
