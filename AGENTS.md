# AGENTS.md

Single Rust binary crate (`amux`), no workspace. One process is both the CLI
(tmux/rmux session manager) and the `serve` HTTP/WS daemon. Entry point
`src/main.rs` matches on the clap `Command` enum and delegates to
`src/commands/*`; plain `amux` launches the TUI (`src/tui.rs`).

## Build / test / lint

```bash
cargo build                     # debug
cargo build --release
cargo test                      # all tests
cargo test <name>                # one test or module (substring match)
cargo check                     # typecheck without linking
cargo clippy                    # lint (NOT enforced in CI)
cargo fmt -- --check             # check without rewriting
cargo build --features full      # macOS-only host extras
```

- CI (`.github/workflows/ci.yml`) runs `cargo build` + `cargo test` on
  ubuntu/macos/windows, plus a `full` build on macOS. Keep Unix-only code behind
  `#[cfg(unix)]`; Windows is a supported target.
- Tests are inline `#[cfg(test)] mod tests`. There are no dev-dependencies;
  `tempfile` is a regular dep. Tests must not require a real rmux/tmux — the
  multiplexer round-trip tests self-skip when none is installed.
- The ignored auto-mode integration test starts local Claude Code and consumes
  quota: opt in with `cargo test auto_asks_the_local_model -- --ignored --nocapture`.
- Formatting currently has widespread drift; use check-only mode rather than
  reformatting unrelated code. Clippy and formatting are not CI gates.
- Release: pushing a `v*` tag runs `.github/workflows/release.yml`, which builds
  binaries without `full` and rewrites the external Homebrew tap formula.
  Version lives in `Cargo.toml`; root `Formula/amux.rb` is stale.

## Test isolation gotcha

`HOME`, `AMUX_DB_PATH` and `AMUX_STATE_DIR` are process-global; two tests that
redirect them under the parallel runner clobber each other. Any test that reads
or sets them must hold `crate::test_home::lock()` (`src/main.rs`), and tests
needing a throwaway metadata DB should use `crate::test_home::scratch_db()`.
`scratch_db()` already holds the lock; do not acquire it again. Keep its guard
alive for the test so the previous DB path is restored on drop. Without isolation,
store calls can write to the user's real DB. `store.rs` caches its connection keyed
by `AMUX_DB_PATH`.

## Non-obvious architecture

- **Built-in agents** (`src/config.rs`) are `claude/cc`, `codex/cx`,
  `opencode/oc`, `pi/p` — not just claude/codex (README/CLAUDE.md are stale).
  CLI and server launch commands use the shared config list; do not add a second
  hardcoded list. The server still honors legacy `AGENT_MONITOR_CC_COMMAND` /
  `AGENT_MONITOR_CX_COMMAND` overrides.
- **Session naming** (`src/session.rs`): `<alias>_<slug>_<hash8>` where hash8 is
  the first 8 hex of SHA-256 of the canonicalized abs path; `amux new` appends
  `-<suffix>`.
- **`src/tmux.rs`** (name is legacy) drives the multiplexer as a subprocess and
  defaults to **rmux**; `AMUX_MUX=tmux` falls back to tmux.
- `src/serve/server.rs` owns routing, snapshots/status inference, and terminal
  WebSockets; the TUI reuses its status API. The `full` feature adds macOS
  control-center / usage / APNs routes in `src/serve/full/`.
- **`webui/`** is a *prebuilt* Flutter web bundle embedded at compile time via
  `include_dir!` in `src/serve/server.rs`. There are no Dart sources in this
  repo — do not hand-edit it.
- **`docs/`** is the amux.cc website + install scripts, not project docs.
- Do not add AI attribution or `Co-Authored-By` lines to commit messages.
- `CLAUDE.md` is an older architecture write-up; verify its details against the
  code (several are out of date).

## State & environment

- `src/store.rs` owns SQLite metadata in `~/.amux/amux.db` (WAL), migrating the
  old project-history, session-labels, and session-ids JSON files. Do not use
  `scripts/backfill-session-ids.py` as a current storage reference: it writes
  the old JSON path. Agent transcripts remain in the agents' own stores.
- `~/.amux/` also holds `serve.pid`, `serve.log`, `state/` (status snapshot +
  `events.ndjson`), and `sessions.json` (save/restore). The daemon's port is in
  the DB (`serve.port`), not the pid file.
- Some runtime artifacts are committed anyway — `output/mobile-uploads/`,
  `excalidraw.log`, `.DS_Store`; don't treat them as source or add more.
- `~/.config/amux/config.toml` (`$XDG_CONFIG_HOME` honored) — extra agents
- `~/.cc-switch/cc-switch.db` — provider database (`src/provider.rs`), separate
  from amux metadata; override with `CC_SWITCH_DB_PATH`.
