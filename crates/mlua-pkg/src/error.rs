//! Crate-wide error type for the PkgMgr subsystem.
//!
//! [`PkgError`] is the single error type for all PkgMgr operations
//! (manifest parsing, lockfile I/O, git fetch, CLI).  It is distinct from
//! the existing [`crate::ResolveError`], which covers runtime `resolve()` failures.
//! Additional variants are added by subsequent subtasks.

/// Crate-wide error for PkgMgr operations.
///
/// # Error taxonomy
///
/// | Variant | Phase | Source |
/// |---------|-------|--------|
/// | [`ManifestParse`](Self::ManifestParse) | Parse | [`toml::de::Error`] via `#[from]` |
/// | [`Io`](Self::Io) | I/O | [`std::io::Error`] via `#[from]` |
/// | [`Validation`](Self::Validation) | Post-parse | custom message |
///
/// Variants are `#[non_exhaustive]` so that future subtasks can add new
/// variants without breaking downstream `match` arms that include a wildcard.
///
/// The existing [`crate::ResolveError`] is intentionally not modified.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum PkgError {
    /// TOML parse failure when reading `mlua-pkg.toml` or `mlua-pkg.lock`.
    ///
    /// Automatically constructed from [`toml::de::Error`] via `?`.
    #[error("manifest parse error: {source}")]
    ManifestParse {
        #[from]
        source: toml::de::Error,
    },

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
}
