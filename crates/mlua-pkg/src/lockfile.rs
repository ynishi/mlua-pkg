//! `mlua-pkg.lock` read / write.
//!
//! A [`Lockfile`] captures the resolved package graph at a point in time.
//! Each `[[pkg]]` entry pins a dependency to a specific commit SHA for
//! fully-reproducible installs.
//!
//! # Schema (TOML)
//!
//! ```toml
//! version = 1
//!
//! [[pkg]]
//! name   = "foo"
//! source = "git+https://github.com/x/foo"
//! tag    = "v1.2.0"
//! sha    = "abc123def456..."   # full 40-char SHA
//! entry  = "src"
//!
//! [[pkg]]
//! name   = "bar"
//! source = "git+https://github.com/y/bar"
//! rev    = "abc123"
//! sha    = "def456..."
//! entry  = "src"
//! ```
//!
//! # Stability
//!
//! The lockfile is intended to be committed to version control.  [`Lockfile::write`]
//! sorts packages by name before serializing, producing diff-stable output.
//!
//! The `entry` field is always stored with forward-slash separators (`/`) to
//! remain portable across platforms.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::PkgError;

// ── Lockfile ──────────────────────────────────────────────────────────────────

/// Root structure of `mlua-pkg.lock`.
///
/// Use [`Lockfile::read`] to load an existing lockfile and [`Lockfile::write`]
/// to persist one.  [`Lockfile::default`] creates an empty lockfile with
/// `version = 1`.
///
/// Unknown top-level keys cause an immediate parse error
/// (`#[serde(deny_unknown_fields)]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lockfile {
    /// Schema version.  Always `1` in this implementation.
    ///
    /// Future tooling may bump this number and apply a migration before
    /// deserializing the rest of the file.
    pub version: u32,

    /// Locked package entries.  Written as `[[pkg]]` in TOML.
    ///
    /// May be empty for a newly-initialized lockfile (no deps installed yet).
    #[serde(rename = "pkg", default, skip_serializing_if = "Vec::is_empty")]
    pub pkg: Vec<LockedPkg>,
}

impl Default for Lockfile {
    fn default() -> Self {
        Self {
            version: 1,
            pkg: Vec::new(),
        }
    }
}

// ── LockedPkg ─────────────────────────────────────────────────────────────────

/// A single locked package entry in `[[pkg]]`.
///
/// Pins one dependency to an exact commit SHA together with the metadata
/// needed to re-resolve or update it in the future.
///
/// Unknown keys cause an immediate parse error (`#[serde(deny_unknown_fields)]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedPkg {
    /// Local package alias.  Must be unique within a lockfile.
    pub name: String,

    /// Source URL with protocol prefix, e.g. `"git+https://github.com/x/foo"`.
    ///
    /// The `git+` prefix follows Cargo lock convention and leaves room for
    /// future `http+` or `luarocks+` sources.
    pub source: String,

    /// Git tag used to resolve this package (if any).
    ///
    /// At most one of `tag`, `rev`, `branch` is expected to be set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,

    /// Git revision (commit SHA short-or-full) supplied by the consumer manifest
    /// (if any).
    ///
    /// When `rev` is set, `sha` must equal its fully-resolved commit SHA.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,

    /// Git branch this package was resolved from (if any).
    ///
    /// Non-reproducible by nature; the resolved commit is captured in `sha`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,

    /// Full 40-character commit SHA that pins this package.
    ///
    /// This is the canonical reproducibility anchor.  Short SHAs are **not**
    /// accepted; the GitFetcher (ST3) always returns the full SHA.
    pub sha: String,

    /// Resolved Lua `require` entry path within the vendored directory.
    ///
    /// Stored with forward-slash separators (`"src"`, `"lua"`, `"."`) for
    /// portability across platforms.
    #[serde(with = "entry_serde")]
    pub entry: PathBuf,
}

// ── Path serde helper ─────────────────────────────────────────────────────────

/// Custom serde (de)serialization for `PathBuf` fields that must be stored
/// as forward-slash strings in TOML.
mod entry_serde {
    use std::path::{Path, PathBuf};

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
        // Replace backslashes with forward slashes for Windows portability.
        let s = path.to_string_lossy().replace('\\', "/");
        serializer.serialize_str(&s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<PathBuf, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(PathBuf::from(s))
    }
}

// ── Lockfile impl ─────────────────────────────────────────────────────────────

