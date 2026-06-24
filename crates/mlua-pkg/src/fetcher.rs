//! Git-based package fetcher.
//!
//! The [`Fetcher`] trait abstracts over different fetch backends (git, luarocks,
//! http).  [`GitFetcher`] implements the git backend using libgit2 via the
//! [`git2`] crate.  No subprocess `git` invocations are used.
//!
//! # Cache layout
//!
//! ```text
//! <cache_root>/git/<host>/<path…>/<sha>/
//! ```
//!
//! For example, `https://github.com/ynishi/lshape` at SHA `abc123` becomes:
//!
//! ```text
//! <cache_root>/git/github.com/ynishi/lshape/abc123/
//! ```
//!
//! If the directory already exists the clone is skipped.
//!
//! # Authentication
//!
//! `GitFetcher` uses a `RemoteCallbacks`-based credential cascade:
//!
//! 1. SSH agent (`Cred::ssh_key_from_agent`) — tried only when the remote
//!    advertises `SSH_KEY` in `allowed_types`.
//! 2. Credential helper (`Cred::credential_helper`) — tried when `USER_PASS_PLAINTEXT`
//!    is advertised.
//! 3. `Cred::default()` — last resort.
//!
//! The callback tracks attempted credential types to avoid infinite retry loops.

use std::{
    path::{Component, PathBuf},
    sync::atomic::{AtomicU64, AtomicU8, Ordering},
};

use git2::{build::RepoBuilder, CredentialType, FetchOptions, RemoteCallbacks, Repository};

use crate::{
    manifest::{Dep, Manifest},
    PkgError,
};

/// Monotonic counter for unique temp-clone directory names within one process.
static TMP_CTR: AtomicU64 = AtomicU64::new(0);

// ── Fetcher trait ─────────────────────────────────────────────────────────────

/// Abstraction over package fetch backends.
///
/// Implementations are *not* required to be [`Send`] or [`Sync`] — the MVP
/// is single-threaded.
pub trait Fetcher {
    /// Fetch the package described by `dep` and return a [`FetchedPkg`].
    ///
    /// # Errors
    ///
    /// Returns [`PkgError`] on any git, I/O, or validation failure.
    fn fetch(&self, dep: &Dep) -> Result<FetchedPkg, PkgError>;
}

// ── FetchedPkg ────────────────────────────────────────────────────────────────

/// Result of a successful [`Fetcher::fetch`] call.
#[derive(Debug, Clone)]
pub struct FetchedPkg {
    /// Absolute path to the cloned repository on disk.
    pub cache_path: PathBuf,

    /// The resolved commit SHA (40-character hex string).
    pub sha: String,

    /// The parsed `mlua-pkg.toml` found at `cache_path`, if present.
    pub manifest: Option<Manifest>,
}

// ── GitFetcher ────────────────────────────────────────────────────────────────

/// [`Fetcher`] implementation backed by libgit2.
pub struct GitFetcher {
    /// Root directory under which all git caches are stored.
    cache_root: PathBuf,
}

impl GitFetcher {
    /// Create a new `GitFetcher` that stores clones under `cache_root`.
    pub fn new(cache_root: PathBuf) -> Self {
        Self { cache_root }
    }

    /// Compute the cache directory for the given URL and SHA.
    ///
    /// Layout: `<cache_root>/git/<host>/<path…>/<sha>/`
    ///
    /// # Errors
    ///
    /// Returns [`PkgError::Validation`] if the URL cannot be parsed or if any
    /// URL-derived path component contains `..` (path traversal defence).
    fn cache_dir(&self, url: &str, sha: &str) -> Result<PathBuf, PkgError> {
        // Strip protocol prefix and trailing `.git`.
        let stripped = url
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_start_matches("ssh://")
            .trim_start_matches("git@")
            .replace(':', "/") // git@github.com:user/repo → github.com/user/repo
            .trim_end_matches(".git")
            .to_owned();

        if stripped.is_empty() {
            return Err(PkgError::Validation {
                message: format!("cannot derive cache path from URL: {url:?}"),
            });
        }

        // Defend against path traversal in every component.
        for component in stripped.split('/') {
            if component == ".." || component == "." {
                return Err(PkgError::Validation {
                    message: format!(
                        "URL {url:?} contains a path traversal component: {component:?}"
                    ),
                });
            }
        }

        // Validate SHA is safe (hex chars only).
        if sha.is_empty() || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(PkgError::Validation {
                message: format!("invalid SHA: {sha:?}"),
            });
        }

