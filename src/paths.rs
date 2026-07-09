//! Path helpers: lexical normalization and containment checks.

use std::path::{Component, Path, PathBuf};

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
}
