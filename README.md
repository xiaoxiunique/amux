# amux

Run AI coding agents (Claude Code, Codex, …) in per-directory, persistent,
re-attachable tmux sessions.

Run an agent from a directory and amux starts it in a tmux session keyed to that
directory. Run it again from the same directory and you jump straight back into
the live session instead of starting a new one.

## Install

```bash
brew tap xiaoxiunique/amux
brew install amux
amux init   # installs cc / cx aliases into your shell rc
```

Reload your shell, then:

```bash
cd ~/projects/myapp
cc          # starts Claude Code in a tmux session for this dir
# detach with Ctrl-b d; run `cc` again here to jump back
cx          # starts Codex in its own session
amux        # TUI: switch between all running agent sessions
amux ls     # list sessions
```

## Commands

| Command | Behavior |
|---------|----------|
| `amux run <agent> [args…]` | Launch or reattach `<agent>` in the current directory. Extra args are forwarded to the agent. |
| `amux init` | Install shell aliases (`cc`, `cx`, …) into your rc file. |
| `amux ls` | List amux-managed sessions across all directories. |
| `amux kill <name>` | Kill a session by name. |
| `amux config` | Show resolved config and its path. |
| `amux` | Open the TUI session selector. |

### TUI keys

`/` type to filter · `↑`/`↓` (or `j`/`k`) move · `Enter` attach · `d` kill ·
`n` start a new agent in the current directory · `q` quit.

## Configuration

`~/.config/amux/config.toml` (or `$XDG_CONFIG_HOME/amux/config.toml`):

```toml
[[agent]]
name    = "gemini"
alias   = "cg"
command = ["gemini", "chat"]
```

Built-in `claude` (cc) and `codex` (cx) work with no config. An agent with the
same `name` as a built-in overrides it. Re-run `amux init` after adding agents to
install their aliases.

## How it works

The tmux session name is `<alias>_<dirslug>_<hash8>`, e.g.
`cc_myproject_1a2b3c4d`, where `hash8` is the first 8 hex chars of the SHA-256 of
the absolute directory path. Same agent + same directory always maps to the same
session, so re-running attaches instead of duplicating.

## Requires

- tmux (Linux or macOS). Without tmux, amux runs the agent directly.

## License

MIT
