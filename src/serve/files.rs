//! Read-only file browsing for the phone client.
//!
//! Scope is deliberately narrow: **browse and download, never write**, and only
//! within directories the user already works in. The allowed roots are the
//! project directories amux already knows about — the cwd of every live pane
//! plus the project history — so nothing outside them is reachable even though
//! the server may run without a token.
//!
//! Path checks canonicalize before comparing. A prefix test on the raw string
//! would be defeated by `..` segments or by a symlink inside a project that
//! points at, say, `~/.ssh`.

use serde::Serialize;
use std::path::{Path, PathBuf};

/// Directory names skipped by default. Without this a single project is
/// unusable on a phone: amux's own directory holds 71789 files, but only 64
/// once these are excluded.
const NOISE_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "build",
    ".dart_tool",
    "Pods",
    ".venv",
    "venv",
    "__pycache__",
    ".next",
    "dist",
    ".gradle",
    "DerivedData",
];

/// Upper bound on an inline text preview. Anything larger must be downloaded;
/// holding a huge file in a JSON string helps nobody on a phone.
pub const MAX_PREVIEW_BYTES: u64 = 1024 * 1024;

/// Cap on entries returned for one directory, so a pathological directory
/// can't produce an unbounded response.
const MAX_ENTRIES: usize = 2000;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Root {
    pub path: String,
    pub name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    /// Seconds since the epoch; `None` when the platform won't report it.
    pub modified: Option<f64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Listing {
    pub path: String,
    /// The root this path sits under, so the client can stop "up" navigation
    /// at the boundary instead of walking into a 403.
    pub root: String,
    pub entries: Vec<Entry>,
    /// True when noise directories and dotfiles were filtered out.
    pub filtered: bool,
}

/// Paths that must never become a browsable root, however they got into the
/// pane list or project history.
///
/// The history records wherever an agent was ever started, which in practice
/// included `$HOME`, `~/.claude`, `~/.Trash` and `~/Library/Logs`. Roots absorb
/// their descendants, so a single such entry silently widens the browsable area
/// far beyond "my projects" — `$HOME` alone would expose `~/.ssh`, `~/Library`
/// and every browser profile.
fn is_forbidden_root(p: &Path) -> bool {
    // The filesystem root, and the home directory itself.
    if p.parent().is_none() {
        return true;
    }
    let home = dirs::home_dir();
    if home.as_deref() == Some(p) {
        return true;
    }

    // Anything inside a sensitive or non-project area of the home directory.
    if let Some(home) = home.as_deref() {
        if let Ok(rel) = p.strip_prefix(home) {
            let first = rel.components().next().and_then(|c| c.as_os_str().to_str());
            match first {
                // Dot-directories hold credentials and tool state, not projects
                // (~/.claude, ~/.ssh, ~/.config, ~/.Trash …).
                Some(f) if f.starts_with('.') => return true,
                Some("Library" | "Applications" | "Movies" | "Music" | "Pictures") => {
                    return true
                }
                _ => {}
            }
        }
    }

    // Shared scratch space and system trees: not projects.
    let s = p.to_string_lossy();
    for prefix in ["/tmp", "/private/tmp", "/var/tmp", "/private/var/tmp",
                   "/private/var/folders"] {
        if s == prefix || s.starts_with(&format!("{prefix}/")) {
            return true;
        }
    }
    matches!(
        p.to_str(),
        Some(
            "/Users" | "/Applications" | "/Library" | "/System" | "/private" | "/var" | "/etc"
                | "/opt" | "/usr"
        )
    )
}

/// Directories the client may browse: every live pane's cwd plus the project
/// history. Deduplicated, and nested paths collapse into their ancestor so the
/// same tree isn't offered twice.
pub fn roots(pane_paths: &[String], history_paths: &[String]) -> Vec<Root> {
    let mut all: Vec<PathBuf> = pane_paths
        .iter()
        .chain(history_paths.iter())
        .filter(|p| !p.is_empty())
        .filter_map(|p| Path::new(p).canonicalize().ok())
        .filter(|p| p.is_dir())
        .filter(|p| !is_forbidden_root(p))
        .collect();

    all.sort();
    all.dedup();

    // Drop any path that lives under another one already in the list.
    let mut kept: Vec<PathBuf> = Vec::new();
    for p in all {
        if kept.iter().any(|k| p.starts_with(k)) {
            continue;
        }
        kept.push(p);
    }

    kept.into_iter()
        .map(|p| Root {
            name: p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.to_string_lossy().into_owned()),
            path: p.to_string_lossy().into_owned(),
        })
        .collect()
}

