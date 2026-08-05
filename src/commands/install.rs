use anyhow::{bail, Context, Result};
use std::process::Command;

/// Common Chinese npm registry, used by `--china`. Hardcoded rather than
/// fetched from a dynamic config because this mirror has been stable for
/// years and the alternative (a dead `amux.cc/mirrors.json` lookup) gates
/// the very install step that would be needed to fetch it.
const CN_REGISTRY: &str = "https://registry.npmmirror.com";

/// Mirror prefix for GitHub downloads, e.g. `https://gh.api.99988866.xyz/`.
/// Used by `--china` as a best-effort default.
const CN_GITHUB_PROXY: &str = "https://gh.api.99988866.xyz";

/// Ensure the machine has Claude Code, Codex CLI, and rmux available.
///
/// Detects each one; installs whatever is missing via the canonical package
/// manager for this platform, preferring `bun` over `npm` when both exist.
pub fn install_agents(agents: &[crate::config::Agent], china: bool) -> Result<()> {
    // Registry override, either from --china or an explicit env var.
    // The env var takes precedence so a user can use a private mirror
    // without typing it every time.
    let registry: Option<String> = std::env::var("AMUX_REGISTRY")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| china.then(|| CN_REGISTRY.to_string()));
    let _github_proxy: Option<String> = std::env::var("AMUX_GITHUB_PROXY")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| china.then(|| CN_GITHUB_PROXY.to_string()));

    if let Some(ref r) = registry {
        println!("using registry: {r}");
    }

    let pkg_mgr = ensure_package_manager(china)?;

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
            PkgMgr::Bun => {
                let mut cmd = Command::new("bun");
                cmd.arg("install").arg("-g");
                if let Some(ref r) = registry {
                    cmd.arg("--registry").arg(r);
                }
                cmd.arg(npm_pkg).status()
            }
            PkgMgr::Npm => {
                let mut cmd = Command::new("npm");
                cmd.arg("install").arg("-g");
                if let Some(ref r) = registry {
                    cmd.arg("--registry").arg(r);
                }
                cmd.arg(npm_pkg).status()
            }
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
fn ensure_package_manager(china: bool) -> Result<PkgMgr> {
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
    // With --china, set BUN_INSTALL so the install script uses the npm mirror
    // for the binary download rather than hitting GitHub Releases directly.
    let mut cmd = Command::new("bash");
    cmd.arg("-c");
    if china {
        cmd.env(
            "BUN_INSTALL",
            format!("https://bun.sh/install?registry={CN_REGISTRY}"),
        );
    }
    cmd.arg("curl -fsSL https://bun.sh/install | bash");
    let status = cmd.status().context("running bun installer")?;

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
