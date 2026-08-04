#[allow(
    clippy::all,
    unused_imports,
    dead_code,
    unused_variables,
    unused_mut,
    unreachable_code,
    unused_assignments
)]
pub mod cron;
pub mod files;
pub mod herdr;
pub mod server;

#[cfg(feature = "full")]
#[allow(clippy::all, dead_code)]
pub(crate) mod full;

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const DEFAULT_HOST: &str = "0.0.0.0";
const DEFAULT_PORT: u16 = 8787;

/// ~/.amux/ state directory
fn state_dir() -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .context("cannot determine home directory")?
        .join(".amux");
    if !dir.exists() {
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    Ok(dir)
}

fn pid_file() -> Result<PathBuf> {
    Ok(state_dir()?.join("serve.pid"))
}

fn log_file() -> Result<PathBuf> {
    Ok(state_dir()?.join("serve.log"))
}

/// Check if a process with the given PID is alive.
#[cfg(unix)]
fn is_running(pid: u32) -> bool {
    // kill -0 checks existence without sending a signal
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(windows)]
fn is_running(pid: u32) -> bool {
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
        .unwrap_or(false)
}

/// Ask the process to terminate (graceful), then force-kill.
#[cfg(unix)]
fn signal_terminate(pid: u32) {
    unsafe { libc::kill(pid as i32, libc::SIGTERM); }
}
#[cfg(unix)]
fn signal_kill(pid: u32) {
    unsafe { libc::kill(pid as i32, libc::SIGKILL); }
}

#[cfg(windows)]
fn signal_terminate(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string()])
        .output();
}
#[cfg(windows)]
fn signal_kill(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F", "/T"])
        .output();
}

/// Read PID from file, return None if missing or stale.
fn read_pid() -> Option<u32> {
    let path = pid_file().ok()?;
    let text = fs::read_to_string(&path).ok()?;
    let pid: u32 = text.trim().parse().ok()?;
    if is_running(pid) {
        Some(pid)
    } else {
        // Stale PID file — clean up
        let _ = fs::remove_file(&path);
        None
    }
}

/// Start the agent monitor server.
pub fn serve(
    port: u16,
    host: Option<&str>,
    token: Option<&str>,
    foreground: bool,
    open: bool,
    with_herdr: bool,
) -> Result<()> {
    let host = host.unwrap_or(DEFAULT_HOST);
    let port = if port == 0 { DEFAULT_PORT } else { port };
    let token = token.unwrap_or("");

    if foreground {
        // Run directly in this process with a tokio runtime
        herdr::set_enabled(with_herdr);
        if with_herdr {
            println!("herdr bridge enabled");
        }
        if open {
            open_browser(port);
        }
        let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;
        rt.block_on(server::run_server(host, port, token));
        return Ok(());
    }

    // Check if already running
    if let Some(pid) = read_pid() {
        bail!("Agent monitor is already running (pid: {pid}). Use `amux stop` first.");
    }

    // Daemonize: re-exec ourselves with --foreground
    let exe = std::env::current_exe().context("cannot determine current executable")?;
    let log = log_file()?;
    let log_out = fs::File::create(&log).with_context(|| format!("creating {}", log.display()))?;
    let log_err = log_out.try_clone()?;

    let mut cmd = Command::new(&exe);
    cmd.arg("serve")
        .arg("--foreground")
        .arg("--port")
        .arg(port.to_string());
    if host != DEFAULT_HOST {
        cmd.arg("--host").arg(host);
    }
    if !token.is_empty() {
        cmd.arg("--token").arg(token);
    }
    // The daemon is this binary re-executed, so the flag has to be forwarded
    // or `--herdr` would silently do nothing in the default (daemon) mode.
    if with_herdr {
        cmd.arg("--herdr");
    }

    let child = cmd
        .stdout(log_out)
        .stderr(log_err)
        .stdin(std::process::Stdio::null())
        .spawn()
        .context("failed to spawn daemon")?;

    let pid = child.id();

    // Write PID file
    let pf = pid_file()?;
    fs::write(&pf, pid.to_string()).with_context(|| format!("writing {}", pf.display()))?;

    println!("Agent monitor started on http://{}:{} (pid: {})", host, port, pid);
    println!("Web UI:  http://localhost:{port}");
    println!("Logs: {}", log.display());
    if open {
        open_browser(port);
    }
    Ok(())
}

/// Open the web UI in the user's default browser (best-effort).
fn open_browser(port: u16) {
    let url = format!("http://127.0.0.1:{port}");
    #[cfg(target_os = "macos")]
    let opener: Option<&str> = Some("open");
    #[cfg(target_os = "linux")]
    let opener: Option<&str> = Some("xdg-open");
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let opener: Option<&str> = None;

    match opener {
        Some(cmd) => {
            let _ = Command::new(cmd).arg(&url).spawn();
        }
        None => println!("Open {url} in your browser."),
    }
}

/// Stop the agent monitor daemon.
pub fn stop() -> Result<()> {
    let pf = pid_file()?;

    let Some(pid) = read_pid() else {
        println!("Agent monitor is not running.");
        return Ok(());
    };

    // Ask it to terminate gracefully
    signal_terminate(pid);

    // Wait up to 5 seconds
    for _ in 0..50 {
        if !is_running(pid) {
            let _ = fs::remove_file(&pf);
            println!("Agent monitor stopped (pid: {}).", pid);
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    // Force kill
    signal_kill(pid);
    let _ = fs::remove_file(&pf);
    println!("Agent monitor killed (pid: {}).", pid);
    Ok(())
}
