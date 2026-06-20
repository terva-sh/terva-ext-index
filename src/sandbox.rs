//! Path confinement for the `index` extension.
//!
//! terva extensions are expected to **self-jail**: a tool may only touch
//! paths inside the workspace (the session `cwd`) or the extension's own
//! `data_dir` / `extension_dir`. Unlike host-side tools there is no
//! `/unjail` for an extension — the jail is always on. If the agent truly
//! needs a file outside the workspace it must pull it in deliberately (the
//! host `read` tool can be unjailed, or it can copy the file into the
//! workspace), never lean on `index` to read outside the project.
//!
//! The confinement semantics mirror terva's own `tools/sandbox.go`:
//! canonicalize (resolve symlinks on) both the roots and the target, then
//! require the target to be the root or a descendant. Canonicalizing the
//! target is what defeats a symlink inside the workspace that points out of
//! it — the resolved path lands outside every root and is refused.

use std::path::{Path, PathBuf};

/// Why a path was rejected by the jail.
#[derive(Debug)]
pub enum JailError {
    /// The path could not be canonicalized — it does not exist, or a
    /// component is unreadable. Carries the underlying IO error.
    NotFound(std::io::Error),
    /// The path canonicalized fine but resolves outside every jail root.
    Outside,
}

/// A set of canonical roots a path is allowed to resolve inside.
pub struct Jail {
    roots: Vec<PathBuf>,
}

impl Jail {
    /// Build a jail from candidate roots (the workspace `cwd`, plus the
    /// extension's `data_dir` / `extension_dir`). Roots that cannot be
    /// canonicalized (e.g. a `data_dir` that does not exist yet) are
    /// dropped; an empty set refuses everything, which is the safe default.
    pub fn new<I, P>(roots: I) -> Jail
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let roots = roots
            .into_iter()
            .filter_map(|r| std::fs::canonicalize(r).ok())
            .collect();
        Jail { roots }
    }

    /// Canonicalize `path` (resolving symlinks) and confirm it is one of
    /// the jail roots or a descendant. Returns the canonical path on
    /// success so the caller can read exactly what it checked (no TOCTOU
    /// gap between the check and a later re-resolution of `path`).
    pub fn resolve(&self, path: &Path) -> Result<PathBuf, JailError> {
        let canon = std::fs::canonicalize(path).map_err(JailError::NotFound)?;
        // Path::starts_with is component-wise, so "/a/bc" is NOT under
        // "/a/b" — no string-prefix false positives.
        if self.roots.iter().any(|root| canon.starts_with(root)) {
            Ok(canon)
        } else {
            Err(JailError::Outside)
        }
    }

    #[cfg(test)]
    pub fn root_count(&self) -> usize {
        self.roots.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn unique_dir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "terva-ext-index-jail-{}-{}",
            tag,
            std::process::id()
        ));
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn allows_file_inside_root() {
        let root = unique_dir("inside");
        let f = root.join("a.rs");
        fs::write(&f, b"x").unwrap();
        let jail = Jail::new([&root]);
        let canon = jail.resolve(&f).expect("inside the root must be allowed");
        assert!(canon.ends_with("a.rs"));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn refuses_path_outside_root() {
        let root = unique_dir("outside-root");
        let other = unique_dir("outside-other");
        let f = other.join("secret.rs");
        fs::write(&f, b"x").unwrap();
        let jail = Jail::new([&root]);
        match jail.resolve(&f) {
            Err(JailError::Outside) => {}
            other => panic!("expected Outside, got {other:?}"),
        }
        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&other).ok();
    }

    #[test]
    fn refuses_symlink_escape() {
        // A symlink *inside* the root that points *outside* it must be
        // refused, because we confine the canonical (symlink-resolved) path.
        let root = unique_dir("symlink-root");
        let outside = unique_dir("symlink-target");
        let secret = outside.join("secret.rs");
        fs::write(&secret, b"x").unwrap();
        let link = root.join("link.rs");
        let _ = fs::remove_file(&link);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        #[cfg(not(unix))]
        return; // symlink semantics differ; covered on unix CI.
        let jail = Jail::new([&root]);
        match jail.resolve(&link) {
            Err(JailError::Outside) => {}
            other => panic!("symlink escape must be refused, got {other:?}"),
        }
        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn missing_path_is_not_found() {
        let root = unique_dir("missing");
        let jail = Jail::new([&root]);
        match jail.resolve(&root.join("nope.rs")) {
            Err(JailError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn uncanonicalizable_roots_are_dropped() {
        let root = unique_dir("drop");
        let jail = Jail::new([root.clone(), root.join("does-not-exist")]);
        assert_eq!(jail.root_count(), 1, "missing root should be dropped");
        fs::remove_dir_all(&root).ok();
    }
}
