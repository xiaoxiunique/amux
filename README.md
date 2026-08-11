# amux

Run AI coding agents (Claude Code, Codex, …) in per-directory, persistent,
re-attachable [rmux](https://rmux.io) sessions.

Run an agent from a directory and amux starts it in an rmux session keyed to that
directory. Run it again from the same directory and you jump straight back into
the live session instead of starting a new one.

> amux uses **rmux** (a tmux-compatible multiplexer) as its session backend —
> it avoids tmux's multi-session mouse-scroll crash and runs on Linux, macOS and
> Windows. Set `AMUX_MUX=tmux` to fall back to tmux.

## Install

One command, on any platform:

```bash
curl -fsSL https://amux.cc/install.sh | sh          # macOS / Linux
```

```powershell
irm https://amux.cc/install.ps1 | iex               # Windows
```

This installs amux and rmux, then sets up the shell aliases and keybindings.
Behind a slow connection to GitHub, set `AMUX_GITHUB_PROXY` first.

Or via Homebrew:

```bash
brew tap xiaoxiunique/amux
brew trust xiaoxiunique/amux   # Homebrew 6+ requires trusting third-party taps
brew install amux
amux init   # installs shell aliases + rmux/Ghostty keybindings
```

No agent CLIs on the machine yet? `amux install` fetches Claude Code and Codex,
bootstrapping a package manager if there isn't one (`--china` for mirrors).

Reload your shell, then:

```bash
cd ~/projects/myapp
cc          # starts Claude Code in an rmux session for this dir
# detach with Ctrl-b d; run `cc` again here to jump back
cx          # starts Codex in its own session
amux myapp  # from anywhere: fuzzy-jump back to this dir's session
amux        # TUI: switch between all running agent sessions
amux ls     # list sessions
```

## Commands

| Command | Behavior |
|---------|----------|
| `amux run <agent> [args…]` | Launch or reattach `<agent>` in the current directory. Extra args are forwarded to the agent. |
| `amux <dir>` | Fuzzy-match a running session by directory name and jump to it from anywhere — e.g. `amux mbox`. Prompts if several match. |
| `amux <session-id>` | Resume a past conversation by id prefix, from anywhere, in whichever agent recorded it — e.g. `amux 019fc770`. |
| `amux new [agent] <name>` | Open an **additional** session for this directory, alongside one already running. `amux new debug`, or `amux new cx debug` for Codex. |
| `amux sessions` | List recent Claude Code and Codex conversations for this directory, with each one's title. |
| `amux init` | Install shell aliases (`cc`, `cx`, …), rmux keybindings, and Ghostty keybindings. |
| `amux install` | Install any missing agent CLIs (Claude Code, Codex), bootstrapping a package manager if the machine has none. `--china` uses mirrors. |
| `amux install-cli` | Put amux + rmux and the `cc`/`cx` shortcuts on your PATH — for a self-contained copy, without Homebrew. |
| `amux ls` | List amux-managed sessions across all directories. |
| `amux kill <name>` | Kill a session by name. |
| `amux save [file]` / `amux restore [file]` | Save the running session list and restore it later (default `~/.amux/sessions.json`). |
| `amux config` | Show resolved config and its path. |
| `amux` | Open the TUI session selector. |

### Two agents on one project

`amux run` maps one directory to one session, so running `cc` twice in a
directory reattaches rather than starting a second agent. When you do want two
side by side, name the extra one:

```bash
cd ~/projects/myapp
cc                  # the project's main session
amux new debug      # a second Claude session, cc_myapp_<hash>-debug
amux new cx review  # and a Codex one
```

The extra session is independent: it starts a fresh conversation and never
touches what's running in the main one.

### Past conversations

```bash
amux sessions       # what has run in this directory, newest first
#   Claude Code (最近 5):
#     9f5202fb     542KB  3小时前
#       分析 IPA 包跳转 schema
amux 9f5202fb       # reopen that one — works from any directory
```

Titles come from the agents' own stores, so conversations started outside amux
show up too.

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

## Session switcher (Ghostty + rmux)

`amux init` also installs a quick session switcher for Ghostty split-pane
workflows:

- **Shift+Cmd+O** — opens a list of all sessions. `j`/`k` or `↑`/`↓` to move,
  `Enter` to switch the current pane to that session, `q`/`Esc` to cancel.

Ghostty sends `ESC o` on the key combo, which rmux picks up as `M-o` and binds
to its native `choose-tree`. An fzf popup would be nicer to search, but rmux's
`display-popup` has no client context — `switch-client` from inside one can't
tell which client to move on a multi-client server, so the popup would open and
then fail to switch anything.

## Clipboard

`amux init` also wires mouse selection to the system clipboard. The binding
covers rmux's default `emacs` mode-keys as well as `copy-mode-vi`, so
double-click, triple-click and drag all copy — on macOS via `pbcopy`, on
Windows via `clip`.

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

The rmux session name is `<alias>_<dirslug>_<hash8>`, e.g.
`cc_myproject_1a2b3c4d`, where `hash8` is the first 8 hex chars of the SHA-256 of
the absolute directory path. Same agent + same directory always maps to the same
session, so re-running attaches instead of duplicating. `amux new <name>` appends
`-<name>`, which is how a second session for the same directory stays distinct.

### Conversation resume

When a session's rmux window is gone (reboot, `amux kill`), re-running the agent
resumes the *exact* conversation it was on, not just "the latest". amux records
each session's real agent session id (Claude/Codex) in `~/.amux/session-ids.json`
and relaunches with `codex resume <id>` / `claude --resume <id>`. This stays
correct even after switching model providers: Codex refuses to open a session
whose recorded provider is no longer in `config.toml` (CC Switch rewrites that
table on every switch), so amux reads the provider name back out of the
transcript and re-supplies it on the command line.

## Requires

- rmux (Linux, macOS, or Windows). Without a multiplexer, amux runs the agent directly. `AMUX_MUX=tmux` falls back to tmux.
- Ghostty (optional, for the Shift+Cmd+O session switcher)

## License

MIT
