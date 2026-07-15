//! Path helpers: lexical normalization, containment, and ~/.nano layout.

use dirs::home_dir;
use std::path::{Component, Path, PathBuf};

/// Everything nano owns under the home tree lives here.
pub fn nano_home() -> PathBuf {
    home_dir().unwrap_or_default().join(".nano")
}

pub fn nano_config_path() -> PathBuf {
    nano_home().join("config.json")
}

pub fn nano_mcp_cache_path() -> PathBuf {
    nano_home().join("mcp_cache.json")
}

pub fn nano_sessions_dir() -> PathBuf {
    nano_home().join("sessions")
}

pub fn nano_trusted_projects_dir() -> PathBuf {
    nano_home().join("trusted-projects")
}

/// Stable, filesystem-safe key for a cwd (FNV-1a hex). Collision risk is
/// academic; each session still stores its absolute `cwd` for filtering.
pub fn cwd_session_key(cwd: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in cwd.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

pub fn session_file_for_cwd(cwd: &str) -> PathBuf {
    nano_sessions_dir().join(format!("{}.jsonl", cwd_session_key(cwd)))
}

/// Create `~/.nano` (+ `sessions/`) on demand. Best-effort; callers ignore errors.
pub fn ensure_nano_dirs() {
    let home = nano_home();
    let sessions = home.join("sessions");
    let trusted_projects = home.join("trusted-projects");
    let _ = std::fs::create_dir_all(&sessions);
    let _ = std::fs::create_dir_all(&trusted_projects);
    make_private(&home, 0o700);
    make_private(&sessions, 0o700);
    make_private(&trusted_projects, 0o700);
    make_private(&home.join("config.json"), 0o600);
    make_private(&home.join("mcp_cache.json"), 0o600);
    if let Ok(entries) = std::fs::read_dir(sessions) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_file()) {
                make_private(&entry.path(), 0o600);
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(trusted_projects) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_file()) {
                make_private(&entry.path(), 0o600);
            }
        }
    }
}

#[cfg(unix)]
fn make_private(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;

    if path.exists() {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
}

#[cfg(not(unix))]
fn make_private(_path: &Path, _mode: u32) {}

/// Lexically normalize a path: resolve `.` and `..` components without
/// touching the filesystem.
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

/// Returns true if `path` is `root` itself or located under `root`,
/// comparing lexically normalized paths.
pub fn path_is_inside(root: &Path, path: &Path) -> bool {
    let root = normalize_path(root);
    let path = normalize_path(path);
    path == root || path.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_resolves_dot_components() {
        assert_eq!(
            normalize_path(Path::new("/a/./b/../c")),
            PathBuf::from("/a/c")
        );
    }

    #[test]
    fn path_is_inside_accepts_root_and_children() {
        assert!(path_is_inside(Path::new("/a/b"), Path::new("/a/b")));
        assert!(path_is_inside(Path::new("/a/b"), Path::new("/a/b/c")));
        assert!(!path_is_inside(Path::new("/a/b"), Path::new("/a/bc")));
        assert!(!path_is_inside(Path::new("/a/b"), Path::new("/a/b/../d")));
    }

    #[test]
    fn cwd_session_key_is_stable() {
        assert_eq!(cwd_session_key("/tmp/a"), cwd_session_key("/tmp/a"));
        assert_ne!(cwd_session_key("/tmp/a"), cwd_session_key("/tmp/b"));
    }
}
