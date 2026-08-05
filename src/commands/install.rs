use anyhow::{bail, Context, Result};
use std::process::Command;

/// Ensure the machine has Claude Code, Codex CLI, and rmux available.
///
/// Detects each one; installs whatever is missing via the canonical package
/// manager for this platform, preferring `bun` over `npm` when both exist.
pub fn install_agents(agents: &[crate::config::Agent]) -> Result<()> {
    let pkg_mgr = ensure_package_manager()?;

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

/// Pick the package manager: bun > brew > npm. If nothing is available at
/// all, try to bootstrap bun via its zero-dependency install script
/// (`curl -fsSL https://bun.sh/install | bash`) and retry.
fn ensure_package_manager() -> Result<PkgMgr> {
    if which("bun").is_some() {
        return Ok(PkgMgr::Bun);
    }
    if cfg!(target_os = "macos") && which("brew").is_some() {
        return Ok(PkgMgr::Brew);
    }
    if which("npm").is_some() {
        return Ok(PkgMgr::Npm);
    }

    println!("No package manager found — installing bun (one binary, zero deps)…");

    // Bun's installer writes to ~/.bun and adds it to the current shell's rc
    // file. We can't re-source the rc from here, but we know where the binary
    // lands and can run it directly.
    let status = Command::new("bash")
        .arg("-c")
        .arg("curl -fsSL https://bun.sh/install | bash")
        .status()
        .context("running bun installer")?;

    if !status.success() {
        bail!("bun install failed. Install a package manager manually (brew, bun, npm) and try again.");
    }

    // The installer puts bun at ~/.bun/bin/bun.
    let bun_path = dirs::home_dir()
        .map(|h| h.join(".bun/bin/bun"))
        .filter(|p| p.is_file());

    // On Windows the path differs, but bun on Windows is experimental; the
    // user would have npm via Node.js installer anyway. Keep trying.
    if bun_path.is_some() {
        return Ok(PkgMgr::Bun);
    }

    // Give it one more try — the shell may have refreshed.
    if which("bun").is_some() {
        Ok(PkgMgr::Bun)
    } else {
        bail!("bun binary not found after install — add ~/.bun/bin to your PATH and re-run")
    }
}

fn which(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}
