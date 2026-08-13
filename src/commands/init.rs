use crate::config::Agent;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

const BEGIN: &str = "# >>> amux managed block >>>";
const END: &str = "# <<< amux managed block <<<";
const CODEX_NOTIFY_BEGIN: &str = "# >>> amux managed codex notify >>>";
const CODEX_NOTIFY_END: &str = "# <<< amux managed codex notify <<<";

/// Render the alias block for the given agents.
pub fn render_block(agents: &[Agent]) -> String {
    let mut s = String::new();
    s.push_str(BEGIN);
    s.push('\n');
    for a in agents {
        s.push_str(&format!("alias {}='amux run {}'\n", a.alias, a.name));
    }
    s.push_str(END);
    s
}

/// Render the rmux setup block: mouse text-selection → clipboard, plus the fzf
/// session switcher on M-o (Ghostty Shift+Cmd+O).
pub fn render_mux_block() -> String {
    // The clipboard pipe command is platform-specific: pbcopy on macOS,
    // clip on Windows. The body is a format string, so we resolve the
    // platform-dependent value before it is substituted.
    let clipper = if cfg!(windows) { "clip" } else { "pbcopy" };
    format!(
        "{BEGIN}\n\
         # mouse text selection copies to the system clipboard\n\
         set -g mouse on\n\
         # Default pipe for every copy-mode binding that calls copy-pipe*\n\
         # without an explicit command — which is most of them, including the\n\
         # built-in double/triple-click word and line selections. Binding only\n\
         # y and MouseDragEnd covers copy-mode-vi, but mode-keys defaults to\n\
         # emacs, so a mouse drag lands on the emacs table and the selection\n\
         # never reaches the system clipboard.\n\
         set -g copy-command \"{clipper}\"\n\
         bind -T copy-mode-vi y send -X copy-pipe-and-cancel \"{clipper}\"\n\
         bind -T copy-mode-vi MouseDragEnd1Pane send -X copy-pipe-and-cancel \"{clipper}\"\n\
         bind -T copy-mode M-w send -X copy-pipe-and-cancel \"{clipper}\"\n\
         bind -T copy-mode MouseDragEnd1Pane send -X copy-pipe-and-cancel \"{clipper}\"\n\
         # Size a session to whichever client last used it. `smallest` — the\n\
         # obvious choice for not clipping rows — pins a session to the\n\
         # narrowest client that ever attached: leave a split-pane window open\n\
         # at home and a laptop elsewhere can never grow past it, because the\n\
         # idle client still counts. `latest` follows the machine you're\n\
         # actually typing on.\n\
         set -g window-size latest\n\
         # Scrollback depth. The default 2000 is thin for agent work: a single\n\
         # build log or test run can bury everything before it, and the phone\n\
         # app's log view can only show what the buffer still holds.\n\
         set -g history-limit 50000\n\
         # amux session switcher (Ghostty Shift+Cmd+O → ESC o). Native choose-tree:\n\
         # j/k or ↑/↓ move, Enter switches, q/Esc cancels. We can't use an fzf\n\
         # popup here — rmux's display-popup has no client context (client_name\n\
         # etc. expand empty), so switch-client from the popup can't move the\n\
         # right client on a multi-client server. choose-tree switches natively.\n\
         bind -n M-o choose-tree -Zs\n\
         {END}"
    )
}

/// Render Ghostty keybinding block (Shift+Cmd+O sends ESC o to the multiplexer).
pub fn render_ghostty_block() -> String {
    format!(
        "{BEGIN}\n\
         # Shift+Cmd+O → send ESC o to the multiplexer (amux session switcher).\n\
         # Lowercase `o`: ESC-O (uppercase) is the SS3 introducer and gets eaten.\n\
         keybind = shift+super+o=text:\\x1bo\n\
         {END}"
    )
}

/// Remove any existing managed block (between markers, inclusive) from `existing`.
pub fn remove_block(existing: &str) -> String {
    let (Some(start), Some(end_line_start)) = (existing.find(BEGIN), existing.find(END)) else {
        return existing.to_string();
    };
    let end = end_line_start + END.len();
    // also consume a trailing newline after END if present
    let mut after = end;
    if existing[after..].starts_with('\n') {
        after += 1;
    }
    let mut before = start;
    // consume a preceding newline before BEGIN to avoid leaving a blank gap
    if before > 0 && existing[..before].ends_with('\n') {
        before -= 1;
    }
    format!("{}{}", &existing[..before], &existing[after..])
}

