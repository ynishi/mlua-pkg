//! Crate-wide error type for the PkgMgr subsystem.
//!
//! [`PkgError`] is the single error type for all PkgMgr operations
//! (manifest parsing, lockfile I/O, git fetch, CLI).  It is distinct from
//! the existing [`crate::ResolveError`], which covers runtime `resolve()` failures.
//! Additional variants are added by subsequent subtasks.

use std::path::PathBuf;

/// Crate-wide error for PkgMgr operations.
///
/// # Error taxonomy
///
/// | Variant | Phase | Source |
/// |---------|-------|--------|
/// | [`ManifestParse`](Self::ManifestParse) | Parse | [`toml::de::Error`] via `#[from]` |
/// | [`LockfileParse`](Self::LockfileParse) | Parse | [`toml::de::Error`] via `map_err` |
/// | [`LockfileWrite`](Self::LockfileWrite) | Serialize | [`toml::ser::Error`] via `#[from]` |
/// | [`MissingLockfile`](Self::MissingLockfile) | I/O | path not found |
/// | [`SameNameConflict`](Self::SameNameConflict) | Validate | duplicate name in lockfile |
/// | [`Io`](Self::Io) | I/O | [`std::io::Error`] via `#[from]` |
/// | [`Validation`](Self::Validation) | Post-parse | custom message |
/// | [`GitFetch`](Self::GitFetch) | Fetch | [`git2::Error`] via `#[from]` |
/// | [`TagShaMismatch`](Self::TagShaMismatch) | Fetch | tag ↔ SHA conflict |
///
/// Variants are `#[non_exhaustive]` so that future subtasks can add new
/// variants without breaking downstream `match` arms that include a wildcard.
///
/// The existing [`crate::ResolveError`] is intentionally not modified.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum PkgError {
    /// TOML parse failure when reading `mlua-pkg.toml`.
    ///
    /// Automatically constructed from [`toml::de::Error`] via `?`.
    #[error("manifest parse error: {source}")]
    ManifestParse {
        #[from]
        source: toml::de::Error,
    },

    /// TOML parse failure when reading `mlua-pkg.lock`.
    ///
    /// Does **not** carry `#[from]` — [`ManifestParse`](Self::ManifestParse)
    /// already claims `From<toml::de::Error>`.  Lockfile read paths must use
    /// `.map_err(|e| PkgError::LockfileParse { source: e })` explicitly.
    #[error("lockfile parse error: {source}")]
    LockfileParse { source: toml::de::Error },

    /// TOML serialization failure when writing `mlua-pkg.lock`.
    ///
    /// Automatically constructed from [`toml::ser::Error`] via `?`.
    #[error("lockfile write error: {source}")]
    LockfileWrite {
        #[from]
        source: toml::ser::Error,
    },

    /// `mlua-pkg.lock` not found at the expected path.
    ///
    /// Raised by [`Lockfile::read`](crate::lockfile::Lockfile::read) when the
    /// file is absent, to distinguish the "not yet installed" case from other
    /// I/O failures.
    #[error("lockfile not found: {}", path.display())]
    MissingLockfile { path: PathBuf },

    /// Two packages share the same `name` field within a lockfile.
    ///
    /// Package names must be unique within a lockfile.  Raised during
    /// [`Lockfile::read`](crate::lockfile::Lockfile::read) and (in ST5) at
    /// install time.
    #[error("duplicate package name in lockfile: {name:?}")]
    SameNameConflict { name: String },

    /// I/O error while reading or writing files (manifest, lockfile, cache).
    ///
    /// Automatically constructed from [`std::io::Error`] via `?`.
    #[error("I/O error: {source}")]
    Io {
        #[from]
        source: std::io::Error,
    },

    /// Post-parse validation failure.
    ///
    /// Raised when the parsed manifest satisfies TOML grammar but violates
    /// semantic constraints, e.g. specifying both `tag` and `rev` in a
    /// single dependency entry.
    #[error("manifest validation error: {message}")]
    Validation { message: String },

    /// git2 operation failure during fetch or clone.
    ///
    /// Automatically constructed from [`git2::Error`] via `?`.
    #[error("git fetch error: {source}")]
    GitFetch {
        #[from]
        source: git2::Error,
    },

    /// The resolved SHA for a tag did not match the expected SHA.
    ///
    /// Raised when the caller pins a specific SHA together with a tag and
    /// the tag resolves to a different commit.
    #[error("tag SHA mismatch: expected {expected}, got {actual}")]
    TagShaMismatch { expected: String, actual: String },

    /// No suitable entry point directory found within a cached package.
    ///
    /// Raised by [`mlua_pkg::resolve_entry`](crate::resolve_entry) when neither
    /// the override path nor any fallback candidate (`src/`, `lua/`, `.`) exists
    /// as a directory under `cache_path`.
    ///
    /// Also raised during [`VendoredResolver::from_lockfile`](crate::resolvers::VendoredResolver::from_lockfile)
    /// construction when a package in the lockfile has no resolvable entry.
    #[error("entry not found for {name:?}: tried {}", format_paths(attempted))]
    EntryNotFound {
        name: String,
        attempted: Vec<PathBuf>,
    },
}

fn format_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}
