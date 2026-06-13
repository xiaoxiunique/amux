# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test

```bash
cargo build                  # debug build
cargo build --release        # release build
cargo test                   # all tests
cargo test -- test_name      # single test (substring match)
cargo run -- <amux args>     # run the binary (pass args after --)
cargo clippy                 # lint
cargo fmt                    # format
```

Tests are inline `#[cfg(test)] mod tests` blocks — no separate test files. The `tempfile` crate (regular dep) is used in tests. There are no dev-dependencies.

## Architecture

Single binary crate — no workspace, no sub-crates. Entry point is `src/main.rs`.

Two major subsystems live in the same binary: the **CLI** (tmux session manager) and the **serve** daemon (agent monitor HTTP/WS server).

### CLI subsystem

**Dispatch pattern:** `main.rs` resolves config (`config::resolve_agents()`), then matches on the clap `Command` enum and delegates to the appropriate `commands::*` module. No command (plain `amux`) launches the TUI.

**Session naming** (`src/session.rs`): `<alias>_<dirslug>_<hash8>` — the first 8 hex chars of SHA-256 of the canonicalized absolute directory path. This makes sessions deterministic: same agent + same directory always maps to the same tmux session.

**Config** (`src/config.rs`): Built-in agents (claude/cc, codex/cx) are merged with `~/.config/amux/config.toml`. File agents with the same `name` override builtins; new names are appended. `find()` matches by name or alias.

**Tmux wrapper** (`src/tmux.rs`): Wraps tmux as a subprocess (`new-session`, `send-keys`, `attach-session`/`switch-client`, `kill-session`, `list-sessions`). `attach_or_switch` detects whether we're already inside tmux and uses `switch-client` vs `attach-session` accordingly. Also contains the shell-quoting helper used to safely construct `send-keys` command lines.

**`run` command** (`src/commands/run.rs`): Canonicalizes cwd, generates session name, creates detached session if missing and sends the agent command, then attaches/switches. Falls back to running the agent directly if tmux is unavailable.

**`init` command** (`src/commands/init.rs`): Inserts a managed block (`# >>> amux ... >>>` / `# <<< amux ... <<<`) with `alias X='amux run agent'` lines into the shell rc file. Idempotent — re-running replaces the block, preserving surrounding user content. Also installs tmux keybindings and Ghostty keybindings.

**TUI** (`src/tui.rs`): ratatui + crossterm. Lists managed sessions (filtered by alias prefix + hash suffix pattern), supports vim keys, filter-as-you-type, attach/kill/new operations.

**Managed session detection** (`src/commands/sessions.rs`): Scans all tmux sessions for names matching `<alias>_<slug>_<8hex>` against known agent aliases.

### Serve subsystem (agent monitor server)

**`amux serve`** (`src/serve/mod.rs`): Daemonizes by re-executing itself with `--foreground`. PID file at `~/.amux/serve.pid`, log at `~/.amux/serve.log`. `amux stop` sends SIGTERM then SIGKILL.

**Server** (`src/serve/server.rs`): Axum HTTP + WebSocket server that monitors all tmux panes, captures output, and infers agent status (Running/Waiting/Idle/Failed/Done) from terminal output patterns. Polls tmux every 2.5s and broadcasts snapshots via WebSocket.

Key routes: `GET /api/snapshot`, `GET /ws` (snapshot stream), `POST /api/send` (paste into pane), `POST /api/key` (special keys), `GET /terminal/ws` (interactive PTY terminal), `POST /api/session/kill`, `GET/POST /api/project-history`, `GET/POST /api/cc-switch`, `POST /api/refine-text` (DeepSeek), `POST /api/upload-image`.

Auth: optional `--token` flag (checked via Bearer header or query param).

### Provider module (CC Switch)

`src/provider.rs` reads `~/.cc-switch/cc-switch.db` (SQLite via `rusqlite`) to resolve Claude/Codex provider settings. Used by both the CLI and the serve subsystem.

### State directories

- `~/.amux/` — serve PID file and log
- `~/.agent-monitor/project-history.json` — project launch history
- `~/.cc-switch/cc-switch.db` — provider database

### Environment variables

- `DEEPSEEK_API_KEY` — enables AI-powered interaction message enrichment in the server
- `AGENT_MONITOR_TMUX_PATH` — override tmux binary path
- `AGENT_MONITOR_STATE_DIR` — override state directory (default `~/.agent-monitor`)
- `CC_SWITCH_DB_PATH` — override CC Switch SQLite database path

## Key dependencies

- `clap` (derive) — CLI parsing
- `ratatui` + `crossterm` — TUI
- `serde` + `toml` — config deserialization
- `sha2` — deterministic session naming
- `dirs` — home/config directory resolution
- `anyhow` — error handling
- `axum` + `tokio` — HTTP/WebSocket server
- `rusqlite` (bundled) — CC Switch provider database
- `reqwest` — DeepSeek API client
- `portable-pty` — PTY for interactive terminal WebSocket
- `chrono` — timestamps
- `libc` — POSIX signals for daemon management
