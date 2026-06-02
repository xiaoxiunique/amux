# amux — Design Spec

**Date:** 2026-06-02
**Status:** Approved (design), pending implementation plan

## 1. Summary

`amux` is a Rust CLI that runs AI coding agents (Claude Code, Codex, and any
user-defined agent) inside **per-directory, persistent, re-attachable tmux
sessions**. Running an agent again from the same directory jumps back into its
live session instead of starting a new one.

It is cross-platform (Linux + macOS), depends only on `tmux`, and has no
Ghostty/AppleScript coupling. It is the generalization and packaging of the
user's existing `cc` / `cx` zsh functions so other users can install it via
Homebrew.

Relationship to gtab (the reference project): complementary. gtab manages
terminal **window layouts**; amux manages **agent sessions**.

## 2. Goals / Non-Goals

### Goals
- One command to launch-or-reattach an AI agent in a tmux session keyed to the
  current directory.
- Built-in `claude` (alias `cc`) and `codex` (alias `cx`); zero config to start.
- User-extensible: define arbitrary agents in a config file.
- Manage sessions: list, kill, and a keyboard TUI selector to switch between all
  running agent sessions across projects.
- Distributable via a Homebrew tap.

### Non-Goals (YAGNI)
- No window/layout save & restore (that is gtab's job).
- No process persistence beyond what tmux already provides.
- No remote / SSH session orchestration.
- No per-agent environment-variable injection in v1.

## 3. Core Concepts

### Agent
A named launch specification.

```toml
# ~/.config/amux/config.toml
[[agent]]
name    = "claude"
alias   = "cc"
command = ["claude", "--dangerously-skip-permissions"]

[[agent]]
name    = "codex"
alias   = "cx"
command = ["codex", "--yolo"]
```

- `name`    — canonical identifier used with `amux run <name>`.
- `alias`   — short shell alias installed by `amux init`; also the **session
  name prefix**.
- `command` — argv vector executed inside the tmux session.

Built-in defaults for `claude`/`codex` are compiled in. The config file
**overrides and extends** them. If the file is absent, the built-in defaults
apply.

### Session model (preserves existing `_agent_tmux_run` behavior)

Session name = `<alias>_<dirslug>_<hash8>`, e.g. `cc_myproject_1a2b3c4d`.

- `alias`   — agent alias (`cc`, `cx`).
- `dirslug` — sanitized basename of the absolute cwd (`[A-Za-z0-9_-]`,
  collapsed underscores; empty → `work`).
- `hash8`   — first 8 hex chars of SHA-256 of the absolute cwd, to disambiguate
  same-named directories in different paths.

Launch algorithm (identical semantics to the current zsh helper):
1. If `tmux` is not installed → run the agent command directly (foreground) and
   exit. (Graceful fallback.)
2. Compute session name from alias + cwd.
3. If session exists → attach to it: `switch-client` when already inside tmux,
   otherwise `attach-session`.
4. Else create detached in cwd (`new-session -d -s <name> -c <cwd>`), send the
   agent command + Enter, then attach/switch as in step 3.

The binary performs the attach by `exec`ing `tmux attach-session` (replacing
itself) when outside tmux, and by calling `tmux switch-client` (a server
command) when inside tmux.

## 4. Command Surface

| Command | Behavior |
|---------|----------|
| `amux run <agent> [args…]` | Core: create-or-reattach session for `<agent>` in cwd. Extra args are appended to the agent's `command`. |
| `amux init` | Write a managed block of shell aliases (one per agent: `cc`, `cx`, …) into the user's shell rc. Interactive-shell only. Does **not** install a binary literally named `cc` (would shadow the system C compiler). |
| `amux ls` (alias `list`) | List all amux-managed tmux sessions across directories: agent, directory, attached?, age. |
| `amux kill <name>` | Kill a named session. |
| `amux` (no args) | Open the TUI selector. |
| `amux config` | Print the resolved config and its file path. |

`amux init` installs aliases of the form `alias cc='amux run claude'` (or a thin
shell function) inside a clearly delimited managed block, so it can be rewritten
idempotently and removed cleanly. Because aliases live only in interactive
shells, the system `cc` (clang) is never shadowed for build tooling.

### Identifying "amux-managed" sessions

amux only lists/kills sessions whose names match the
`<known-alias>_<slug>_<8hex>` pattern for an alias present in the resolved
config, so it never touches unrelated tmux sessions.

## 5. TUI Selector

Opened by bare `amux`. Built with `ratatui` + `crossterm` (the same family gtab
uses).

- **Scope: global/unified** — lists all amux-managed sessions across every
  directory. Primary value: jump to any running agent in any project.
- Columns: agent, directory (abbreviated, e.g. `~/p/myproject`), attached
  indicator, age/idle.
- Keys:
  - `/` — filter/search.
  - `↑`/`↓` (and `j`/`k`) — move selection.
  - `Enter` — attach/switch to the selected session.
  - `d` — kill the selected session (with confirm).
  - `n` — start a **new** agent in the current directory: pick an agent from the
    configured list, then create-or-attach.
  - `q` / `Esc` — quit.

When the TUI triggers an attach/switch or launch, it tears down the TUI and
hands the terminal to tmux (same exec/switch logic as `amux run`).

## 6. Configuration

- Path: `$XDG_CONFIG_HOME/amux/config.toml`, default `~/.config/amux/config.toml`.
- Format: TOML, array-of-tables `[[agent]]` as shown above.
- Resolution order: built-in defaults → overlaid/extended by config file. An
  agent in config with the same `name` as a built-in overrides it.
- Validation: `alias` must be non-empty and match `[A-Za-z0-9_-]+`; `command`
  must be non-empty. Invalid entries are reported with a clear error.

## 7. Distribution

- Rust + Cargo workspace, single binary `amux`.
- Homebrew tap `<user>/amux` with a formula.
  - v1: build-from-source formula (`cargo install --path .`), mirroring gtab's
    approach for simplicity.
  - Later: prebuilt release binaries via GitHub Releases for faster installs.
- README with quick install, `amux init`, and the cc/cx workflow.

## 8. Cross-Platform Notes

- tmux runs on Linux and macOS; amux has no OS-specific dependencies.
- Hard dependency: `tmux`. If missing, amux falls back to running the agent
  directly (see launch algorithm step 1) and prints a hint.
- Shell rc detection for `amux init`: detect zsh vs bash via `$SHELL` /
  rc-file presence; write to `~/.zshrc` or `~/.bashrc` accordingly. A managed
  block with start/end markers makes the edit idempotent and removable.

## 9. Testing Strategy

- **Unit:** session-name generation (slug sanitization, hash determinism),
  config parsing/merge/validation, agent resolution, init managed-block
  insert/update/remove (idempotency).
- **Integration:** launch algorithm against a real tmux server in CI
  (create → reattach is idempotent; kill removes; ls only reports
  amux-managed sessions). Use a temp `$HOME`/cwd and unique session prefixes.
- **TUI:** logic layer (filtering, selection, action dispatch) tested
  independently of rendering; rendering smoke-tested with ratatui's test
  backend.
- Fallback path (no tmux) tested by stubbing tmux absence.

## 10. Open Questions

None blocking. Resolved decisions:
- TUI scope: global/unified. ✔
- TUI `n` to launch new agent in cwd: included in v1. ✔
- Session-name prefix: agent `alias`. ✔
- Language/form: Rust compiled binary. ✔
- Invocation: main command `amux` + `amux init` installs short aliases. ✔
