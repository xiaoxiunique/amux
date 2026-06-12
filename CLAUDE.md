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

## Architecture

Single binary crate — no workspace, no sub-crates. Entry point is `src/main.rs`.

**Dispatch pattern:** `main.rs` resolves config (`config::resolve_agents()`), then matches on the clap `Command` enum and delegates to the appropriate `commands::*` module. No command (plain `amux`) launches the TUI.

**Session naming** (`src/session.rs`): `<alias>_<dirslug>_<hash8>` — the first 8 hex chars of SHA-256 of the canonicalized absolute directory path. This makes sessions deterministic: same agent + same directory always maps to the same tmux session.

**Config** (`src/config.rs`): Built-in agents (claude/cc, codex/cx) are merged with `~/.config/amux/config.toml`. File agents with the same `name` override builtins; new names are appended. `find()` matches by name or alias.

**Tmux wrapper** (`src/tmux.rs`): Wraps tmux as a subprocess (`new-session`, `send-keys`, `attach-session`/`switch-client`, `kill-session`, `list-sessions`). `attach_or_switch` detects whether we're already inside tmux and uses `switch-client` vs `attach-session` accordingly. Also contains the shell-quoting helper used to safely construct `send-keys` command lines.

**`run` command** (`src/commands/run.rs`): Canonicalizes cwd, generates session name, creates detached session if missing and sends the agent command, then attaches/switches. Falls back to running the agent directly if tmux is unavailable.

**`init` command** (`src/commands/init.rs`): Inserts a managed block (`# >>> amux ... >>>` / `# <<< amux ... <<<`) with `alias X='amux run agent'` lines into the shell rc file. Idempotent — re-running replaces the block, preserving surrounding user content.

**TUI** (`src/tui.rs`): ratatui + crossterm. Lists managed sessions (filtered by alias prefix + hash suffix pattern), supports vim keys, filter-as-you-type, attach/kill/new operations.

**Managed session detection** (`src/commands/sessions.rs`): Scans all tmux sessions for names matching `<alias>_<slug>_<8hex>` against known agent aliases.

## Key dependencies

- `clap` (derive) — CLI parsing
- `ratatui` + `crossterm` — TUI
- `serde` + `toml` — config deserialization
- `sha2` — deterministic session naming
- `dirs` — home/config directory resolution
- `anyhow` — error handling
