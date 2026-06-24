//! End-to-end integration smoke test for the PkgMgr facade (v0.4.0).
//!
//! **Network-free tests** (default): use a local `file://` git repository as
//! the package fixture.  No subprocess `git` invocations; all git operations
//! go through `git2`.
//!
//! **Network tests** (opt-in via `--ignored`): fetch the real
//! `https://github.com/ynishi/lshape` repo.  Skipped in CI by default.

use std::{
    fs,
    path::{Path, PathBuf},
};

use git2::Signature;
use mlua::Lua;
use mlua_pkg::{
    fetcher::{Fetcher, GitFetcher},
    lockfile::{LockedPkg, Lockfile},
    manifest::{Dep, Manifest},
    resolve_entry,
    resolvers::VendoredResolver,
    Registry,
};
use tempfile::TempDir;

// ── Fixture helpers ────────────────────────────────────────────────────────────

/// Create a minimal git repository in `dir` with the given `files` and an
/// annotated tag `tag_name`.
///
/// Returns the commit SHA (40-char hex).
fn make_local_git_repo(
    dir: &Path,
    files: &[(&str, &str)],
    tag_name: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let repo = git2::Repository::init(dir)?;
    let sig = Signature::now("test", "test@example.com")?;

    for (rel_path, content) in files {
        let full = dir.join(rel_path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&full, content)?;
    }

    let mut index = repo.index()?;
    index.add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)?;
    index.write()?;

    let tree_id = index.write_tree()?;
    let tree = repo.find_tree(tree_id)?;
    let commit_id = repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])?;
    let commit_obj = repo.find_commit(commit_id)?;
    repo.tag(tag_name, commit_obj.as_object(), &sig, "release", false)?;

    Ok(commit_id.to_string())
}

/// Write a minimal consumer `mlua-pkg.toml` to `dir` with one dep entry.
fn write_consumer_manifest(dir: &Path, pkg_name: &str, pkg_alias: &str, git_url: &str, tag: &str) {
    let content = format!(
        "[package]\n\
         name = \"{pkg_name}\"\n\
         version = \"0.0.1\"\n\
         \n\
         [deps]\n\
         {pkg_alias}.git = \"{git_url}\"\n\
         {pkg_alias}.tag = \"{tag}\"\n"
    );
    fs::write(dir.join("mlua-pkg.toml"), content).expect("write consumer manifest");
}

// ── Test: full install round-trip (network-free) ───────────────────────────────

