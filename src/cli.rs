use clap::{Parser, Subcommand};
use std::ffi::OsString;
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
    /// Install amux + rmux and the cc/cx shortcuts onto your PATH (self-contained
    /// setup from a bundled or installed binary).
    InstallCli,
    /// Ensure Claude Code and Codex CLI are on the machine, installing any
    /// that are missing via npm / bun / brew.
    Install {
        /// Use Chinese mirrors for package registries and GitHub downloads.
        #[arg(long)]
        china: bool,
    },
    /// List amux-managed sessions.
    #[command(alias = "list")]
    Ls,
    /// Open an additional session for this directory, alongside any that are
    /// already running. `amux new debug`, or `amux new cx debug` for Codex.
    New {
        /// Name for the new session, or the agent when a name follows.
        first: String,
        /// Name for the new session, when the first argument is an agent.
        second: Option<String>,
    },
    /// List recent Claude Code and Codex conversations for this directory.
    Sessions {
        /// How many per agent (default 5).
        #[arg(long, short = 'n')]
        limit: Option<usize>,
    },
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
        /// Also surface agents running inside herdr (https://herdr.dev)
        /// alongside the rmux sessions.
        #[arg(long)]
        herdr: bool,
        /// Port for the DeepSeek Harness UI relay (default: this port + 1).
        /// Starts automatically when a `dsh web` is running, since dsh binds
        /// loopback only and a phone cannot reach it otherwise. Pass 0 to
        /// disable. Pair with `dsh web --trusted-host <address>`.
        #[arg(long, value_name = "PORT")]
        dsh_port: Option<u16>,
    },
    /// Record an explicit agent lifecycle status for the monitor.
    Hook {
        /// State: running/start, waiting, idle, failed, or done/completed.
        state: String,
        /// tmux pane id, e.g. %61. Defaults to $TMUX_PANE when omitted.
        #[arg(long, env = "TMUX_PANE")]
        pane: Option<String>,
        /// tmux session name. Useful when no pane id is available.
        #[arg(long)]
        session: Option<String>,
        /// Source label stored with the event.
        #[arg(long)]
        source: Option<String>,
        /// Optional task/turn id from the calling agent.
        #[arg(long)]
        task_id: Option<String>,
        /// Optional short status message.
        #[arg(long)]
        message: Option<String>,
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
    /// Fuzzy-match a session by directory name and attach to it.
    /// E.g. `amux mbox` attaches to a session whose directory matches "mbox".
    #[command(external_subcommand)]
    Goto(Vec<OsString>),
}
