use crate::commands::session_ids::{recent_sessions, PastSession};
use anyhow::{Context, Result};
use std::time::{SystemTime, UNIX_EPOCH};

/// How many sessions per agent to show by default.
const DEFAULT_LIMIT: usize = 5;

/// List recent Claude Code and Codex conversations for the current directory.
///
/// Reads the agents' own session stores rather than anything amux records, so
/// it also sees conversations started outside amux.
pub fn list_sessions(limit: Option<usize>) -> Result<()> {
    let cwd = std::env::current_dir()
        .context("cannot read current directory")?
        .canonicalize()
        .context("cannot canonicalize current directory")?;
    let limit = limit.unwrap_or(DEFAULT_LIMIT);

    println!("{}", cwd.display());

    for (agent, label) in [("claude", "Claude Code"), ("codex", "Codex")] {
        let sessions = recent_sessions(agent, &cwd, limit);
        println!();
        if sessions.is_empty() {
            println!("{label}: 没有会话记录");
            continue;
        }
        println!("{label} (最近 {}):", sessions.len());
        for s in &sessions {
            println!(
                "  {}  {:>8}  {}",
                short_id(&s.id),
                human_size(s.size),
                relative_time(s.modified),
            );
        }
    }
    Ok(())
}

/// First 8 chars — enough to identify a session, and what the agents' own
/// resume UIs display.
fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.0}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / 1024.0 / 1024.0)
    }
}

/// "3分钟前" / "2小时前" / "5天前" — more scannable than a timestamp when the
/// question is "which one was I just in".
fn relative_time(epoch_secs: f64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(epoch_secs);
    let delta = (now - epoch_secs).max(0.0);

    if delta < 60.0 {
        "刚刚".to_string()
    } else if delta < 3600.0 {
        format!("{}分钟前", (delta / 60.0) as u64)
    } else if delta < 86400.0 {
        format!("{}小时前", (delta / 3600.0) as u64)
    } else {
        format!("{}天前", (delta / 86400.0) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_sizes() {
        assert_eq!(human_size(512), "512B");
        assert_eq!(human_size(2048), "2KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0MB");
    }

    #[test]
    fn formats_relative_times() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        assert_eq!(relative_time(now), "刚刚");
        assert_eq!(relative_time(now - 600.0), "10分钟前");
        assert_eq!(relative_time(now - 7200.0), "2小时前");
        assert_eq!(relative_time(now - 3.0 * 86400.0), "3天前");
        // A file dated in the future must not underflow.
        assert_eq!(relative_time(now + 1000.0), "刚刚");
    }

    #[test]
    fn shortens_ids() {
        assert_eq!(short_id("7ec5d280-1234-5678"), "7ec5d280");
        assert_eq!(short_id("abc"), "abc");
    }

    #[allow(dead_code)]
    fn _assert_past_session_shape(s: &PastSession) {
        let _ = (&s.id, &s.path, s.modified, s.size);
    }
}
