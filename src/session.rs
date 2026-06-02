use sha2::{Digest, Sha256};
use std::path::Path;

/// Sanitize a directory basename into `[A-Za-z0-9_-]`, collapse runs of `_`,
/// trim leading/trailing `_`, and fall back to "work" when empty.
pub fn slug(base: &str) -> String {
    let mapped: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    let mut collapsed = mapped;
    while collapsed.contains("__") {
        collapsed = collapsed.replace("__", "_");
    }
    let trimmed = collapsed.trim_matches('_').to_string();
    if trimmed.is_empty() { "work".to_string() } else { trimmed }
}

/// First 8 hex chars of SHA-256 of the input.
pub fn hash8(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{:02x}", b)).collect();
    hex[..8].to_string()
}

/// `<alias>_<slug(basename)>_<hash8(abs_path)>`
pub fn session_name(alias: &str, abs_cwd: &Path) -> String {
    let base = abs_cwd
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("root");
    format!("{}_{}_{}", alias, slug(base), hash8(&abs_cwd.to_string_lossy()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn slug_sanitizes_and_collapses() {
        assert_eq!(slug("My Project!!"), "My_Project");
        assert_eq!(slug("a..b"), "a_b");
        assert_eq!(slug("__x__"), "x");
        assert_eq!(slug("///"), "work");
    }

    #[test]
    fn hash8_is_deterministic_and_8_chars() {
        let a = hash8("/Users/me/proj");
        let b = hash8("/Users/me/proj");
        assert_eq!(a, b);
        assert_eq!(a.len(), 8);
        assert_ne!(hash8("/Users/me/proj"), hash8("/Users/me/other"));
    }

    #[test]
    fn session_name_format() {
        let p = PathBuf::from("/Users/me/myproject");
        let name = session_name("cc", &p);
        assert!(name.starts_with("cc_myproject_"));
        assert_eq!(name.matches('_').count() >= 2, true);
    }
}
