//! `mlua-pkg.toml` manifest parser.
//!
//! A single [`Manifest`] type covers both consumer manifests
//! (with a populated `[deps]` table) and author manifests
//! (`[package]` only, with an optional `entry` field).
//!
//! # Consumer manifest example
//!
//! ```toml
//! [package]
//! name = "my-app"
//! version = "0.1.0"
//!
//! [deps]
//! foo  = { git = "https://github.com/x/foo", tag = "v1.2.0" }
//! bar  = { git = "https://github.com/y/bar", rev = "abc123" }
//! baz  = { git = "https://github.com/z/baz", branch = "main" }
//!
//! [deps.qux]
//! git   = "https://github.com/q/qux"
//! tag   = "v2.0.0"
//! entry = "lib"
//! ```
//!
//! # Author manifest example
//!
//! ```toml
//! [package]
//! name    = "foo"
//! version = "1.2.0"
//! entry   = "src"
//! ```

use std::{collections::HashMap, fs, path::{Path, PathBuf}};

use serde::{Deserialize, Serialize};

use crate::PkgError;

// ── Package ───────────────────────────────────────────────────────────────────

/// `[package]` section of `mlua-pkg.toml`.
///
/// Present in both consumer and author manifests.  The `entry` field is used
/// by author-side manifests to declare the Lua `require` root; consumer-side
/// manifests typically omit it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    /// Package name (must be unique within a consumer's dependency graph).
    pub name: String,

    /// Package version string (SemVer expected; not enforced at parse time).
    pub version: String,

    /// Entry root for Lua `require` resolution.
    ///
    /// Used by author-side manifests.  When absent, the fallback chain
    /// (`src/` → `lua/` → repo root) is applied at install time (ST4).
    pub entry: Option<PathBuf>,
}

// ── Dep ──────────────────────────────────────────────────────────────────────

/// A git-based dependency declared in `[deps]`.
///
/// At most one of `tag`, `rev`, or `branch` may be set.  All three being
/// absent is accepted at parse time (treated as HEAD resolution); hard
/// enforcement is deferred to the fetcher (ST3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dep {
    /// Remote git URL (required).
    pub git: String,

    /// Pin to a specific tag.  Mutually exclusive with `rev` and `branch`.
    pub tag: Option<String>,

    /// Pin to a specific commit SHA.  Mutually exclusive with `tag` and `branch`.
    pub rev: Option<String>,

    /// Track a branch (non-reproducible).  Mutually exclusive with `tag` and `rev`.
    pub branch: Option<String>,

    /// Override the Lua `require` entry root for this dependency.
    ///
    /// Takes precedence over the author's own `[package].entry`.
    /// When absent, the author's `entry` (or the fallback chain) applies.
    pub entry: Option<PathBuf>,
}

impl Dep {
    /// Validate that at most one of `tag`, `rev`, `branch` is set.
    fn validate_ref_exclusivity(&self, dep_name: &str) -> Result<(), PkgError> {
        let count = [self.tag.is_some(), self.rev.is_some(), self.branch.is_some()]
            .into_iter()
            .filter(|&b| b)
            .count();
        if count > 1 {
            return Err(PkgError::Validation {
                message: format!(
                    "dep '{dep_name}': only one of `tag`, `rev`, `branch` may be specified, \
                     but multiple are set"
                ),
            });
        }
        Ok(())
    }
}

// ── Manifest ─────────────────────────────────────────────────────────────────

/// Parsed `mlua-pkg.toml`.
///
/// Shared schema for consumer and author manifests.  The `deps` map is empty
/// for author-side manifests (packages that are depended upon, not consumers).
///
/// Unknown top-level keys cause an immediate parse error
/// (`#[serde(deny_unknown_fields)]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// `[package]` section — required in all manifests.
    pub package: Package,

    /// `[deps]` table — optional; absent in author-side manifests.
    ///
    /// Keys are the local package alias used in `require()`.
    #[serde(default)]
    pub deps: HashMap<String, Dep>,
}

