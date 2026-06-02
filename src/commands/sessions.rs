use crate::config::Agent;
use crate::tmux;
use anyhow::Result;

/// A parsed amux-managed session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedSession {
    pub name: String,
    pub alias: String,
}

/// Return managed sessions: those whose name begins with `<alias>_` for a known
/// alias and match the `<alias>_<slug>_<8hex>` shape.
pub fn managed_sessions(all: &[String], agents: &[Agent]) -> Vec<ManagedSession> {
    let mut out = Vec::new();
    for name in all {
        for a in agents {
            let prefix = format!("{}_", a.alias);
            if let Some(rest) = name.strip_prefix(&prefix) {
                // rest must end with `_<8 hex>`
                if let Some(idx) = rest.rfind('_') {
                    let tail = &rest[idx + 1..];
                    let is_hash = tail.len() == 8 && tail.chars().all(|c| c.is_ascii_hexdigit());
                    if is_hash && idx > 0 {
                        out.push(ManagedSession { name: name.clone(), alias: a.alias.clone() });
                        break;
                    }
                }
            }
        }
    }
    out
}

pub fn list(agents: &[Agent]) -> Result<()> {
    let all = tmux::list_session_names()?;
    let managed = managed_sessions(&all, agents);
    if managed.is_empty() {
        println!("No amux sessions.");
        return Ok(());
    }
    println!("{:<28} {}", "SESSION", "AGENT");
    for s in managed {
        println!("{:<28} {}", s.name, s.alias);
    }
    Ok(())
}

pub fn kill(name: &str) -> Result<()> {
    tmux::kill_session(name)?;
    println!("killed {name}");
    Ok(())
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
    fn detects_managed_and_ignores_others() {
        let all = vec![
            "cc_myproject_1a2b3c4d".to_string(),
            "cx_api_deadbeef".to_string(),
            "random_session".to_string(),
            "cc_nohash".to_string(),
        ];
        let m = managed_sessions(&all, &agents());
        let names: Vec<_> = m.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"cc_myproject_1a2b3c4d"));
        assert!(names.contains(&"cx_api_deadbeef"));
        assert!(!names.contains(&"random_session"));
        assert!(!names.contains(&"cc_nohash"));
    }
}
