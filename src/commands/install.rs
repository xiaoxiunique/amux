use anyhow::{bail, Context, Result};
use std::process::Command;

/// Ensure the machine has Claude Code, Codex CLI, and rmux available.
///
/// Detects each one; installs whatever is missing via the canonical package
/// manager for this platform, preferring `bun` over `npm` when both exist.
pub fn install_agents(agents: &[crate::config::Agent]) -> Result<()> {
    let pkg_mgr = detect_package_manager();

    let mut missing = Vec::new();
    // The configured agents are the ones amux knows how to launch — we ensure
    // each is on PATH so `cc`/`cx` won't fail at the first `send-keys`.
    for agent in agents {
        if which(&agent.name).is_some() {
            println!("✅ {:<8} already installed", agent.name);
            continue;
        }
        missing.push(agent.name.clone());
    }

    if missing.is_empty() {
        println!("All agents are present.");
        return Ok(());
    }

    println!(
        "Installing with {}:",
        match pkg_mgr {
            PkgMgr::Bun => "bun",
            PkgMgr::Npm => "npm",
            PkgMgr::Brew => "brew",
        }
    );

    for name in &missing {
        let (npm_pkg, brew_formula) = match name.as_str() {
            "claude" => ("@anthropic-ai/claude-code", Some("claude-cli")),
            "codex" => ("@openai/codex", Some("codex")),
            _ => continue,
        };

        let result = match pkg_mgr {
            PkgMgr::Bun => Command::new("bun")
                .args(["install", "-g", npm_pkg])
                .status(),
            PkgMgr::Npm => Command::new("npm")
                .args(["install", "-g", npm_pkg])
                .status(),
            PkgMgr::Brew => {
                let f = brew_formula.unwrap_or(npm_pkg);
                Command::new("brew").args(["install", f]).status()
            }
        };

        match result {
            Ok(s) if s.success() => println!("✅ {name} installed"),
            Ok(_) => eprintln!("⚠ {name} install exited with an error"),
            Err(e) => eprintln!("⚠ {name}: {e}"),
        }
    }

    // Verify every agent that was missing is now reachable.
    let mut still_missing = false;
    for name in &missing {
        match which(name) {
            Some(_) => {} // resolved
            None => {
                eprintln!(
                    "✗ {name} 仍然不在 PATH 中 — 请手动安装:\n\
                       npm install -g @anthropic-ai/{name}-code  (Claude Code)\n\
                       npm install -g @openai/codex               (Codex)"
                );
                still_missing = true;
            }
        }
    }
    if still_missing {
        bail!("some agents could not be installed");
    }
    Ok(())
}

enum PkgMgr {
    Bun,
    Npm,
    Brew,
}

/// Pick the package manager: bun > brew > npm, preferring the faster tool
/// when available and the system one otherwise.
fn detect_package_manager() -> PkgMgr {
    if which("bun").is_some() {
        PkgMgr::Bun
    } else if cfg!(target_os = "macos") && which("brew").is_some() {
        PkgMgr::Brew
    } else {
        PkgMgr::Npm
    }
}

fn which(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}