impl Manifest {
    /// Read and parse a `mlua-pkg.toml` file at `path`.
    ///
    /// Performs post-parse validation after TOML deserialization:
    /// - Each `[deps]` entry must specify at most one of `tag`, `rev`, `branch`.
    ///
    /// # Errors
    ///
    /// - [`PkgError::Io`] — file cannot be read.
    /// - [`PkgError::ManifestParse`] — TOML is syntactically invalid, a
    ///   required field is missing, an unknown field is present, or a type
    ///   mismatch occurs.
    /// - [`PkgError::Validation`] — post-parse invariants are violated (e.g.
    ///   `tag` and `rev` both set on the same dependency).
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, PkgError> {
        let content = fs::read_to_string(path)?;
        let manifest: Self = toml::from_str(&content)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Run post-parse semantic validation across all dependency entries.
    fn validate(&self) -> Result<(), PkgError> {
        for (name, dep) in &self.deps {
            dep.validate_ref_exclusivity(name)?;
        }
        Ok(())
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Write `content` to a temporary file and return the handle.
    /// The file is deleted when the handle is dropped.
    fn temp_manifest(content: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    // ── 1. Consumer happy path ─────────────────────────────────────────────

    #[test]
    fn consumer_happy_path() {
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"

[deps]
foo = { git = "https://github.com/x/foo", tag = "v1.2.0" }
bar = { git = "https://github.com/y/bar", rev = "abc123" }
baz = { git = "https://github.com/z/baz", branch = "main" }

[deps.qux]
git   = "https://github.com/q/qux"
tag   = "v2.0.0"
entry = "lib"
"#;
        let f = temp_manifest(toml);
        let m = Manifest::from_path(f.path()).unwrap();

        assert_eq!(m.package.name, "my-app");
        assert_eq!(m.package.version, "0.1.0");
        assert!(m.package.entry.is_none());
        assert_eq!(m.deps.len(), 4);

        let foo = &m.deps["foo"];
        assert_eq!(foo.git, "https://github.com/x/foo");
        assert_eq!(foo.tag.as_deref(), Some("v1.2.0"));
        assert!(foo.rev.is_none());
        assert!(foo.branch.is_none());
        assert!(foo.entry.is_none());

        let bar = &m.deps["bar"];
        assert_eq!(bar.rev.as_deref(), Some("abc123"));
        assert!(bar.tag.is_none());

        let baz = &m.deps["baz"];
        assert_eq!(baz.branch.as_deref(), Some("main"));
        assert!(baz.tag.is_none());

        let qux = &m.deps["qux"];
        assert_eq!(qux.tag.as_deref(), Some("v2.0.0"));
        assert_eq!(qux.entry, Some(PathBuf::from("lib")));
    }

    // ── 2. Author happy path ───────────────────────────────────────────────

    #[test]
    fn author_happy_path() {
        let toml = r#"
[package]
name    = "foo"
version = "1.2.0"
entry   = "src"
"#;
        let f = temp_manifest(toml);
        let m = Manifest::from_path(f.path()).unwrap();

        assert_eq!(m.package.name, "foo");
        assert_eq!(m.package.version, "1.2.0");
        assert_eq!(m.package.entry, Some(PathBuf::from("src")));
        assert!(m.deps.is_empty());
    }

    // ── 3. tag + rev are mutually exclusive ───────────────────────────────

    #[test]
    fn tag_and_rev_mutually_exclusive() {
        let toml = r#"
[package]
name    = "my-app"
version = "0.1.0"

[deps.bad]
git = "https://github.com/x/bad"
tag = "v1.0.0"
rev = "abc123"
"#;
        let f = temp_manifest(toml);
        let err = Manifest::from_path(f.path()).unwrap_err();
        assert!(
            matches!(err, PkgError::Validation { .. }),
            "expected Validation error, got: {err}"
        );
        assert!(err.to_string().contains("bad"));
    }

    // ── 4. Invalid TOML ───────────────────────────────────────────────────

    #[test]
    fn invalid_toml_returns_parse_error() {
        let toml = "this is not valid = [ toml";
        let f = temp_manifest(toml);
        let err = Manifest::from_path(f.path()).unwrap_err();
        assert!(
            matches!(err, PkgError::ManifestParse { .. }),
            "expected ManifestParse error, got: {err}"
        );
    }

    // ── 5. Missing [package] section ──────────────────────────────────────

    #[test]
    fn missing_package_section_returns_parse_error() {
        let toml = r#"
[deps]
foo = { git = "https://github.com/x/foo", tag = "v1.0.0" }
"#;
        let f = temp_manifest(toml);
        let err = Manifest::from_path(f.path()).unwrap_err();
        assert!(
            matches!(err, PkgError::ManifestParse { .. }),
            "expected ManifestParse error for missing [package], got: {err}"
        );
    }

    // ── 6. Round-trip serialization (Serialize derive ground-truth) ────────

    #[test]
    fn round_trip_serialize_deserialize() {
        let original = Manifest {
            package: Package {
                name: "roundtrip".into(),
                version: "0.1.0".into(),
                entry: None,
            },
            deps: {
                let mut m = HashMap::new();
                m.insert(
                    "lib".into(),
                    Dep {
                        git: "https://github.com/x/lib".into(),
                        tag: Some("v1.0.0".into()),
                        rev: None,
                        branch: None,
                        entry: None,
                    },
                );
                m
            },
        };

        let serialized = toml::to_string(&original).unwrap();
        let deserialized: Manifest = toml::from_str(&serialized).unwrap();
        assert_eq!(original, deserialized);
    }

    // ── extra: all three ref fields set is also a validation error ────────

    #[test]
    fn all_three_ref_fields_is_validation_error() {
        let toml = r#"
[package]
name    = "my-app"
version = "0.1.0"

[deps.oops]
git    = "https://github.com/x/oops"
tag    = "v1.0.0"
rev    = "abc123"
branch = "main"
"#;
        let f = temp_manifest(toml);
        let err = Manifest::from_path(f.path()).unwrap_err();
        assert!(matches!(err, PkgError::Validation { .. }));
    }

    // ── extra: unknown field in [package] is rejected ─────────────────────

    #[test]
    fn unknown_field_in_package_is_rejected() {
        let toml = r#"
[package]
name    = "my-app"
version = "0.1.0"
unknown = "should-fail"
"#;
        let f = temp_manifest(toml);
        let err = Manifest::from_path(f.path()).unwrap_err();
        assert!(
            matches!(err, PkgError::ManifestParse { .. }),
            "expected ManifestParse error for unknown field, got: {err}"
        );
    }
}