/// Resolve `requested` and confirm it sits inside one of `roots`.
///
/// Canonicalizing first is what makes this safe: it collapses `..` and follows
/// symlinks, so neither can escape a root. Returns the resolved path and the
/// root that contains it.
pub fn resolve_within(
    requested: &str,
    roots: &[Root],
) -> Result<(PathBuf, PathBuf), String> {
    if requested.is_empty() {
        return Err("path is required".to_string());
    }
    let candidate = Path::new(requested)
        .canonicalize()
        .map_err(|_| format!("no such path: {requested}"))?;

    for root in roots {
        let root_path = Path::new(&root.path);
        if candidate.starts_with(root_path) {
            return Ok((candidate, root_path.to_path_buf()));
        }
    }
    Err("path is outside the browsable project directories".to_string())
}

fn is_noise(name: &str, is_dir: bool) -> bool {
    if is_dir && NOISE_DIRS.contains(&name) {
        return true;
    }
    name.starts_with('.')
}

/// List one directory. `show_all` includes dotfiles and the noise directories.
pub fn list(dir: &Path, root: &Path, show_all: bool) -> Result<Listing, String> {
    let read = std::fs::read_dir(dir).map_err(|e| format!("cannot read directory: {e}"))?;

    let mut entries: Vec<Entry> = Vec::new();
    for item in read.flatten() {
        let name = item.file_name().to_string_lossy().into_owned();
        let meta = match item.metadata() {
            Ok(m) => m,
            Err(_) => continue, // broken symlink etc. — skip rather than fail
        };
        let is_dir = meta.is_dir();
        if !show_all && is_noise(&name, is_dir) {
            continue;
        }
        entries.push(Entry {
            path: item.path().to_string_lossy().into_owned(),
            name,
            is_dir,
            size: if is_dir { 0 } else { meta.len() },
            modified: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64()),
        });
        if entries.len() >= MAX_ENTRIES {
            break;
        }
    }

    // Directories first, then case-insensitive by name — the ordering a file
    // browser is expected to have.
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    Ok(Listing {
        path: dir.to_string_lossy().into_owned(),
        root: root.to_string_lossy().into_owned(),
        entries,
        filtered: !show_all,
    })
}

/// Outcome of asking for a text preview.
pub enum Preview {
    Text { content: String, size: u64 },
    /// Not previewable inline; the client should offer a download instead.
    Binary { size: u64, reason: &'static str },
}

/// Read a file for inline preview.
///
/// Binary detection is a UTF-8 check on the bytes actually read: source, logs
/// and config are text by definition, and anything that isn't valid UTF-8 would
/// only render as mojibake.
pub fn preview(file: &Path) -> Result<Preview, String> {
    let meta = std::fs::metadata(file).map_err(|e| format!("cannot stat file: {e}"))?;
    if meta.is_dir() {
        return Err("path is a directory".to_string());
    }
    let size = meta.len();
    if size > MAX_PREVIEW_BYTES {
        return Ok(Preview::Binary { size, reason: "too large to preview" });
    }
    let bytes = std::fs::read(file).map_err(|e| format!("cannot read file: {e}"))?;
    match String::from_utf8(bytes) {
        Ok(content) => Ok(Preview::Text { content, size }),
        Err(_) => Ok(Preview::Binary { size, reason: "not a text file" }),
    }
}

/// Best-effort content type from the extension, for downloads.
pub fn content_type_for(file: &Path) -> &'static str {
    match file
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        Some("json") => "application/json",
        Some("zip") => "application/zip",
        Some("gz" | "tgz") => "application/gzip",
        Some("mp4") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("csv") => "text/csv",
        Some("html" | "htm") => "text/html",
        Some("txt" | "md" | "log" | "toml" | "yaml" | "yml") => "text/plain",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn root_of(dir: &Path) -> Vec<Root> {
        vec![Root {
            path: dir.canonicalize().unwrap().to_string_lossy().into_owned(),
            name: "t".into(),
        }]
    }