        let mut path = self.cache_root.join("git");
        for segment in stripped.split('/') {
            if segment.is_empty() {
                continue;
            }
            // Extra check: ensure no path component resolves to `..` via PathBuf.
            let p = path.join(segment);
            for c in p.components() {
                if c == Component::ParentDir {
                    return Err(PkgError::Validation {
                        message: format!(
                            "URL {url:?} resolves to a path with parent-dir traversal"
                        ),
                    });
                }
            }
            path = p;
        }
        path = path.join(sha);
        Ok(path)
    }

    /// Validate the URL structure before any network operation.
    ///
    /// Rejects URLs that contain `..` or `.` path components (path traversal
    /// defence).  Called at the start of [`Fetcher::fetch`] so that invalid
    /// URLs are rejected without making a network connection.
    fn validate_url(url: &str) -> Result<(), PkgError> {
        let stripped = url
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_start_matches("ssh://")
            .trim_start_matches("git@")
            .replace(':', "/")
            .trim_end_matches(".git")
            .to_owned();

        if stripped.is_empty() {
            return Err(PkgError::Validation {
                message: format!("cannot derive cache path from URL: {url:?}"),
            });
        }

        for component in stripped.split('/') {
            if component == ".." || component == "." {
                return Err(PkgError::Validation {
                    message: format!(
                        "URL {url:?} contains a path traversal component: {component:?}"
                    ),
                });
            }
        }
        Ok(())
    }

    /// Return a unique temp-clone path inside `git_base` (same filesystem, so
    /// `std::fs::rename` works atomically).
    fn temp_clone_path(git_base: &std::path::Path) -> PathBuf {
        let n = TMP_CTR.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        git_base.join(format!(".fetch-{pid}-{n}"))
    }

    /// Resolve the git ref from `dep` to a full 40-char SHA.
    ///
    /// Resolution order:
    /// 1. `rev` — treated as a revspec; the commit it peels to is returned.
    /// 2. `tag` — resolved via `refs/tags/<tag>` (peeled to commit).
    /// 3. `branch` — resolved via `refs/remotes/origin/<branch>` (peeled to commit).
    /// 4. No ref — `HEAD` (latest commit on default branch).
    ///
    /// The repository must already have been fetched (all refs present).
    fn resolve_sha(repo: &Repository, dep: &Dep) -> Result<String, PkgError> {
        let oid = if let Some(rev) = &dep.rev {
            repo.revparse_single(rev)?.peel_to_commit()?.id()
        } else if let Some(tag) = &dep.tag {
            let refname = format!("refs/tags/{tag}");
            repo.find_reference(&refname)?.peel_to_commit()?.id()
        } else if let Some(branch) = &dep.branch {
            let refname = format!("refs/remotes/origin/{branch}");
            repo.find_reference(&refname)?.peel_to_commit()?.id()
        } else {
            // Default: HEAD
            repo.head()?.peel_to_commit()?.id()
        };
        Ok(oid.to_string())
    }

    /// Reset the worktree of `repo` hard to the commit named by `sha`.
    fn checkout_sha(repo: &Repository, sha: &str) -> Result<(), PkgError> {
        let oid = git2::Oid::from_str(sha).map_err(|e| PkgError::Validation {
            message: format!("invalid SHA {sha}: {e}"),
        })?;
        let obj = repo.find_object(oid, None)?;
        repo.reset(&obj, git2::ResetType::Hard, None)?;
        Ok(())
    }

    /// Build `FetchOptions` with the credential cascade callback.
    fn make_fetch_options() -> FetchOptions<'static> {
        let mut callbacks = RemoteCallbacks::new();

        // Track which credential types we've already tried to avoid infinite loops.
        // Bits: 0 = SSH_KEY, 1 = USER_PASS_PLAINTEXT, 2 = DEFAULT
        let tried = AtomicU8::new(0);

        callbacks.credentials(move |_url, username, allowed| {
            let tried_bits = tried.load(Ordering::Relaxed);

            // 1. SSH agent
            if allowed.contains(CredentialType::SSH_KEY) && (tried_bits & 0b001 == 0) {
                tried.fetch_or(0b001, Ordering::Relaxed);
                let user = username.unwrap_or("git");
                return git2::Cred::ssh_key_from_agent(user);
            }

            // 2. Credential helper (HTTPS)
            if allowed.contains(CredentialType::USER_PASS_PLAINTEXT) && (tried_bits & 0b010 == 0) {
                tried.fetch_or(0b010, Ordering::Relaxed);
                if let Ok(cfg) = git2::Config::open_default() {
                    return git2::Cred::credential_helper(&cfg, _url, username);
                }
                // If we cannot open git config fall through to default cred.
            }

            // 3. Default
            if tried_bits & 0b100 == 0 {
                tried.fetch_or(0b100, Ordering::Relaxed);
                return git2::Cred::default();
            }

            Err(git2::Error::from_str("all credential types exhausted"))
        });

        let mut fo = FetchOptions::new();
        fo.remote_callbacks(callbacks);
        fo
    }
}

