use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "amux", version, about = "Run AI agents in per-directory tmux sessions")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Launch or reattach an agent in the current directory.
    Run {
        /// Agent name or alias (e.g. claude, cc).
        agent: String,
        /// Extra args forwarded to the agent command.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Install shell aliases (cc, cx, ...) into your rc file.
    Init,
    /// List amux-managed sessions.
    #[command(alias = "list")]
    Ls,
    /// Kill a session by name.
    Kill { name: String },
    /// Show resolved config and its path.
    Config,
}