    #[test]
    fn resolves_a_path_inside_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "hi").unwrap();
        let roots = root_of(tmp.path());
        let (p, r) = resolve_within(
            tmp.path().join("a.txt").to_str().unwrap(),
            &roots,
        )
        .unwrap();
        assert!(p.ends_with("a.txt"));
        assert_eq!(r, tmp.path().canonicalize().unwrap());
    }

    #[test]
    fn dotdot_cannot_escape_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let inner = tmp.path().join("proj");
        fs::create_dir(&inner).unwrap();
        fs::write(tmp.path().join("secret.txt"), "s").unwrap();
        // Root is the inner dir; reach for the sibling above it.
        let roots = root_of(&inner);
        let escape = inner.join("../secret.txt");
        let err = resolve_within(escape.to_str().unwrap(), &roots).unwrap_err();
        assert!(err.contains("outside"), "{err}");
    }

    #[test]
    fn symlink_out_of_the_root_is_rejected() {
        // The reason paths are canonicalized rather than string-prefixed: a
        // link inside a project must not expose what it points at.
        let tmp = tempfile::tempdir().unwrap();
        let inner = tmp.path().join("proj");
        fs::create_dir(&inner).unwrap();
        let outside = tmp.path().join("outside.txt");
        fs::write(&outside, "secret").unwrap();
        let link = inner.join("link.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        #[cfg(not(unix))]
        return;

        let roots = root_of(&inner);
        let err = resolve_within(link.to_str().unwrap(), &roots).unwrap_err();
        assert!(err.contains("outside"), "{err}");
    }

    #[test]
    fn missing_path_is_an_error_not_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = root_of(tmp.path());
        assert!(resolve_within("/definitely/not/here", &roots).is_err());
        assert!(resolve_within("", &roots).is_err());
    }

    #[test]
    fn listing_hides_noise_and_sorts_dirs_first() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("node_modules")).unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("README.md"), "x").unwrap();
        fs::write(tmp.path().join(".env"), "SECRET=1").unwrap();
        let root = tmp.path().canonicalize().unwrap();

        let l = list(&root, &root, false).unwrap();
        let names: Vec<&str> = l.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["src", "README.md"]);
        assert!(l.filtered);

        // show_all reveals both the noise dir and the dotfile.
        let all = list(&root, &root, true).unwrap();
        let names: Vec<&str> = all.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"node_modules"));
        assert!(names.contains(&".env"));
        assert!(!all.filtered);
    }

    #[test]
    fn preview_returns_text_and_flags_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let text = tmp.path().join("a.txt");
        fs::write(&text, "hello 世界").unwrap();
        match preview(&text).unwrap() {
            Preview::Text { content, size } => {
                assert_eq!(content, "hello 世界");
                assert!(size > 0);
            }
            _ => panic!("expected text"),
        }

        let bin = tmp.path().join("a.bin");
        fs::write(&bin, [0xff, 0xfe, 0x00, 0x01]).unwrap();
        assert!(matches!(preview(&bin).unwrap(), Preview::Binary { .. }));
    }

    #[test]
    fn nested_roots_collapse_into_their_ancestor() {
        // Uses real project paths: tempdirs live under /var/folders, which the
        // forbidden-root guard (correctly) rejects.
        let home = dirs::home_dir().unwrap();
        let outer = home.join("projects/devs/opensource");
        let inner = outer.join("amux");
        if !inner.is_dir() {
            return; // not this machine
        }
        let rs = roots(
            &[outer.to_string_lossy().into_owned()],
            &[inner.to_string_lossy().into_owned()],
        );
        assert_eq!(rs.len(), 1, "nested path should collapse");
        assert!(rs[0].path.ends_with("opensource"));
    }

    #[test]
    fn home_directory_never_becomes_a_root() {
        // Regression: the project history genuinely contained "/Users/not",
        // "~/.claude", "~/.Trash/tmptest", "~/Library/Logs/DiagnosticReports"
        // and "/private/tmp". Because roots absorb their descendants, those
        // entries turned credential stores and the whole home directory into
        // browsable space. All must be dropped outright.
        let home = dirs::home_dir().unwrap();
        let hs = |sub: &str| home.join(sub).to_string_lossy().into_owned();
        let rs = roots(
            &[home.to_string_lossy().into_owned()],
            &[
                hs(".claude"),
                hs(".ssh"),
                hs(".Trash/tmptest"),
                hs("Library/Logs/DiagnosticReports"),
                "/private/tmp".to_string(),
                "/private/var/folders/x/T/tmp.abc".to_string(),
                "/tmp".to_string(),
                "/Users".to_string(),
                "/usr".to_string(),
            ],
        );
        assert!(
            rs.is_empty(),
            "got roots that should be forbidden: {:?}",
            rs.iter().map(|r| &r.path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_project_under_home_is_still_allowed() {
        // The guard rejects $HOME and its dot/Library subtrees, not every path
        // beneath it — a normal project directory must still be browsable.
        let home = dirs::home_dir().unwrap();
        let proj = home.join("projects/devs/opensource/amux");
        if !proj.is_dir() {
            return; // not this machine; the shape is covered by the unit above
        }
        let rs = roots(&[proj.to_string_lossy().into_owned()], &[]);
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0].name, "amux");
    }
}
