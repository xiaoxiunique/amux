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
amux init   # installs shell aliases + tmux/Ghostty keybindings
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
| `amux init` | Install shell aliases (`cc`, `cx`, …), tmux keybindings, and Ghostty keybindings. |
| `amux ls` | List amux-managed sessions across all directories. |
| `amux kill <name>` | Kill a session by name. |
| `amux config` | Show resolved config and its path. |
| `amux` | Open the TUI session selector. |

### TUI keys

`/` type to filter · `↑`/`↓` (or `j`/`k`) move · `Enter` attach · `d` kill ·
`n` start a new agent in the current directory · `q` quit.

## Web dashboard

`amux serve` runs a local server with a built-in web UI — every agent session
with live status and logs, plus input you can send — in your browser, no app to
install.

```bash
amux serve --open    # start and open http://localhost:8787
amux serve           # start in the background (daemon)
amux stop            # stop it
```

Add `--token <secret>` to require a bearer token, and reach it from another
device over Tailscale or an ngrok tunnel. The same server also backs the Agent
Port mobile app.

## Session switcher (Ghostty + tmux)

`amux init` also installs a quick session switcher for Ghostty split-pane
workflows:

- **Shift+Cmd+O** — pops up an fzf selector listing all amux sessions. Pick one
  to switch the current pane to that session.

This works by having Ghostty send `ESC O` on the key combo, which tmux picks up
as `M-O` and runs the fzf popup. Requires `fzf`; skipped automatically if fzf or
Ghostty is not installed.

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
- fzf (optional, for session switcher keybinding)
- Ghostty (optional, for Shift+Cmd+O keybinding)

## License

MIT