impl Lockfile {
    /// Read and parse a `mlua-pkg.lock` file at `path`.
    ///
    /// # Errors
    ///
    /// | Error | Condition |
    /// |-------|-----------|
    /// | [`PkgError::MissingLockfile`] | File does not exist |
    /// | [`PkgError::LockfileParse`] | Invalid TOML or unknown / missing fields |
    /// | [`PkgError::SameNameConflict`] | Duplicate `name` in `[[pkg]]` entries |
    /// | [`PkgError::Io`] | Other I/O failure |
    pub fn read(path: impl AsRef<Path>) -> Result<Self, PkgError> {
        let path = path.as_ref();

        let content = fs::read_to_string(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                PkgError::MissingLockfile {
                    path: path.to_path_buf(),
                }
            } else {
                PkgError::Io { source: e }
            }
        })?;

        let lockfile: Self =
            toml::from_str(&content).map_err(|source| PkgError::LockfileParse { source })?;

        // Defense-in-depth: detect duplicate package names early.
        let mut seen: HashSet<&str> = HashSet::with_capacity(lockfile.pkg.len());
        for pkg in &lockfile.pkg {
            if !seen.insert(pkg.name.as_str()) {
                return Err(PkgError::SameNameConflict {
                    name: pkg.name.clone(),
                });
            }
        }

        Ok(lockfile)
    }

    /// Write the lockfile to `path`.
    ///
    /// Packages are sorted by name before writing to produce diff-stable
    /// output suitable for version control.
    ///
    /// This implementation uses [`fs::write`] (not atomic).  Atomic write
    /// via `tempfile::NamedTempFile::persist` is a planned enhancement for
    /// a future subtask.
    ///
    /// # Errors
    ///
    /// | Error | Condition |
    /// |-------|-----------|
    /// | [`PkgError::LockfileWrite`] | TOML serialization failed |
    /// | [`PkgError::Io`] | File write failed |
    pub fn write(&self, path: impl AsRef<Path>) -> Result<(), PkgError> {
        // Sort a clone by name for diff-stable output.
        let mut sorted_pkg = self.pkg.clone();
        sorted_pkg.sort_by(|a, b| a.name.cmp(&b.name));

        let to_serialize = Self {
            version: self.version,
            pkg: sorted_pkg,
        };

        // `?` auto-converts toml::ser::Error → PkgError::LockfileWrite via #[from].
        let content = toml::to_string_pretty(&to_serialize)?;

        // `?` auto-converts std::io::Error → PkgError::Io via #[from].
        fs::write(path, content)?;

        Ok(())
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Write `content` to a temp file and return the handle (deleted on drop).
    fn write_temp(content: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    /// Build a minimal [`LockedPkg`] fixture with tag-based ref.
    fn pkg_tag(name: &str, sha_char: char) -> LockedPkg {
        LockedPkg {
            name: name.to_owned(),
            source: format!("git+https://github.com/x/{name}"),
            tag: Some("v1.0.0".to_owned()),
            rev: None,
            branch: None,
            sha: sha_char.to_string().repeat(40),
            entry: PathBuf::from("src"),
        }
    }

    // ── 1. Empty lockfile ────────────────────────────────────────────────────

    #[test]
    fn read_empty_lockfile() {
        let toml = "version = 1\n";
        let f = write_temp(toml);
        let lf = Lockfile::read(f.path()).unwrap();
        assert_eq!(lf.version, 1);
        assert!(lf.pkg.is_empty());
    }

    // ── 2. Single package ────────────────────────────────────────────────────

    #[test]
    fn read_single_pkg() {
        let toml = r#"
version = 1

[[pkg]]
name   = "foo"
source = "git+https://github.com/x/foo"
tag    = "v1.2.0"
sha    = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
entry  = "src"
"#;
        let f = write_temp(toml);
        let lf = Lockfile::read(f.path()).unwrap();

        assert_eq!(lf.version, 1);
        assert_eq!(lf.pkg.len(), 1);

        let pkg = &lf.pkg[0];
        assert_eq!(pkg.name, "foo");
        assert_eq!(pkg.source, "git+https://github.com/x/foo");
        assert_eq!(pkg.tag.as_deref(), Some("v1.2.0"));
        assert!(pkg.rev.is_none());
        assert!(pkg.branch.is_none());
        assert_eq!(pkg.sha, "a".repeat(40));
        assert_eq!(pkg.entry, PathBuf::from("src"));
    }

    // ── 3. Multiple packages (tag / rev / branch each) ───────────────────────

    #[test]
    fn read_multiple_pkgs() {
        let toml = r#"
version = 1

[[pkg]]
name   = "foo"
source = "git+https://github.com/x/foo"
tag    = "v1.2.0"
sha    = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
entry  = "src"

[[pkg]]
name   = "bar"
source = "git+https://github.com/y/bar"
rev    = "deadbeef"
sha    = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
entry  = "lua"

[[pkg]]
name   = "baz"
source = "git+https://github.com/z/baz"
branch = "main"
sha    = "cccccccccccccccccccccccccccccccccccccccc"
entry  = "."
"#;
        let f = write_temp(toml);
        let lf = Lockfile::read(f.path()).unwrap();

        assert_eq!(lf.pkg.len(), 3);

        // Check each pkg in declaration order.
        assert_eq!(lf.pkg[0].name, "foo");
        assert_eq!(lf.pkg[0].tag.as_deref(), Some("v1.2.0"));

        assert_eq!(lf.pkg[1].name, "bar");
        assert_eq!(lf.pkg[1].rev.as_deref(), Some("deadbeef"));

        assert_eq!(lf.pkg[2].name, "baz");
        assert_eq!(lf.pkg[2].branch.as_deref(), Some("main"));
        assert_eq!(lf.pkg[2].entry, PathBuf::from("."));
    }

    // ── 4. Round-trip: write → read produces identical Lockfile ─────────────

    #[test]
    fn round_trip_write_then_read() {
        // Original is already name-sorted so write order matches.
        let original = Lockfile {
            version: 1,
            pkg: vec![
                LockedPkg {
                    name: "alib".to_owned(),
                    source: "git+https://github.com/a/alib".to_owned(),
                    tag: None,
                    rev: Some("abc123".to_owned()),
                    branch: None,
                    sha: "a".repeat(40),
                    entry: PathBuf::from("lua"),
                },
                LockedPkg {
                    name: "zlib".to_owned(),
                    source: "git+https://github.com/z/zlib".to_owned(),
                    tag: Some("v1.0.0".to_owned()),
                    rev: None,
                    branch: None,
                    sha: "z".repeat(40),
                    entry: PathBuf::from("src"),
                },
            ],
        };

        let f = tempfile::NamedTempFile::new().unwrap();
        original.write(f.path()).unwrap();
        let loaded = Lockfile::read(f.path()).unwrap();

        assert_eq!(original, loaded);
    }

    // ── 4b. write sorts by name ───────────────────────────────────────────────

    #[test]
    fn write_sorts_by_name() {
        // Insert in reverse-alphabetical order.
        let lf = Lockfile {
            version: 1,
            pkg: vec![
                pkg_tag("zeta", 'z'),
                pkg_tag("alpha", 'a'),
                pkg_tag("mu", 'm'),
            ],
        };

        let f = tempfile::NamedTempFile::new().unwrap();
        lf.write(f.path()).unwrap();
        let loaded = Lockfile::read(f.path()).unwrap();

        assert_eq!(loaded.pkg[0].name, "alpha");
        assert_eq!(loaded.pkg[1].name, "mu");
        assert_eq!(loaded.pkg[2].name, "zeta");
    }

    // ── 5. Missing file → PkgError::MissingLockfile ──────────────────────────

    #[test]
    fn missing_file_returns_missing_lockfile_error() {
        let path = PathBuf::from("/nonexistent/dir/mlua-pkg.lock");
        let err = Lockfile::read(&path).unwrap_err();
        assert!(
            matches!(err, PkgError::MissingLockfile { .. }),
            "expected MissingLockfile, got: {err}"
        );
    }

    // ── 6. Invalid TOML → PkgError::LockfileParse ───────────────────────────

    #[test]
    fn invalid_toml_returns_lockfile_parse_error() {
        let f = write_temp("this is not = [ valid toml");
        let err = Lockfile::read(f.path()).unwrap_err();
        assert!(
            matches!(err, PkgError::LockfileParse { .. }),
            "expected LockfileParse, got: {err}"
        );
    }

    // ── 7. Duplicate name → PkgError::SameNameConflict ──────────────────────

    #[test]
    fn duplicate_name_returns_same_name_conflict() {
        let toml = r#"
version = 1

[[pkg]]
name   = "foo"
source = "git+https://github.com/x/foo"
sha    = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
entry  = "src"

[[pkg]]
name   = "foo"
source = "git+https://github.com/y/foo"
sha    = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
entry  = "lib"
"#;
        let f = write_temp(toml);
        let err = Lockfile::read(f.path()).unwrap_err();
        assert!(
            matches!(&err, PkgError::SameNameConflict { name } if name == "foo"),
            "expected SameNameConflict for 'foo', got: {err}"
        );
    }

    // ── 8. default() produces version=1, empty pkg ──────────────────────────

    #[test]
    fn default_lockfile_is_version_1_empty() {
        let lf = Lockfile::default();
        assert_eq!(lf.version, 1);
        assert!(lf.pkg.is_empty());
    }
}