impl Fetcher for GitFetcher {
    fn fetch(&self, dep: &Dep) -> Result<FetchedPkg, PkgError> {
        let url = &dep.git;

        // Reject obviously malformed / traversal URLs before any I/O.
        Self::validate_url(url)?;

        // All git caches live under `<cache_root>/git/`.
        let git_base = self.cache_root.join("git");
        std::fs::create_dir_all(&git_base)?;

        // Clone into a temp directory that lives on the same filesystem as the
        // final cache location so that `std::fs::rename` is atomic.
        let tmp_path = Self::temp_clone_path(&git_base);

        // ── Clone ────────────────────────────────────────────────────────────
        let fo = Self::make_fetch_options();
        let repo = match RepoBuilder::new().fetch_options(fo).clone(url, &tmp_path) {
            Ok(r) => r,
            Err(e) => {
                // Best-effort cleanup on error.
                let _ = std::fs::remove_dir_all(&tmp_path);
                return Err(e.into());
            }
        };

        // ── Resolve SHA ──────────────────────────────────────────────────────
        let sha = match Self::resolve_sha(&repo, dep) {
            Ok(s) => s,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp_path);
                return Err(e);
            }
        };

        // ── Checkout resolved commit ─────────────────────────────────────────
        // `clone()` leaves the worktree at the cloned HEAD (= default branch
        // tip), which for tag/rev/branch lookups is the wrong commit.  Reset
        // hard to the resolved SHA so the cached content matches the SHA we
        // pin in the lockfile.
        if let Err(e) = Self::checkout_sha(&repo, &sha) {
            let _ = std::fs::remove_dir_all(&tmp_path);
            return Err(e);
        }

        // ── Compute final cache path ─────────────────────────────────────────
        let cache_path = match self.cache_dir(url, &sha) {
            Ok(p) => p,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp_path);
                return Err(e);
            }
        };

        if cache_path.exists() {
            // Already cached — discard the temp clone.
            drop(repo);
            let _ = std::fs::remove_dir_all(&tmp_path);
        } else {
            // Ensure the parent directory exists, then rename temp → final.
            if let Some(parent) = cache_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            drop(repo); // Release file handles before rename.
            std::fs::rename(&tmp_path, &cache_path)?;
        }

        // ── Parse manifest if present ────────────────────────────────────────
        let manifest_path = cache_path.join("mlua-pkg.toml");
        let manifest = if manifest_path.exists() {
            Some(Manifest::from_path(&manifest_path)?)
        } else {
            None
        };

        Ok(FetchedPkg {
            cache_path,
            sha,
            manifest,
        })
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Repository, Signature};
    use std::fs;
    use tempfile::TempDir;

    /// Create a minimal git repo in `dir` with one commit and return the SHA.
    fn init_repo_with_commit(dir: &std::path::Path) -> String {
        let repo = Repository::init(dir).unwrap();

        // Configure identity for the test repo.
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Test").unwrap();
        config.set_str("user.email", "test@example.com").unwrap();
        drop(config);

        // Create an initial file and commit.
        let file_path = dir.join("README.md");
        fs::write(&file_path, "# test\n").unwrap();

        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("README.md")).unwrap();
        index.write().unwrap();

        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = Signature::now("Test", "test@example.com").unwrap();
        let oid = repo
            .commit(Some("HEAD"), &sig, &sig, "initial commit", &tree, &[])
            .unwrap();
        oid.to_string()
    }

    /// Add an annotated tag to the HEAD commit of `repo`.
    fn add_tag(repo: &Repository, tag_name: &str) -> String {
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        let sig = Signature::now("Test", "test@example.com").unwrap();
        repo.tag(tag_name, head.as_object(), &sig, tag_name, false)
            .unwrap();
        head.id().to_string()
    }

    // ── 1. clone a local file:// repo (happy path) ───────────────────────────

    #[test]
    fn clone_local_repo_happy_path() {
        let src = TempDir::new().unwrap();
        let sha = init_repo_with_commit(src.path());

        let cache_root = TempDir::new().unwrap();
        let fetcher = GitFetcher::new(cache_root.path().to_path_buf());

        let url = format!("file://{}", src.path().display());
        let dep = Dep {
            git: url,
            tag: None,
            rev: None,
            branch: None,
            entry: None,
        };

        let result = fetcher.fetch(&dep).unwrap();
        assert_eq!(result.sha, sha, "SHA should match the initial commit");
        assert!(result.cache_path.exists(), "cache_path must exist on disk");
        assert!(
            result.manifest.is_none(),
            "no mlua-pkg.toml in bare test repo"
        );
    }

    // ── 2. resolve tag → SHA ──────────────────────────────────────────────────

    #[test]
    fn resolve_tag_sha() {
        let src = TempDir::new().unwrap();
        init_repo_with_commit(src.path());
        let repo = Repository::open(src.path()).unwrap();
        let expected_sha = add_tag(&repo, "v0.1.0");
        drop(repo);

        let cache_root = TempDir::new().unwrap();
        let fetcher = GitFetcher::new(cache_root.path().to_path_buf());

        let url = format!("file://{}", src.path().display());
        let dep = Dep {
            git: url,
            tag: Some("v0.1.0".to_string()),
            rev: None,
            branch: None,
            entry: None,
        };

        let result = fetcher.fetch(&dep).unwrap();
        assert_eq!(result.sha, expected_sha, "tag must resolve to expected SHA");
        assert!(result.cache_path.exists());
    }

    // ── 3. resolve rev → SHA ──────────────────────────────────────────────────

    #[test]
    fn resolve_rev_sha() {
        let src = TempDir::new().unwrap();
        let sha = init_repo_with_commit(src.path());

        let cache_root = TempDir::new().unwrap();
        let fetcher = GitFetcher::new(cache_root.path().to_path_buf());

        let url = format!("file://{}", src.path().display());
        let dep = Dep {
            git: url,
            rev: Some(sha.clone()),
            tag: None,
            branch: None,
            entry: None,
        };

        let result = fetcher.fetch(&dep).unwrap();
        assert_eq!(result.sha, sha, "rev should resolve to the given SHA");
    }

    // ── 4. nonexistent repo returns GitFetch error ────────────────────────────

    #[test]
    fn nonexistent_repo_returns_error() {
        let cache_root = TempDir::new().unwrap();
        let fetcher = GitFetcher::new(cache_root.path().to_path_buf());

        let dep = Dep {
            git: "file:///nonexistent/path/that/does/not/exist".to_string(),
            tag: None,
            rev: None,
            branch: None,
            entry: None,
        };

        let err = fetcher.fetch(&dep).unwrap_err();
        assert!(
            matches!(err, PkgError::GitFetch { .. }),
            "expected GitFetch error, got: {err}"
        );
    }

    // ── 5. second fetch of same repo uses cache (skip re-clone) ──────────────

    #[test]
    fn second_fetch_uses_cache() {
        let src = TempDir::new().unwrap();
        let sha = init_repo_with_commit(src.path());

        let cache_root = TempDir::new().unwrap();
        let fetcher = GitFetcher::new(cache_root.path().to_path_buf());

        let url = format!("file://{}", src.path().display());
        let dep = Dep {
            git: url,
            rev: Some(sha.clone()),
            tag: None,
            branch: None,
            entry: None,
        };

        let first = fetcher.fetch(&dep).unwrap();
        let second = fetcher.fetch(&dep).unwrap();

        assert_eq!(
            first.cache_path, second.cache_path,
            "cache paths must be identical"
        );
        assert_eq!(first.sha, second.sha);
    }

    // ── 6. path traversal in URL is rejected ──────────────────────────────────

    #[test]
    fn path_traversal_in_url_is_rejected() {
        let cache_root = TempDir::new().unwrap();
        let fetcher = GitFetcher::new(cache_root.path().to_path_buf());

        let dep = Dep {
            git: "https://github.com/../../../etc/passwd".to_string(),
            tag: None,
            rev: None,
            branch: None,
            entry: None,
        };

        let err = fetcher.fetch(&dep).unwrap_err();
        assert!(
            matches!(err, PkgError::Validation { .. }),
            "expected Validation error for path traversal, got: {err}"
        );
    }

    // ── 7. manifest is parsed when mlua-pkg.toml is present ──────────────────

    #[test]
    fn manifest_parsed_when_present() {
        let src = TempDir::new().unwrap();

        // Write mlua-pkg.toml before the initial commit.
        let toml_path = src.path().join("mlua-pkg.toml");
        fs::write(
            &toml_path,
            r#"[package]
name = "test-lib"
version = "0.1.0"
"#,
        )
        .unwrap();

        let repo = Repository::init(src.path()).unwrap();
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Test").unwrap();
        config.set_str("user.email", "test@example.com").unwrap();
        drop(config);

        let mut index = repo.index().unwrap();
        index
            .add_path(std::path::Path::new("mlua-pkg.toml"))
            .unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = Signature::now("Test", "test@example.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "add manifest", &tree, &[])
            .unwrap();

        let cache_root = TempDir::new().unwrap();
        let fetcher = GitFetcher::new(cache_root.path().to_path_buf());

        let url = format!("file://{}", src.path().display());
        let dep = Dep {
            git: url,
            tag: None,
            rev: None,
            branch: None,
            entry: None,
        };

        let result = fetcher.fetch(&dep).unwrap();
        let manifest = result.manifest.expect("manifest should be parsed");
        assert_eq!(manifest.package.name, "test-lib");
        assert_eq!(manifest.package.version, "0.1.0");
    }

    // ── 8. fetched worktree content matches the resolved ref, not HEAD ───────
    //
    // Regression for the v0.4.0 bug where fetch() resolved the SHA from
    // `tag = "v0.1.0"` and pinned it in the lockfile / cache dir name, but
    // left the worktree at the cloned HEAD (= default branch tip).  Consumers
    // therefore got the latest content with a stale SHA — reproducibility lost.

    #[test]
    fn fetched_worktree_matches_resolved_tag_not_head() {
        let src = TempDir::new().unwrap();
        let repo = Repository::init(src.path()).unwrap();
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Test").unwrap();
        config.set_str("user.email", "test@example.com").unwrap();
        drop(config);
        let sig = Signature::now("Test", "test@example.com").unwrap();

        // Commit 1: VERSION = "0.1.0", tagged v0.1.0.
        fs::write(src.path().join("VERSION"), "0.1.0").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("VERSION")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let c1 = repo
            .commit(Some("HEAD"), &sig, &sig, "v0.1.0", &tree, &[])
            .unwrap();
        let c1_obj = repo.find_object(c1, None).unwrap();
        repo.tag("v0.1.0", &c1_obj, &sig, "v0.1.0", false).unwrap();
        let v010_sha = c1.to_string();

        // Commit 2: VERSION = "0.2.0", HEAD advances. No tag.
        fs::write(src.path().join("VERSION"), "0.2.0").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("VERSION")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let parent = repo.find_commit(c1).unwrap();
        let c2 = repo
            .commit(Some("HEAD"), &sig, &sig, "v0.2.0", &tree, &[&parent])
            .unwrap();
        assert_ne!(c1, c2, "HEAD must have advanced past the tag");

        // Fetch tag v0.1.0.
        let cache_root = TempDir::new().unwrap();
        let fetcher = GitFetcher::new(cache_root.path().to_path_buf());
        let dep = Dep {
            git: format!("file://{}", src.path().display()),
            tag: Some("v0.1.0".to_string()),
            rev: None,
            branch: None,
            entry: None,
        };
        let fetched = fetcher.fetch(&dep).unwrap();

        // SHA must point at the tag commit, not HEAD.
        assert_eq!(fetched.sha, v010_sha, "SHA must resolve to tag commit");

        // Worktree content must match the tag commit content, not HEAD's.
        let version = fs::read_to_string(fetched.cache_path.join("VERSION")).unwrap();
        assert_eq!(
            version, "0.1.0",
            "fetched worktree must contain tag v0.1.0 content, got HEAD content instead"
        );
    }

    // ── 9. cache_dir rejects SHA with non-hex chars ───────────────────────────

    #[test]
    fn cache_dir_rejects_invalid_sha() {
        let cache_root = TempDir::new().unwrap();
        let fetcher = GitFetcher::new(cache_root.path().to_path_buf());

        let err = fetcher
            .cache_dir("https://github.com/x/y", "../evil")
            .unwrap_err();
        assert!(
            matches!(err, PkgError::Validation { .. }),
            "expected Validation error for invalid SHA, got: {err}"
        );
    }
}