/// End-to-end smoke: manifest → fetch → symlink → lockfile → VendoredResolver
/// → `require("foo")` in Lua.
///
/// Package contains `src/foo.lua` with `return { version = "0.1.0" }`.
#[test]
fn install_round_trip_local_file() -> Result<(), Box<dyn std::error::Error>> {
    // ── 1. Create author-side package repo ──────────────────────────────────────
    let pkg_repo = TempDir::new()?;
    // The package exposes `src/init.lua` so that `require("foo")` resolves via
    // the `vendored/foo` directory symlink → `<cache>/src/init.lua` path.
    make_local_git_repo(
        pkg_repo.path(),
        &[
            ("src/init.lua", "return { version = \"0.1.0\" }\n"),
            // mlua-pkg.toml for the author side (entry = "src")
            (
                "mlua-pkg.toml",
                "[package]\nname = \"foo\"\nversion = \"0.1.0\"\nentry = \"src\"\n",
            ),
        ],
        "v0.1.0",
    )?;
    let pkg_url = format!("file://{}", pkg_repo.path().display());

    // ── 2. Consumer workspace ───────────────────────────────────────────────────
    let workspace = TempDir::new()?;
    write_consumer_manifest(workspace.path(), "consumer", "foo", &pkg_url, "v0.1.0");

    let manifest_path = workspace.path().join("mlua-pkg.toml");
    let lock_path = workspace.path().join("mlua-pkg.lock");
    let cache_dir = workspace.path().join(".mlua-pkgs/cache");
    let vendored_dir = workspace.path().join(".mlua-pkgs/vendored");

    // ── 3. Parse consumer manifest ──────────────────────────────────────────────
    let manifest = Manifest::from_path(&manifest_path)?;
    assert_eq!(manifest.deps.len(), 1);

    // ── 4. Fetch ─────────────────────────────────────────────────────────────────
    let fetcher = GitFetcher::new(cache_dir.clone());
    let dep: &Dep = manifest.deps.get("foo").expect("dep 'foo' must exist");
    let fetched = fetcher.fetch(dep)?;

    assert!(!fetched.sha.is_empty(), "SHA must not be empty");
    assert_eq!(fetched.sha.len(), 40, "SHA must be 40-char hex");
    assert!(fetched.cache_path.exists(), "cache_path must exist on disk");

    // The fetched package should carry the author manifest.
    let author_manifest = fetched
        .manifest
        .as_ref()
        .expect("author mlua-pkg.toml should be parsed");
    assert_eq!(author_manifest.package.entry, Some(PathBuf::from("src")));

    // ── 5. Resolve entry ─────────────────────────────────────────────────────────
    let author_entry = author_manifest.package.entry.clone();
    let override_entry: Option<&Path> = dep.entry.as_deref().or(author_entry.as_deref());
    let entry_abs = resolve_entry(&fetched.cache_path, override_entry)?;
    assert!(
        entry_abs.exists(),
        "resolved entry must exist: {:?}",
        entry_abs
    );

    // ── 6. Create vendored symlink ───────────────────────────────────────────────
    fs::create_dir_all(&vendored_dir)?;
    let symlink_path = vendored_dir.join("foo");
    // absolute symlink is fine in tests
    #[cfg(unix)]
    std::os::unix::fs::symlink(&entry_abs, &symlink_path)?;
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&entry_abs, &symlink_path)?;

    // ── 7. Build lockfile ────────────────────────────────────────────────────────
    let entry_rel: PathBuf = match entry_abs.strip_prefix(&fetched.cache_path) {
        Ok(rel) if rel.as_os_str().is_empty() => PathBuf::from("."),
        Ok(rel) => rel.to_path_buf(),
        Err(_) => PathBuf::from("."),
    };
    let lockfile = Lockfile {
        version: 1,
        pkg: vec![LockedPkg {
            name: "foo".to_string(),
            source: format!("git+{pkg_url}"),
            tag: Some("v0.1.0".to_string()),
            rev: None,
            branch: None,
            sha: fetched.sha.clone(),
            entry: entry_rel,
        }],
    };
    lockfile.write(&lock_path)?;
    assert!(lock_path.exists(), "lockfile must be written");

    // ── 8. VendoredResolver + Registry ──────────────────────────────────────────
    let resolver = VendoredResolver::from_lockfile(&lock_path, &vendored_dir)?;

    let lua = Lua::new();
    let mut reg = Registry::new();
    reg.add(resolver);
    reg.install(&lua)?;

    // ── 9. require("foo") in Lua ─────────────────────────────────────────────────
    let version: String = lua.load("return require('foo').version").eval()?;
    assert_eq!(
        version, "0.1.0",
        "require('foo').version must equal '0.1.0'"
    );

    Ok(())
}

// ── Test: SHA fixation (idempotent fetch) ──────────────────────────────────────

/// Fetching the same dep twice must return the same SHA, and the cache
/// directory is reused (not re-cloned).
#[test]
fn sha_fixation_idempotent_fetch() -> Result<(), Box<dyn std::error::Error>> {
    let pkg_repo = TempDir::new()?;
    make_local_git_repo(
        pkg_repo.path(),
        &[("lib/bar.lua", "return 'bar'\n")],
        "v0.2.0",
    )?;
    let pkg_url = format!("file://{}", pkg_repo.path().display());

    let cache_dir = TempDir::new()?;
    let fetcher = GitFetcher::new(cache_dir.path().to_path_buf());

    let dep = Dep {
        git: pkg_url,
        tag: Some("v0.2.0".to_string()),
        rev: None,
        branch: None,
        entry: None,
    };

    let first = fetcher.fetch(&dep)?;
    let second = fetcher.fetch(&dep)?;

    assert_eq!(
        first.sha, second.sha,
        "SHA must be identical across two fetches"
    );
    assert_eq!(
        first.cache_path, second.cache_path,
        "cache_path must be identical (cache hit)"
    );
    assert!(first.cache_path.exists(), "cache_path must exist on disk");

    Ok(())
}

// ── Test: resolve_entry fallback chain ────────────────────────────────────────

/// When no override entry is given and neither `src/` nor `lua/` exists,
/// `resolve_entry` returns the cache root itself.
#[test]
fn resolve_entry_fallback_to_root() -> Result<(), Box<dyn std::error::Error>> {
    let pkg_repo = TempDir::new()?;
    make_local_git_repo(pkg_repo.path(), &[("init.lua", "return {}\n")], "v0.1.0")?;
    let pkg_url = format!("file://{}", pkg_repo.path().display());

    let cache_dir = TempDir::new()?;
    let fetcher = GitFetcher::new(cache_dir.path().to_path_buf());

    let dep = Dep {
        git: pkg_url,
        tag: Some("v0.1.0".to_string()),
        rev: None,
        branch: None,
        entry: None,
    };

    let fetched = fetcher.fetch(&dep)?;
    // No `src/` or `lua/` — should fall back to cache_path itself.
    let entry = resolve_entry(&fetched.cache_path, None)?;
    assert_eq!(
        entry, fetched.cache_path,
        "fallback entry must equal cache_path when src/ and lua/ are absent"
    );

    Ok(())
}