/// Insert or replace the managed block in `existing`, returning the new content.
pub fn upsert_block(existing: &str, block: &str) -> String {
    let without = remove_block(existing);
    let trimmed = without.trim_end();
    if trimmed.is_empty() {
        format!("{block}\n")
    } else {
        format!("{trimmed}\n\n{block}\n")
    }
}

/// Pick the user's interactive rc file based on $SHELL.
pub fn rc_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let shell = std::env::var("SHELL").unwrap_or_default();
    if shell.contains("zsh") {
        Some(home.join(".zshrc"))
    } else {
        Some(home.join(".bashrc"))
    }
}

/// Pick the rmux config file to write keybindings into
/// (`~/.config/rmux/rmux.conf`).
pub fn mux_conf_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".config/rmux/rmux.conf"))
}

/// Pick the Ghostty config file path.
/// macOS: `~/Library/Application Support/com.mitchellh.ghostty/config`
/// Other: `~/.config/ghostty/config`
pub fn ghostty_config_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    if cfg!(target_os = "macos") {
        let p = home.join("Library/Application Support/com.mitchellh.ghostty/config");
        if p.exists() {
            return Some(p);
        }
    }
    let p = home.join(".config/ghostty/config");
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

/// Write a managed block into the file at `path`, creating it (and parent
/// directories) if absent.
pub fn install_block(path: &Path, block: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let updated = upsert_block(&existing, block);
    std::fs::write(path, updated).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Write the shell alias managed block into the rc file at `path`.
pub fn install_to(path: &Path, agents: &[Agent]) -> Result<()> {
    install_block(path, &render_block(agents))
}

fn shell_double_quoted(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`");
    format!("\"{escaped}\"")
}

pub fn hook_script_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".amux/hooks/amux-hook"))
}

fn claude_settings_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".claude/settings.json"))
}

fn codex_config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".codex/config.toml"))
}

pub fn render_hook_script(amux_bin: &Path) -> String {
    let amux_bin = shell_double_quoted(&amux_bin.display().to_string());
    format!(
        "#!/bin/sh\n\
         set -eu\n\
         event=\"${{1:-generic}}\"\n\
         cat >/dev/null || true\n\
         case \"$event\" in\n\
           *notification*|*Notification*) state=\"waiting\" ;;\n\
           *fail*|*failed*|*error*) state=\"failed\" ;;\n\
           *start*|*started*|*running*) state=\"running\" ;;\n\
           *) state=\"done\" ;;\n\
         esac\n\
         pane=\"${{TMUX_PANE:-}}\"\n\
         session=\"${{AMUX_SESSION:-}}\"\n\
         if [ -z \"$session\" ] && [ -n \"$pane\" ]; then\n\
           mux=\"${{AMUX_MUX:-rmux}}\"\n\
           session=\"$($mux display-message -p -t \"$pane\" '#{{session_name}}' 2>/dev/null || true)\"\n\
           if [ -z \"$session\" ] && command -v tmux >/dev/null 2>&1; then\n\
             session=\"$(tmux display-message -p -t \"$pane\" '#{{session_name}}' 2>/dev/null || true)\"\n\
           fi\n\
         fi\n\
         if [ -z \"$pane\" ] && [ -z \"$session\" ]; then\n\
           exit 0\n\
         fi\n\
         amux_bin=\"${{AMUX_BIN:-}}\"\n\
         if [ -z \"$amux_bin\" ]; then\n\
           amux_bin={amux_bin}\n\
         fi\n\
         if [ ! -x \"$amux_bin\" ] && command -v amux >/dev/null 2>&1; then\n\
           amux_bin=\"$(command -v amux)\"\n\
         fi\n\
         args=\"$state --source $event --message $event\"\n\
         if [ -n \"$pane\" ]; then\n\
           exec \"$amux_bin\" hook $args --pane \"$pane\"\n\
         fi\n\
         exec \"$amux_bin\" hook $args --session \"$session\"\n"
    )
}

pub fn install_hook_script(path: &Path, amux_bin: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, render_hook_script(amux_bin))
        .with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

fn hook_command(command: &Path, arg: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "command",
        "command": command.display().to_string(),
        "args": [arg],
        "timeout": 5,
    })
}

fn upsert_claude_event_hook(
    hooks: &mut serde_json::Map<String, serde_json::Value>,
    event: &str,
    command: &Path,
    arg: &str,
) {
    let groups = hooks
        .entry(event.to_string())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    if !groups.is_array() {
        *groups = serde_json::Value::Array(Vec::new());
    }
    let groups = groups.as_array_mut().expect("groups is array");
    let exists = groups.iter().any(|group| {
        group
            .get("hooks")
            .and_then(|value| value.as_array())
            .map(|items| {
                items.iter().any(|item| {
                    item.get("command").and_then(|value| value.as_str())
                        == Some(&command.display().to_string())
                        && item
                            .get("args")
                            .and_then(|value| value.as_array())
                            .and_then(|args| args.first())
                            .and_then(|value| value.as_str())
                            == Some(arg)
                })
            })
            .unwrap_or(false)
    });
    if !exists {
        groups.push(serde_json::json!({ "hooks": [hook_command(command, arg)] }));
    }
}

pub fn install_claude_hooks(path: &Path, command: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut root = match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str::<serde_json::Value>(&text)
            .with_context(|| format!("parsing {}", path.display()))?,
        _ => serde_json::json!({}),
    };
    if !root.is_object() {
        root = serde_json::json!({});
    }
    let object = root.as_object_mut().expect("root is object");
    let hooks = object
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    if !hooks.is_object() {
        *hooks = serde_json::json!({});
    }
    let hooks = hooks.as_object_mut().expect("hooks is object");
    upsert_claude_event_hook(hooks, "Stop", command, "claude-stop");
    upsert_claude_event_hook(hooks, "Notification", command, "claude-notification");
    let text = serde_json::to_string_pretty(&root)?;
    std::fs::write(path, format!("{text}\n"))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn remove_marked_block(existing: &str, begin: &str, end_marker: &str) -> String {
    let (Some(start), Some(end_line_start)) = (existing.find(begin), existing.find(end_marker)) else {
        return existing.to_string();
    };
    let end = end_line_start + end_marker.len();
    let mut after = end;
    if existing[after..].starts_with('\n') {
        after += 1;
    }
    let mut before = start;
    if before > 0 && existing[..before].ends_with('\n') {
        before -= 1;
    }
    format!("{}{}", &existing[..before], &existing[after..])
}

fn has_top_level_notify(toml: &str) -> bool {
    toml.lines()
        .take_while(|line| !line.trim_start().starts_with('['))
        .any(|line| line.trim_start().starts_with("notify"))
}

pub fn render_codex_notify_block(command: &Path) -> String {
    let command = command.display().to_string().replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "{CODEX_NOTIFY_BEGIN}\n\
         notify = [\"{command}\", \"codex-notify\"]\n\
         {CODEX_NOTIFY_END}"
    )
}

pub fn upsert_codex_notify_block(existing: &str, command: &Path) -> Option<String> {
    let without = remove_marked_block(existing, CODEX_NOTIFY_BEGIN, CODEX_NOTIFY_END);
    if has_top_level_notify(&without) {
        return None;
    }
    let block = render_codex_notify_block(command);
    let insertion = without
        .lines()
        .position(|line| line.trim_start().starts_with('['))
        .map(|idx| without.lines().take(idx).collect::<Vec<_>>().join("\n"));
    match insertion {
        Some(prefix) => {
            let rest = without.lines().skip(prefix.lines().count()).collect::<Vec<_>>().join("\n");
            let mut out = String::new();
            if !prefix.trim().is_empty() {
                out.push_str(prefix.trim_end());
                out.push_str("\n\n");
            }
            out.push_str(&block);
            if !rest.trim().is_empty() {
                out.push_str("\n\n");
                out.push_str(rest.trim_start_matches('\n'));
            }
            out.push('\n');
            Some(out)
        }
        None => {
            let trimmed = without.trim_end();
            Some(if trimmed.is_empty() {
                format!("{block}\n")
            } else {
                format!("{trimmed}\n\n{block}\n")
            })
        }
    }
}

pub fn install_codex_notify(path: &Path, command: &Path) -> Result<bool> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let Some(updated) = upsert_codex_notify_block(&existing, command) else {
        return Ok(false);
    };
    std::fs::write(path, updated).with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

fn install_agent_hooks_with_bin(amux_bin: &Path) -> Result<Vec<String>> {
    let hook_path = hook_script_path().context("cannot determine amux hook path")?;
    install_hook_script(&hook_path, &amux_bin)?;
    let mut messages = vec![format!("Installed amux hook helper into {}", hook_path.display())];

    if let Some(path) = claude_settings_path() {
        install_claude_hooks(&path, &hook_path)?;
        messages.push(format!("Installed Claude Code hooks into {}", path.display()));
    }
    if let Some(path) = codex_config_path() {
        if install_codex_notify(&path, &hook_path)? {
            messages.push(format!("Installed Codex notify hook into {}", path.display()));
        } else {
            messages.push(format!(
                "Skipped Codex notify hook: top-level notify already exists in {}",
                path.display()
            ));
        }
    }
    Ok(messages)
}

pub fn install_agent_hooks() -> Result<Vec<String>> {
    let amux_bin = std::env::current_exe()
        .context("cannot find current executable")?
        .canonicalize()
        .unwrap_or_else(|_| std::env::current_exe().expect("current executable"));
    install_agent_hooks_with_bin(&amux_bin)
}

/// Where CLI binaries + shims are installed so a terminal can use amux/rmux/cc/cx.
fn cli_bin_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        dirs::data_local_dir().map(|d| d.join("amux").join("bin"))
    }
    #[cfg(not(windows))]
    {
        dirs::home_dir().map(|h| h.join(".local").join("bin"))
    }
}

/// First `name` found on PATH.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// Symlink (unix) or copy (windows) `src` → `dst`, replacing any existing file.
fn link_or_copy(src: &Path, dst: &Path) -> Result<()> {
    // `src` may already be the file we're about to overwrite — rmux is found
    // via PATH, which can resolve to the install directory itself. Removing
    // `dst` first would delete the source, leaving a symlink pointing at
    // nothing (or, for a same-named link, at itself: ELOOP).
    if same_file(src, dst) {
        return Ok(());
    }
    let _ = std::fs::remove_file(dst);
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(src, dst)
            .with_context(|| format!("linking {}", dst.display()))?;
    }
    #[cfg(windows)]
    {
        std::fs::copy(src, dst).with_context(|| format!("copying to {}", dst.display()))?;
    }
    Ok(())
}

/// True when both paths name the same file on disk. Compares canonicalized
/// paths so a symlink and its target count as one — that's exactly the case
/// that would otherwise destroy the binary.
fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        // `b` not existing yet is the normal first-install path.
        _ => false,
    }
}

/// On Windows, download the latest rmux release binary from GitHub when it
/// isn't already on the machine, so `amux install-cli` is genuinely
/// self-contained. Fails gracefully on network or extraction errors — the
/// caller prints a manual-install fallback.
#[cfg(windows)]
fn download_rmux_windows(dest: &Path) -> Result<()> {
    let version = std::env::var("AMUX_RMUX_VERSION")
        .unwrap_or_else(|_| "0.8.0".to_string());
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        a => anyhow::bail!("unsupported architecture: {a} (no rmux binary for it)"),
    };
    let name = format!("rmux-{version}-windows-{arch}");
    let url = format!(
        "{github}https://github.com/helvesec/rmux/releases/download/v{version}/{name}.zip",
        github = std::env::var("AMUX_GITHUB_PROXY")
            .ok()
            .filter(|v| !v.is_empty())
            .map(|p| format!("{p}/"))
            .unwrap_or_default(),
    );
    let tmp = tempfile::tempdir().context("creating temp dir for rmux download")?;
    let zip = tmp.path().join(format!("{name}.zip"));
    let extract = tmp.path().join("rmux-extracted");

    println!("  -> {url}");
    let resp = reqwest::blocking::get(&url)
        .with_context(|| format!("downloading {url}"))?
        .error_for_status()
        .with_context(|| format!("HTTP error from {url}"))?;
    let bytes = resp
        .bytes()
        .with_context(|| format!("reading {url}"))?;
    std::fs::write(&zip, &bytes).context("writing rmux zip to temp")?;

    // PowerShell's Expand-Archive is present on every supported Windows
    // release; no extra tooling needed.
    std::fs::create_dir(&extract).context("creating extract dir")?;
    let status = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "Expand-Archive -Path '{}' -DestinationPath '{}'",
                zip.display(),
                extract.display()
            ),
        ])
        .status()
        .context("running Expand-Archive (is PowerShell available?)")?;
    if !status.success() {
        anyhow::bail!("Expand-Archive failed (corrupt download or disk full?)");
    }

    // The zip typically contains `rmux-{version}-windows-{arch}/rmux.exe`.
    let exe = find_in(&extract, "rmux.exe").with_context(|| {
        format!(
            "rmux.exe not found after extraction — the release layout may have changed;\n\
             extracted to {}",
            extract.display()
        )
    })?;
    std::fs::copy(&exe, dest)
        .with_context(|| format!("copying rmux.exe to {}", dest.display()))?;
    Ok(())
}

/// Recursively search a directory tree for a file with the given name.
#[cfg(windows)]
fn find_in(dir: &Path, name: &str) -> Option<PathBuf> {
    use std::fs;
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        if p.is_dir() {
            if let Some(found) = find_in(&p, name) {
                return Some(found);
            }
        } else if p.file_name().and_then(|n| n.to_str()) == Some(name) {
            return Some(p);
        }
    }
    None
}

/// Make amux + rmux and the `cc`/`cx` shortcuts available to a terminal, from a
/// bundled or installed binary. Idempotent.
pub fn install_cli(agents: &[Agent]) -> Result<()> {
    let exe = std::env::current_exe().context("cannot find current executable")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let exe_dir = exe.parent().map(Path::to_path_buf);

    let rmux_name = if cfg!(windows) { "rmux.exe" } else { "rmux" };
    let amux_name = if cfg!(windows) { "amux.exe" } else { "amux" };

    // rmux: prefer a sibling of amux (bundled apps ship them together), else PATH.
    let rmux = exe_dir
        .as_ref()
        .map(|d| d.join(rmux_name))
        .filter(|p| p.exists())
        .or_else(|| which(rmux_name));

    let bindir = cli_bin_dir().context("cannot determine an install directory")?;
    std::fs::create_dir_all(&bindir)
        .with_context(|| format!("creating {}", bindir.display()))?;

    link_or_copy(&exe, &bindir.join(amux_name))?;
    println!("Installed amux → {}", bindir.join(amux_name).display());
    match &rmux {
        Some(r) => {
            link_or_copy(r, &bindir.join(rmux_name))?;
            println!("Installed rmux → {}", bindir.join(rmux_name).display());
        }
        None => {
            #[cfg(windows)]
            {
                println!("rmux not found — downloading from GitHub releases…");
                match download_rmux_windows(&bindir.join(rmux_name)) {
                    Ok(()) => println!(
                        "Installed rmux → {}",
                        bindir.join(rmux_name).display()
                    ),
                    Err(e) => eprintln!(
                        "Failed to download rmux: {e}\n\
                         Install it manually from https://rmux.io"
                    ),
                }
            }
            #[cfg(not(windows))]
            {
                eprintln!(
                    "note: rmux not found next to amux or on PATH — install it from https://rmux.io"
                )
            }
        },
    }

    #[cfg(not(windows))]
    {
        if let Some(rc) = rc_path() {
            install_to(&rc, agents)?;
            println!("Installed cc/cx aliases into {}", rc.display());
        }
        match install_agent_hooks_with_bin(&bindir.join(amux_name)) {
            Ok(messages) => {
                for message in messages {
                    println!("{message}");
                }
            }
            Err(e) => eprintln!("Skipped agent hooks: {e}"),
        }
        let on_path = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).any(|d| d == bindir))
            .unwrap_or(false);
        if !on_path {
            eprintln!(
                "note: {} is not on PATH — add `export PATH=\"{}:$PATH\"` to your shell rc.",
                bindir.display(),
                bindir.display()
            );
        }
        println!("Done. Open a new terminal (or `source` your rc) to use amux, rmux, cc, cx.");
    }
    #[cfg(windows)]
    {
        // `<alias>.cmd` shims so `cc`/`cx` work in cmd/PowerShell.
        let amux_path = bindir.join(amux_name);
        for a in agents {
            let shim = bindir.join(format!("{}.cmd", a.alias));
            std::fs::write(&shim, format!("@\"{}\" run {} %*\r\n", amux_path.display(), a.name))
                .with_context(|| format!("writing {}", shim.display()))?;
        }
        // Add bindir to the user PATH if absent (persisted for new terminals).
        let dir = bindir.display().to_string();
        let ps = format!(
            "$d='{dir}'; $p=[Environment]::GetEnvironmentVariable('PATH','User'); \
             if(-not (($p -split ';') -contains $d)){{ \
               [Environment]::SetEnvironmentVariable('PATH', ($p.TrimEnd(';') + ';' + $d), 'User') }}"
        );
        let _ = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &ps])
            .status();
        println!("Done. Open a new terminal to use amux, rmux, cc, cx.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn installing_a_binary_over_itself_leaves_it_intact() {
        // rmux is located via PATH, which can resolve to the very directory we
        // install into. Removing dst first would then delete the source and
        // leave a symlink pointing at itself (ELOOP) — the binary is gone.
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("rmux");
        std::fs::write(&bin, b"#!/bin/sh\ntrue\n").unwrap();

        link_or_copy(&bin, &bin).unwrap();

        assert!(bin.exists(), "the binary must survive");
        assert!(!bin.is_symlink(), "must not become a link to itself");
        assert_eq!(std::fs::read(&bin).unwrap(), b"#!/bin/sh\ntrue\n");
    }

    #[cfg(unix)]
    #[test]
    fn installing_through_a_symlink_to_itself_is_also_a_no_op() {
        // The same hazard one level of indirection away: PATH found a symlink
        // that already points at the destination.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("rmux-0.8.0");
        let link = dir.path().join("rmux");
        std::fs::write(&real, b"binary").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        link_or_copy(&link, &real).unwrap();

        assert!(real.exists(), "the real binary must survive");
        assert_eq!(std::fs::read(&real).unwrap(), b"binary");
    }

    #[cfg(unix)]
    #[test]
    fn normal_install_still_links() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src-bin");
        let dst = dir.path().join("bin");
        std::fs::write(&src, b"binary").unwrap();

        link_or_copy(&src, &dst).unwrap();

        assert!(dst.is_symlink());
        assert_eq!(std::fs::read(&dst).unwrap(), b"binary");
    }

    fn agents() -> Vec<Agent> {
        vec![
            Agent { name: "claude".into(), alias: "cc".into(), command: vec!["claude".into()] },
            Agent { name: "codex".into(), alias: "cx".into(), command: vec!["codex".into()] },
        ]
    }

    #[test]
    fn render_contains_aliases_and_markers() {
        let b = render_block(&agents());
        assert!(b.starts_with(BEGIN));
        assert!(b.trim_end().ends_with(END));
        assert!(b.contains("alias cc='amux run claude'"));
        assert!(b.contains("alias cx='amux run codex'"));
    }

    #[test]
    fn render_mux_block_contains_keybinding() {
        let b = render_mux_block();
        assert!(b.starts_with(BEGIN));
        assert!(b.trim_end().ends_with(END));
        assert!(b.contains("bind -n M-o choose-tree -Zs"));
        assert!(b.contains("set -g window-size latest"));
    }

    #[test]
    fn mux_block_copies_to_the_clipboard_in_both_mode_tables() {
        // Regression: only copy-mode-vi was bound, but mode-keys defaults to
        // emacs, so a mouse drag hit the emacs table's bare
        // copy-pipe-and-cancel and the text stopped at rmux's own buffer
        // instead of reaching the system clipboard.
        let clipper = if cfg!(windows) { "clip" } else { "pbcopy" };
        let b = render_mux_block();
        assert!(
            b.contains(&format!("set -g copy-command \"{clipper}\"")),
            "copy-command is what covers the bindings we don't restate"
        );
        for table in ["copy-mode-vi", "copy-mode"] {
            assert!(
                b.contains(&format!(
                    "bind -T {table} MouseDragEnd1Pane send -X copy-pipe-and-cancel \"{clipper}\""
                )),
                "missing mouse copy binding for {table}"
            );
        }
    }

    #[test]
    fn render_ghostty_block_contains_keybinding() {
        let b = render_ghostty_block();
        assert!(b.starts_with(BEGIN));
        assert!(b.trim_end().ends_with(END));
        assert!(b.contains("keybind = shift+super+o=text:\\x1bo"));
    }

    #[test]
    fn render_hook_script_calls_amux_hook() {
        let script = render_hook_script(Path::new("/tmp/amux"));
        assert!(script.contains("amux_bin=\"${AMUX_BIN:-}\""));
        assert!(script.contains("amux_bin=\"/tmp/amux\""));
        assert!(script.contains("[ ! -x \"$amux_bin\" ] && command -v amux"));
        assert!(script.contains("hook $args --pane"));
        assert!(script.contains("codex-notify") == false);
    }

    #[test]
    fn upsert_into_empty() {
        let out = upsert_block("", &render_block(&agents()));
        assert!(out.contains("alias cc="));
        assert!(out.starts_with(BEGIN));
    }

    #[test]
    fn upsert_is_idempotent() {
        let block = render_block(&agents());
        let once = upsert_block("export FOO=1\n", &block);
        let twice = upsert_block(&once, &block);
        assert_eq!(once, twice);
        // user content preserved exactly once
        assert_eq!(once.matches("export FOO=1").count(), 1);
        assert_eq!(once.matches(BEGIN).count(), 1);
    }

    #[test]
    fn upsert_mux_block_is_idempotent() {
        let block = render_mux_block();
        let existing = "set -g mouse on\n";
        let once = upsert_block(existing, &block);
        let twice = upsert_block(&once, &block);
        assert_eq!(once, twice);
        assert!(once.contains("set -g mouse on"));
        assert_eq!(once.matches(BEGIN).count(), 1);
    }

    #[test]
    fn remove_restores_user_content() {
        let block = render_block(&agents());
        let with = upsert_block("export FOO=1\n", &block);
        let without = remove_block(&with);
        assert!(!without.contains(BEGIN));
        assert!(without.contains("export FOO=1"));
    }

    #[test]
    fn install_to_creates_and_updates_file() {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join(".zshrc");
        install_to(&rc, &agents()).unwrap();
        let content = std::fs::read_to_string(&rc).unwrap();
        assert!(content.contains("alias cc='amux run claude'"));
        // second install does not duplicate
        install_to(&rc, &agents()).unwrap();
        let content2 = std::fs::read_to_string(&rc).unwrap();
        assert_eq!(content2.matches(BEGIN).count(), 1);
    }

    #[test]
    fn install_block_creates_and_updates_file() {
        let dir = tempfile::tempdir().unwrap();
        // nested path exercises parent-dir creation (like ~/.config/rmux/)
        let conf = dir.path().join("rmux/rmux.conf");
        let block = render_mux_block();
        install_block(&conf, &block).unwrap();
        let content = std::fs::read_to_string(&conf).unwrap();
        assert!(content.contains("bind -n M-o"));
        // idempotent
        install_block(&conf, &block).unwrap();
        let content2 = std::fs::read_to_string(&conf).unwrap();
        assert_eq!(content2.matches(BEGIN).count(), 1);
    }

    #[test]
    fn install_claude_hooks_merges_settings() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        std::fs::write(&settings, r#"{"env":{"A":"B"}}"#).unwrap();
        install_claude_hooks(&settings, Path::new("/tmp/amux-hook")).unwrap();
        install_claude_hooks(&settings, Path::new("/tmp/amux-hook")).unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(value["env"]["A"], "B");
        assert_eq!(value["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert_eq!(value["hooks"]["Notification"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn upsert_codex_notify_block_is_top_level_and_idempotent() {
        let existing = r#"model = "gpt-5"

[projects."/tmp"]
trust_level = "trusted"
"#;
        let once = upsert_codex_notify_block(existing, Path::new("/tmp/amux-hook")).unwrap();
        let twice = upsert_codex_notify_block(&once, Path::new("/tmp/amux-hook")).unwrap();
        assert_eq!(once, twice);
        let notify = once.find("notify =").unwrap();
        let projects = once.find("[projects").unwrap();
        assert!(notify < projects);
        assert_eq!(once.matches(CODEX_NOTIFY_BEGIN).count(), 1);
    }

    #[test]
    fn upsert_codex_notify_block_does_not_override_user_notify() {
        let existing = "notify = [\"custom\"]\n\n[projects]\n";
        assert!(upsert_codex_notify_block(existing, Path::new("/tmp/amux-hook")).is_none());
    }
}
