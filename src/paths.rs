//! Remote path helpers, kept separate so both the CLI and the downloader
//! derive names the same way.

/// Split `/pub/data/file.bin` into (`Some("/pub/data")`, `"file.bin"`).
///
/// A bare name yields `(None, name)`, which means "the login directory".
pub fn split_remote(path: &str) -> (Option<String>, String) {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) => (Some("/".to_string()), trimmed[1..].to_string()),
        Some(idx) => (
            Some(trimmed[..idx].to_string()),
            trimmed[idx + 1..].to_string(),
        ),
        None => (None, trimmed.to_string()),
    }
}

/// The last component of a remote path.
pub fn basename(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(idx) => trimmed[idx + 1..].to_string(),
        None => trimmed.to_string(),
    }
}

/// Join a directory and a name, tolerating a missing or doubled slash.
pub fn join_remote(dir: &str, name: &str) -> String {
    if dir.is_empty() || dir == "." {
        return name.to_string();
    }
    let dir = dir.trim_end_matches('/');
    if dir.is_empty() {
        return format!("/{name}");
    }
    format!("{dir}/{name}")
}

/// `true` for paths that start at the server root.
pub fn is_absolute(path: &str) -> bool {
    path.starts_with('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_absolute_and_relative_paths() {
        assert_eq!(
            split_remote("/pub/data/file.bin"),
            (Some("/pub/data".to_string()), "file.bin".to_string())
        );
        assert_eq!(
            split_remote("/file.bin"),
            (Some("/".to_string()), "file.bin".to_string())
        );
        assert_eq!(split_remote("file.bin"), (None, "file.bin".to_string()));
        assert_eq!(
            split_remote("sub/file.bin"),
            (Some("sub".to_string()), "file.bin".to_string())
        );
    }

    #[test]
    fn extracts_basenames() {
        assert_eq!(basename("/a/b/c.txt"), "c.txt");
        assert_eq!(basename("c.txt"), "c.txt");
        assert_eq!(basename("/a/b/"), "b");
        assert_eq!(basename("/"), "");
    }

    #[test]
    fn joins_paths() {
        assert_eq!(join_remote("/pub", "a.txt"), "/pub/a.txt");
        assert_eq!(join_remote("/pub/", "a.txt"), "/pub/a.txt");
        assert_eq!(join_remote("", "a.txt"), "a.txt");
        assert_eq!(join_remote("/", "a.txt"), "/a.txt");
        assert_eq!(join_remote(".", "a.txt"), "a.txt");
    }

    #[test]
    fn detects_absolute_paths() {
        assert!(is_absolute("/pub/a"));
        assert!(!is_absolute("pub/a"));
    }
}