// ── Test: resolve_entry src/ priority ────────────────────────────────────────

/// When `src/` exists, `resolve_entry` should point to it.
#[test]
fn resolve_entry_prefers_src() -> Result<(), Box<dyn std::error::Error>> {
    let pkg_repo = TempDir::new()?;
    make_local_git_repo(
        pkg_repo.path(),
        &[("src/hello.lua", "return 'hello'\n")],
        "v0.1.0",
    )?;
    let pkg_url = format!("file://{}", pkg_repo.path().display());

    let cache_dir = TempDir::new()?;
    let fetcher = GitFetcher::new(cache_dir.path().to_path_buf());

    let dep = Dep {
        git: pkg_url,
        tag: Some("v0.1.0".to_string()),
        rev: None,
        branch: None,
        entry: None,
    };

    let fetched = fetcher.fetch(&dep)?;
    let entry = resolve_entry(&fetched.cache_path, None)?;
    let expected = fetched.cache_path.join("src");
    assert_eq!(
        entry, expected,
        "resolve_entry must prefer src/ when present"
    );

    Ok(())
}

// ── Network test (opt-in, #[ignore]) ─────────────────────────────────────────

/// Fetch the real `lshape` package from GitHub.
///
/// Skip by default (`cargo test`); run with:
/// ```
/// cargo test -p mlua-pkg --test pkgmgr_smoke -- --ignored
/// ```
#[test]
#[ignore = "requires network — opt-in via `cargo test -- --ignored`"]
fn install_lshape_from_github() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let cache_dir = workspace.path().join("cache");
    let vendored_dir = workspace.path().join("vendored");
    let lock_path = workspace.path().join("mlua-pkg.lock");

    let fetcher = GitFetcher::new(cache_dir.clone());

    let dep = Dep {
        git: "https://github.com/ynishi/lshape".to_string(),
        tag: Some("v0.1.0".to_string()),
        rev: None,
        branch: None,
        entry: None,
    };

    let fetched = fetcher.fetch(&dep)?;
    assert_eq!(fetched.sha.len(), 40, "SHA must be 40-char hex");
    assert!(fetched.cache_path.exists());

    let entry_abs = resolve_entry(&fetched.cache_path, None)?;
    assert!(entry_abs.exists());

    fs::create_dir_all(&vendored_dir)?;
    let symlink_path = vendored_dir.join("lshape");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&entry_abs, &symlink_path)?;
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&entry_abs, &symlink_path)?;

    let entry_rel: PathBuf = match entry_abs.strip_prefix(&fetched.cache_path) {
        Ok(rel) if rel.as_os_str().is_empty() => PathBuf::from("."),
        Ok(rel) => rel.to_path_buf(),
        Err(_) => PathBuf::from("."),
    };

    let lockfile = Lockfile {
        version: 1,
        pkg: vec![LockedPkg {
            name: "lshape".to_string(),
            source: "git+https://github.com/ynishi/lshape".to_string(),
            tag: Some("v0.1.0".to_string()),
            rev: None,
            branch: None,
            sha: fetched.sha.clone(),
            entry: entry_rel,
        }],
    };
    lockfile.write(&lock_path)?;

    // Verify SHA is stable across re-fetch (idempotent).
    let second = fetcher.fetch(&dep)?;
    assert_eq!(fetched.sha, second.sha, "lshape SHA must be stable");

    Ok(())
}

/// Regression: the fetched worktree must contain the **tag v0.1.0** content,
/// not the upstream HEAD content (`main` is at v0.2.0).  This guards against
/// the v0.4.0 bug where `fetch()` resolved the tag SHA correctly but left
/// the worktree on the cloned default branch HEAD.
#[test]
#[ignore = "requires network — opt-in via `cargo test -- --ignored`"]
fn install_lshape_v010_has_correct_content() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let cache_dir = workspace.path().join("cache");
    let fetcher = GitFetcher::new(cache_dir);

    let dep = Dep {
        git: "https://github.com/ynishi/lshape".to_string(),
        tag: Some("v0.1.0".to_string()),
        rev: None,
        branch: None,
        entry: None,
    };
    let fetched = fetcher.fetch(&dep)?;

    let init_lua = fs::read_to_string(fetched.cache_path.join("lshape/init.lua"))?;
    assert!(
        init_lua.contains(r#"M._VERSION = "0.1.0""#),
        "expected M._VERSION = \"0.1.0\" in the v0.1.0 tag content, \
         but found upstream HEAD content instead.  fetcher checked out the \
         wrong commit."
    );

    Ok(())
}
