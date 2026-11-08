//! Content + structure tree hash used to prove read-path non-mutation (W-5).
//!
//! [`tree_hash`] digests every regular file (SHA-256 of contents) and the
//! directory structure itself. The §1.1 fixture experiments' blind spot —
//! directory creation/removal and empty-directory changes — is covered:
//! every directory gets its own record, so creating or unlinking even an
//! empty directory changes the digest.
//!
//! Deliberate exclusions, and why they are safe for the W-5 guarantee:
//!
//! - **Timestamps and permissions** are not hashed. The guarantee we test is
//!   *content and structure*; libgpod's atime/mtime bumps on read are
//!   expected and benign.
//! - **Symlinks are recorded by target string and never followed** — a
//!   hostile tree must not redirect hashing outside the root.
//! - The root directory's own name is not part of the digest (the tree can
//!   be copied anywhere); its *contents* are.
//! - Path components are hashed as **raw bytes**, not lossy strings: two
//!   distinct non-UTF-8 names must never collapse into one digest record.
//!
//! The records are sorted by relative path before hashing, so the digest is
//! independent of `readdir` order.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Why a tree hash failed.
#[derive(Debug, thiserror::Error)]
#[error("tree hash failed at {path}: {source}")]
pub struct TreeHashError {
    pub path: PathBuf,
    #[source]
    pub source: std::io::Error,
}

/// Compute the content + structure digest of the tree rooted at `root`.
///
/// `root` must exist. Returns the lowercase hex SHA-256.
pub fn tree_hash(root: &Path) -> Result<String, TreeHashError> {
    let mut entries: BTreeMap<Vec<u8>, String> = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read = std::fs::read_dir(&dir).map_err(|source| TreeHashError {
            path: dir.clone(),
            source,
        })?;
        for entry in read {
            let entry = entry.map_err(|source| TreeHashError {
                path: dir.clone(),
                source,
            })?;
            let path = entry.path();
            let relative = relative_key(&path, root)?;
            let file_type = entry.file_type().map_err(|source| TreeHashError {
                path: path.clone(),
                source,
            })?;
            let record = if file_type.is_dir() {
                stack.push(path.clone());
                "d".to_string()
            } else if file_type.is_symlink() {
                let target = std::fs::read_link(&path).map_err(|source| TreeHashError {
                    path: path.clone(),
                    source,
                })?;
                format!("l {}", target.to_string_lossy())
            } else {
                let data = std::fs::read(&path).map_err(|source| TreeHashError {
                    path: path.clone(),
                    source,
                })?;
                format!("f {} {}", data.len(), hex(&sha256(&data)))
            };
            entries.insert(relative, record);
        }
    }

    // Canonical order: sorted by relative path, path NUL record LF.
    let mut hasher = Sha256::new();
    for (path, record) in &entries {
        hasher.update(path);
        hasher.update([0]);
        hasher.update(record.as_bytes());
        hasher.update([b'\n']);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Relative path of `path` under `root` as raw encoded bytes, joined with
/// `/` separators — the digest must not depend on Unicode round-trips.
fn relative_key(path: &Path, root: &Path) -> Result<Vec<u8>, TreeHashError> {
    let rel = path.strip_prefix(root).map_err(|source| TreeHashError {
        path: path.to_path_buf(),
        source: std::io::Error::other(source.to_string()),
    })?;
    let mut key = Vec::new();
    for (i, component) in rel.components().enumerate() {
        if i > 0 {
            key.push(b'/');
        }
        key.extend_from_slice(component.as_os_str().as_encoded_bytes());
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, contents: &[u8]) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn identical_trees_hash_identically() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        for root in [a.path(), b.path()] {
            write(root, "iPod_Control/iTunesDB", b"db");
            write(root, "iPod_Control/Artwork/ArtworkDB", b"art");
        }
        assert_eq!(tree_hash(a.path()).unwrap(), tree_hash(b.path()).unwrap());
    }

    #[test]
    fn empty_directory_changes_the_hash() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a", b"x");
        let before = tree_hash(dir.path()).unwrap();
        std::fs::create_dir(dir.path().join("empty")).unwrap();
        let after = tree_hash(dir.path()).unwrap();
        assert_ne!(before, after, "an empty directory must change the digest");
    }

    #[test]
    fn content_change_changes_the_hash() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a", b"x");
        let before = tree_hash(dir.path()).unwrap();
        write(dir.path(), "a", b"y");
        let after = tree_hash(dir.path()).unwrap();
        assert_ne!(before, after);
    }

    #[test]
    fn rename_changes_the_hash() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a", b"x");
        let before = tree_hash(dir.path()).unwrap();
        std::fs::rename(dir.path().join("a"), dir.path().join("b")).unwrap();
        let after = tree_hash(dir.path()).unwrap();
        assert_ne!(before, after);
    }

    #[test]
    fn timestamps_do_not_change_the_hash() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a", b"x");
        let before = tree_hash(dir.path()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        // Same content, newer mtime.
        std::fs::write(dir.path().join("a"), b"x").unwrap();
        let after = tree_hash(dir.path()).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn symlink_is_recorded_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("target"), b"secret").unwrap();
        write(dir.path(), "a", b"x");
        let before = tree_hash(dir.path()).unwrap();
        std::os::unix::fs::symlink(outside.path().join("target"), dir.path().join("link")).unwrap();
        let after = tree_hash(dir.path()).unwrap();
        assert_ne!(
            before, after,
            "the symlink record must be part of the digest"
        );
        // Changing the *target's* content must not change the digest: the
        // link is not followed.
        std::fs::write(outside.path().join("target"), b"changed").unwrap();
        assert_eq!(tree_hash(dir.path()).unwrap(), after);
    }

    #[test]
    fn nested_empty_dirs_are_distinct() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a", b"x");
        let before = tree_hash(dir.path()).unwrap();
        std::fs::create_dir_all(dir.path().join("nested/deep/empty")).unwrap();
        let after = tree_hash(dir.path()).unwrap();
        assert_ne!(before, after);
    }

    #[test]
    fn missing_root_is_an_error() {
        let err = tree_hash(Path::new("/nonexistent/definitely")).unwrap_err();
        assert!(err.to_string().contains("tree hash failed"));
    }

    /// Distinct non-UTF-8 names must not collapse into one record: `a\xff`
    /// and `a\xfe` both lossy-decode to the same replacement string, so a
    /// lossy key would hide a rename between them.
    #[cfg(unix)]
    #[test]
    fn non_utf8_names_do_not_collide() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(std::ffi::OsStr::from_bytes(b"a\xff")), b"x").unwrap();
        let before = tree_hash(dir.path()).unwrap();
        std::fs::rename(
            dir.path().join(std::ffi::OsStr::from_bytes(b"a\xff")),
            dir.path().join(std::ffi::OsStr::from_bytes(b"a\xfe")),
        )
        .unwrap();
        let after = tree_hash(dir.path()).unwrap();
        assert_ne!(before, after, "raw path bytes must be part of the digest");
    }
}
