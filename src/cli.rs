use clap::{Parser, Subcommand};
use std::path::PathBuf;

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
        /// Use a CC Switch provider (e.g. glm, sub, deepseek).
        #[arg(long)]
        provider: Option<String>,
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
    /// Start the agent monitor server (daemon by default).
    Serve {
        /// Port to listen on.
        #[arg(long, default_value_t = 8787)]
        port: u16,
        /// Bind address.
        #[arg(long)]
        host: Option<String>,
        /// Auth token (optional).
        #[arg(long)]
        token: Option<String>,
        /// Run in foreground instead of daemonizing.
        #[arg(long, short)]
        foreground: bool,
        /// Open the web UI in your browser after starting.
        #[arg(long)]
        open: bool,
    },
    /// Stop the agent monitor daemon.
    Stop,
    /// Save the current session list to a file.
    Save {
        /// Output file (default: ~/.amux/sessions.json).
        file: Option<PathBuf>,
    },
    /// Restore sessions from a saved file.
    Restore {
        /// Input file (default: ~/.amux/sessions.json).
        file: Option<PathBuf>,
    },
}
