use crate::config::Agent;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

const BEGIN: &str = "# >>> amux managed block >>>";
const END: &str = "# <<< amux managed block <<<";

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

/// Render the rmux keybinding block (fzf session switcher on M-O).
pub fn render_mux_block() -> String {
    format!(
        "{BEGIN}\n\
         # amux session switcher (triggered by Ghostty Shift+Cmd+O → ESC O)\n\
         bind -n M-O display-popup -E \"rmux list-sessions -F '#{{session_name}}' | grep -E '_[a-f0-9]{{8}}$' | fzf --reverse --header='switch session' | xargs rmux switch-client -t\"\n\
         {END}"
    )
}

/// Render Ghostty keybinding block (Shift+Cmd+O sends ESC O to the multiplexer).
pub fn render_ghostty_block() -> String {
    format!(
        "{BEGIN}\n\
         # Shift+Cmd+O → send ESC O to the multiplexer (amux session switcher)\n\
         keybind = shift+super+o=text:\\x1bO\n\
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(b.contains("bind -n M-O display-popup"));
        assert!(b.contains("fzf"));
        assert!(b.contains("rmux switch-client"));
    }

    #[test]
    fn render_ghostty_block_contains_keybinding() {
        let b = render_ghostty_block();
        assert!(b.starts_with(BEGIN));
        assert!(b.trim_end().ends_with(END));
        assert!(b.contains("keybind = shift+super+o=text:\\x1bO"));
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
        assert!(content.contains("bind -n M-O"));
        // idempotent
        install_block(&conf, &block).unwrap();
        let content2 = std::fs::read_to_string(&conf).unwrap();
        assert_eq!(content2.matches(BEGIN).count(), 1);
    }
}
