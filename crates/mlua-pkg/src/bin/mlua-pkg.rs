//! `mlua-pkg` CLI — install / add / update / clean.
//!
//! Each subcommand delegates to a free function (`run_*`) that accepts
//! explicit path arguments, which makes them unit-testable without touching
//! the process working directory.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use clap::{Parser, Subcommand};

use mlua_pkg::{
    fetcher::{Fetcher, GitFetcher},
    lockfile::{LockedPkg, Lockfile},
    manifest::{Dep, Manifest, Package},
    resolve_entry, PkgError,
};

// ── CLI definition ─────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "mlua-pkg",
    about = "Lua package manager for mlua",
    version,
    author
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Fetch all packages listed in mlua-pkg.toml and write mlua-pkg.lock.
    Install,

    /// Add a new dependency to mlua-pkg.toml (run `install` afterwards to fetch).
    Add {
        /// Local package alias used in `require()`.
        name: String,
        /// Remote git URL.
        git: String,
        /// Pin to a specific tag.
        #[arg(long)]
        tag: Option<String>,
        /// Pin to a specific commit revision.
        #[arg(long)]
        rev: Option<String>,
        /// Track a branch (non-reproducible).
        #[arg(long)]
        branch: Option<String>,
        /// Override the Lua `require()` entry subdir.
        #[arg(long)]
        entry: Option<PathBuf>,
        /// Physically vendor the entry into this directory (manifest-relative).
        #[arg(long)]
        target_dir: Option<PathBuf>,
    },

    /// Refresh deps: bump SemVer-prefix tag pins, refresh branch pins, re-install.
    Update {
        /// Package name to update.  When omitted, all deps are considered.
        name: Option<String>,
        /// Show planned changes without writing the manifest or running install.
        #[arg(long)]
        dry_run: bool,
        /// Also bump exact (full SemVer) tag pins to the latest release.
        #[arg(long)]
        force: bool,
    },

    /// Remove stale cached packages not referenced by the lockfile.
    Clean {
        /// Remove the entire cache directory, not just stale entries.
        #[arg(long)]
        all: bool,
    },
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Strip the redundant `mlua-pkg` arg that Cargo injects when the binary is
/// invoked as `cargo mlua-pkg ...` (Cargo dispatches `cargo <name> <args...>`
/// to `cargo-<name>` with `<name>` re-added as `args[1]`).
fn strip_cargo_subcommand<I>(args: I) -> Vec<String>
where
    I: IntoIterator<Item = String>,
{
    let mut args: Vec<String> = args.into_iter().collect();
    if args.get(1).map(String::as_str) == Some("mlua-pkg") {
        args.remove(1);
    }
    args
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse_from(strip_cargo_subcommand(std::env::args()));
    match cli.cmd {
        Cmd::Install => run_install(
            Path::new("mlua-pkg.toml"),
            Path::new(".mlua-pkgs/cache"),
            Path::new(".mlua-pkgs/vendored"),
            Path::new("mlua-pkg.lock"),
        ),
        Cmd::Add {
            name,
            git,
            tag,
            rev,
            branch,
            entry,
            target_dir,
        } => run_add(
            Path::new("mlua-pkg.toml"),
            name,
            git,
            tag,
            rev,
            branch,
            entry,
            target_dir,
        ),
        Cmd::Update {
            name,
            dry_run,
            force,
        } => run_update(
            name,
            Path::new("mlua-pkg.toml"),
            Path::new(".mlua-pkgs/cache"),
            Path::new(".mlua-pkgs/vendored"),
            Path::new("mlua-pkg.lock"),
            dry_run,
            force,
        ),
        Cmd::Clean { all } => run_clean(
            all,
            Path::new(".mlua-pkgs/cache"),
            Path::new("mlua-pkg.lock"),
        ),
    }
}

// ── install ───────────────────────────────────────────────────────────────────

/// Core install logic — testable with explicit paths.
fn run_install(
    manifest_path: &Path,
    cache_dir: &Path,
    vendored_dir: &Path,
    lock_path: &Path,
) -> anyhow::Result<()> {
    let manifest = Manifest::from_path(manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;

    let fetcher = GitFetcher::new(cache_dir.to_path_buf());
    std::fs::create_dir_all(vendored_dir)?;

    let mut locked_pkgs: Vec<LockedPkg> = Vec::with_capacity(manifest.deps.len());
    // Belt-and-suspenders same-name guard (HashMap keys are already unique for
    // direct deps; this guard becomes meaningful when transitive deps are added).
    let mut seen_names: HashSet<String> = HashSet::new();

    for (name, dep) in &manifest.deps {
        if !seen_names.insert(name.clone()) {
            return Err(PkgError::SameNameConflict { name: name.clone() }.into());
        }

        let fetched = fetcher
            .fetch(dep)
            .with_context(|| format!("fetching '{name}'"))?;

        // Author manifest version-assert: warn on tag mismatch, don't hard-error.
        if let Some(author) = &fetched.manifest {
            if let Some(req_tag) = &dep.tag {
                let av = &author.package.version;
                let normalized = req_tag.strip_prefix('v').unwrap_or(req_tag.as_str());
                if av != req_tag && av != normalized {
                    eprintln!(
                        "warning: {name}: requested tag '{req_tag}' vs \
                         author manifest version '{av}'"
                    );
                }
            }
        }

        // Entry resolution: dep.entry > author-manifest entry > fallback chain.
        let author_entry: Option<PathBuf> = fetched
            .manifest
            .as_ref()
            .and_then(|m| m.package.entry.clone());
        let override_entry: Option<&Path> = dep.entry.as_deref().or(author_entry.as_deref());

        let entry_abs = resolve_entry(&fetched.cache_path, override_entry)
            .with_context(|| format!("resolving entry for '{name}'"))?;

        if let Some(rel_target_dir) = &dep.target_dir {
            // Physical vendor copy: manifest-relative directory, idempotent.
            let manifest_root = manifest_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            let dest = manifest_root.join(rel_target_dir);
            copy_entry_into(&entry_abs, &dest)
                .with_context(|| format!("vendoring '{name}' into {}", dest.display()))?;
        } else {
            // Default: relative symlink .mlua-pkgs/vendored/<name> → ../cache/git/…
            let symlink_path = vendored_dir.join(name);
            if symlink_path.symlink_metadata().is_ok() {
                remove_symlink(&symlink_path)?;
            }
            let rel_target = relative_path(vendored_dir, &entry_abs)?;
            create_symlink(&rel_target, &symlink_path)?;
        }

        // Compute entry relative to the package cache root for the lockfile.
        let entry = entry_rel_to_pkg(&fetched.cache_path, &entry_abs);

        locked_pkgs.push(LockedPkg {
            name: name.clone(),
            source: format!("git+{}", dep.git),
            tag: dep.tag.clone(),
            rev: dep.rev.clone(),
            branch: dep.branch.clone(),
            sha: fetched.sha,
            entry,
        });
    }

    let lockfile = Lockfile {
        version: 1,
        pkg: locked_pkgs,
    };
    lockfile.write(lock_path)?;

    println!("installed {} package(s)", manifest.deps.len());
    Ok(())
}

/// Recursively copy the contents of `src` into `dest`, removing `dest` first
/// so the copy is idempotent (subsequent installs reflect upstream changes /
/// renames / deletions).
fn copy_entry_into(src: &Path, dest: &Path) -> std::io::Result<()> {
    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }
    std::fs::create_dir_all(dest)?;
    copy_dir_contents(src, dest)
}

fn copy_dir_contents(src: &Path, dest: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_dir() {
            std::fs::create_dir_all(&to)?;
            copy_dir_contents(&from, &to)?;
        } else if ft.is_symlink() {
            // Materialise into a regular file/dir (vendored output should not
            // depend on the cache symlink topology).
            let resolved = std::fs::canonicalize(&from)?;
            if resolved.is_dir() {
                std::fs::create_dir_all(&to)?;
                copy_dir_contents(&resolved, &to)?;
            } else {
                std::fs::copy(&resolved, &to)?;
            }
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Strip `cache_path` prefix from `entry_abs`; return `"."` for the repo root.
fn entry_rel_to_pkg(cache_path: &Path, entry_abs: &Path) -> PathBuf {
    match entry_abs.strip_prefix(cache_path) {
        Ok(rel) if rel.as_os_str().is_empty() => PathBuf::from("."),
        Ok(rel) => rel.to_path_buf(),
        Err(_) => PathBuf::from("."),
    }
}

// ── add ───────────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn run_add(
    manifest_path: &Path,
    name: String,
    git: String,
    tag: Option<String>,
    rev: Option<String>,
    branch: Option<String>,
    entry: Option<PathBuf>,
    target_dir: Option<PathBuf>,
) -> anyhow::Result<()> {
    // Validate ref-field exclusivity up front.
    let ref_count = [tag.is_some(), rev.is_some(), branch.is_some()]
        .into_iter()
        .filter(|&b| b)
        .count();
    if ref_count > 1 {
        return Err(anyhow::anyhow!(
            "at most one of --tag, --rev, --branch may be specified"
        ));
    }

    // Load existing manifest or synthesise a minimal one.
    let mut manifest = if manifest_path.exists() {
        Manifest::from_path(manifest_path)
            .with_context(|| format!("reading {}", manifest_path.display()))?
    } else {
        let pkg_name = std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "my-project".to_string());
        Manifest {
            package: Package {
                name: pkg_name,
                version: "0.1.0".to_string(),
                entry: None,
            },
            deps: HashMap::new(),
        }
    };

    let dep = Dep {
        git,
        tag,
        rev,
        branch,
        entry,
        target_dir,
    };
    let existed = manifest.deps.insert(name.clone(), dep).is_some();

    let toml_str = toml::to_string(&manifest)?;
    std::fs::write(manifest_path, toml_str)?;

    if existed {
        println!(
            "updated '{}' in {}; run 'mlua-pkg install' to fetch",
            name,
            manifest_path.display()
        );
    } else {
        println!(
            "added '{}' to {}; run 'mlua-pkg install' to fetch",
            name,
            manifest_path.display()
        );
    }
    Ok(())
}

// ── update ────────────────────────────────────────────────────────────────────

/// Classification of a `[deps.<name>] tag = "..."` value.
#[derive(Debug, PartialEq, Eq)]
enum TagPin {
    /// Full SemVer (X.Y.Z), optionally with pre-release. `update` leaves it
    /// alone unless `--force` is passed.
    Exact,
    /// Partial SemVer (e.g. "v1.0", "v1") interpreted as a prefix that
    /// auto-follows the latest matching release.  The numeric components are
    /// returned in left-to-right order.
    Prefix(Vec<u64>),
}

/// Strip a leading `v`/`V` from a tag string.
fn strip_v_prefix(s: &str) -> &str {
    s.strip_prefix('v')
        .or_else(|| s.strip_prefix('V'))
        .unwrap_or(s)
}

/// Classify the manifest `tag` field into [`TagPin::Exact`] or
/// [`TagPin::Prefix`].  Returns `None` for unparseable values
/// (e.g. `"latest"`, `"feature-branch"`); such pins are skipped on update.
fn classify_tag_pin(tag: &str) -> Option<TagPin> {
    let stripped = strip_v_prefix(tag);
    if semver::Version::parse(stripped).is_ok() {
        return Some(TagPin::Exact);
    }
    let parts: Option<Vec<u64>> = stripped.split('.').map(|s| s.parse::<u64>().ok()).collect();
    let parts = parts?;
    if parts.is_empty() || parts.len() >= 3 {
        return None;
    }
    Some(TagPin::Prefix(parts))
}

/// Pick the SemVer-max tag from `tags` whose components match `pin`'s prefix.
/// Pre-release tags (e.g. `v1.0.0-rc1`) are excluded.
///
/// Returns the original (un-stripped) tag string so the manifest can be
/// rewritten verbatim.
fn pick_latest_for_pin(tags: &[String], pin: &[u64]) -> Option<String> {
    let mut best: Option<(semver::Version, &String)> = None;
    for tag in tags {
        let stripped = strip_v_prefix(tag);
        let Ok(ver) = semver::Version::parse(stripped) else {
            continue;
        };
        if !ver.pre.is_empty() {
            continue;
        }
        let comps = [ver.major, ver.minor, ver.patch];
        if pin.iter().zip(comps.iter()).any(|(p, c)| p != c) {
            continue;
        }
        if pin.len() > comps.len() {
            continue;
        }
        match &best {
            None => best = Some((ver, tag)),
            Some((b, _)) if &ver > b => best = Some((ver, tag)),
            _ => {}
        }
    }
    best.map(|(_, t)| t.clone())
}

/// Pick the SemVer-max release tag (used for `--force` on exact pins).
fn pick_latest_overall(tags: &[String]) -> Option<String> {
    let mut best: Option<(semver::Version, &String)> = None;
    for tag in tags {
        let stripped = strip_v_prefix(tag);
        let Ok(ver) = semver::Version::parse(stripped) else {
            continue;
        };
        if !ver.pre.is_empty() {
            continue;
        }
        match &best {
            None => best = Some((ver, tag)),
            Some((b, _)) if &ver > b => best = Some((ver, tag)),
            _ => {}
        }
    }
    best.map(|(_, t)| t.clone())
}

/// Rewrite the `tag` value for `[deps.<name>]` in `doc`, preserving formatting.
fn set_dep_tag(doc: &mut toml_edit::DocumentMut, name: &str, new_tag: &str) -> anyhow::Result<()> {
    let deps = doc
        .get_mut("deps")
        .and_then(|i| i.as_table_like_mut())
        .ok_or_else(|| anyhow::anyhow!("[deps] table missing"))?;
    let entry = deps
        .get_mut(name)
        .ok_or_else(|| anyhow::anyhow!("dep '{name}' missing in [deps]"))?;
    if let Some(table) = entry.as_table_like_mut() {
        table.insert("tag", toml_edit::value(new_tag));
        Ok(())
    } else {
        Err(anyhow::anyhow!("dep '{name}' is not a table-like entry"))
    }
}

/// Per-dep update outcome.
#[derive(Debug)]
enum UpdateOutcome {
    /// Tag rewritten from `old` to `new` (manifest is mutated).
    TagBumped { old: String, new: String },
    /// Branch / no-pin dep: just re-install so lock picks up new HEAD.
    Refresh,
    /// Skipped (rev pin, exact tag without --force, unparseable tag, no remote match).
    Skipped(String),
}

fn run_update(
    name: Option<String>,
    manifest_path: &Path,
    cache_dir: &Path,
    vendored_dir: &Path,
    lock_path: &Path,
    dry_run: bool,
    force: bool,
) -> anyhow::Result<()> {
    let manifest = Manifest::from_path(manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;

    if let Some(ref n) = name {
        if !manifest.deps.contains_key(n) {
            return Err(anyhow::anyhow!(
                "unknown package '{}' in {}",
                n,
                manifest_path.display()
            ));
        }
    }

    let fetcher = GitFetcher::new(cache_dir.to_path_buf());
    let raw = std::fs::read_to_string(manifest_path)?;
    let mut doc: toml_edit::DocumentMut = raw.parse()?;

    let mut any_refresh = false;
    let mut summary: Vec<String> = Vec::new();

    for (dep_name, dep) in &manifest.deps {
        if let Some(ref n) = name {
            if dep_name != n {
                continue;
            }
        }

        let outcome = update_dep(dep_name, dep, &fetcher, force)?;
        match &outcome {
            UpdateOutcome::TagBumped { old, new } => {
                summary.push(format!("{dep_name}: tag {old} → {new}"));
                if !dry_run {
                    set_dep_tag(&mut doc, dep_name, new)?;
                }
                any_refresh = true;
            }
            UpdateOutcome::Refresh => {
                summary.push(format!("{dep_name}: refresh (branch / unpinned)"));
                any_refresh = true;
            }
            UpdateOutcome::Skipped(reason) => {
                summary.push(format!("{dep_name}: skip ({reason})"));
            }
        }
    }

    if summary.is_empty() {
        println!("no packages selected");
        return Ok(());
    }

    for line in &summary {
        println!("{line}");
    }

    if dry_run {
        println!("(dry-run; manifest not modified)");
        return Ok(());
    }

    std::fs::write(manifest_path, doc.to_string())?;

    if any_refresh {
        run_install(manifest_path, cache_dir, vendored_dir, lock_path)?;
    }
    Ok(())
}

fn update_dep(
    name: &str,
    dep: &Dep,
    fetcher: &GitFetcher,
    force: bool,
) -> anyhow::Result<UpdateOutcome> {
    if dep.rev.is_some() {
        return Ok(UpdateOutcome::Skipped("rev pin".into()));
    }
    if dep.branch.is_some() {
        return Ok(UpdateOutcome::Refresh);
    }
    let Some(current_tag) = &dep.tag else {
        return Ok(UpdateOutcome::Refresh);
    };

    let pin = match classify_tag_pin(current_tag) {
        Some(p) => p,
        None => {
            return Ok(UpdateOutcome::Skipped(format!(
                "tag '{current_tag}' is not SemVer"
            )))
        }
    };

    let tags = fetcher
        .list_tags(&dep.git)
        .with_context(|| format!("listing tags for '{name}'"))?;

    let new_tag = match (&pin, force) {
        (TagPin::Exact, false) => {
            return Ok(UpdateOutcome::Skipped(
                "exact tag pin (pass --force to bump)".into(),
            ))
        }
        (TagPin::Exact, true) => pick_latest_overall(&tags),
        (TagPin::Prefix(p), _) => pick_latest_for_pin(&tags, p),
    };

    let Some(new_tag) = new_tag else {
        return Ok(UpdateOutcome::Skipped(
            "no matching SemVer release tag on remote".into(),
        ));
    };

    if &new_tag == current_tag {
        return Ok(UpdateOutcome::Skipped(format!("already at {new_tag}")));
    }

    Ok(UpdateOutcome::TagBumped {
        old: current_tag.clone(),
        new: new_tag,
    })
}

// ── clean ─────────────────────────────────────────────────────────────────────

fn run_clean(all: bool, cache_dir: &Path, lock_path: &Path) -> anyhow::Result<()> {
    if all {
        if cache_dir.exists() {
            std::fs::remove_dir_all(cache_dir)?;
            println!("removed all cached packages");
        } else {
            println!("nothing to clean");
        }
        return Ok(());
    }

    // Read lockfile — absent means nothing was ever installed.
    let lockfile = match Lockfile::read(lock_path) {
        Ok(lf) => lf,
        Err(PkgError::MissingLockfile { .. }) => {
            println!("no lockfile found; nothing to clean");
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

    let in_use: HashSet<String> = lockfile.pkg.iter().map(|p| p.sha.clone()).collect();

    let git_dir = cache_dir.join("git");
    if !git_dir.exists() {
        println!("nothing to clean");
        return Ok(());
    }

    let mut removed: usize = 0;
    remove_stale_sha_dirs(&git_dir, &in_use, &mut removed)?;
    println!(
        "removed {removed} stale cache entr{}",
        if removed == 1 { "y" } else { "ies" }
    );
    Ok(())
}

/// Recursively walk `dir` and delete subdirectories whose name is a 40-hex
/// SHA that is absent from `in_use`.
fn remove_stale_sha_dirs(
    dir: &Path,
    in_use: &HashSet<String>,
    removed: &mut usize,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if name.len() == 40 && name.chars().all(|c| c.is_ascii_hexdigit()) {
            if !in_use.contains(name.as_ref()) {
                std::fs::remove_dir_all(&path)?;
                *removed += 1;
            }
        } else {
            // Descend deeper (host / org / repo levels).
            remove_stale_sha_dirs(&path, in_use, removed)?;
        }
    }
    Ok(())
}

// ── symlink helpers ───────────────────────────────────────────────────────────

/// Create a directory symlink at `link` pointing to `target`.
#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

#[cfg(not(any(unix, windows)))]
fn create_symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "symlinks not supported on this platform",
    ))
}

/// Remove a symlink at `path`.
#[cfg(unix)]
fn remove_symlink(path: &Path) -> std::io::Result<()> {
    std::fs::remove_file(path)
}

#[cfg(windows)]
fn remove_symlink(path: &Path) -> std::io::Result<()> {
    // On Windows a directory symlink is removed with remove_dir.
    if path.is_dir() {
        std::fs::remove_dir(path)
    } else {
        std::fs::remove_file(path)
    }
}

#[cfg(not(any(unix, windows)))]
fn remove_symlink(path: &Path) -> std::io::Result<()> {
    std::fs::remove_file(path)
}

/// Compute the path to `to` relative to `from_dir`.
///
/// Canonicalises both arguments so the result is correct even when the
/// process working directory is a symlinked path (e.g. macOS `/Users` →
/// `/private/Users`).  Both paths must already exist on the filesystem.
fn relative_path(from_dir: &Path, to: &Path) -> std::io::Result<PathBuf> {
    let from_abs = std::fs::canonicalize(from_dir)?;
    let to_abs = std::fs::canonicalize(to)?;

    let from_parts: Vec<_> = from_abs.components().collect();
    let to_parts: Vec<_> = to_abs.components().collect();

    let common = from_parts
        .iter()
        .zip(to_parts.iter())
        .take_while(|(a, b)| a == b)
        .count();

    let mut rel = PathBuf::new();
    for _ in &from_parts[common..] {
        rel.push("..");
    }
    for c in &to_parts[common..] {
        rel.push(c);
    }
    Ok(rel)
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── helpers ───────────────────────────────────────────────────────────────

    /// Initialise a bare git repository at `dir`, write a single `main.lua`
    /// file, commit it, and return the 40-char commit SHA.
    fn init_repo_with_commit(dir: &Path) -> String {
        use git2::{Repository, Signature};

        let repo = Repository::init(dir).unwrap();
        {
            let mut cfg = repo.config().unwrap();
            cfg.set_str("user.name", "Test").unwrap();
            cfg.set_str("user.email", "test@example.com").unwrap();
        }

        std::fs::write(dir.join("main.lua"), "return {}\n").unwrap();

        let mut index = repo.index().unwrap();
        index.add_path(Path::new("main.lua")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = Signature::now("Test", "test@example.com").unwrap();
        let oid = repo
            .commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        oid.to_string()
    }

    /// Write `content` to `path` (creating parent dirs as needed).
    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    // ── install ───────────────────────────────────────────────────────────────

    #[test]
    fn install_creates_lockfile_and_symlink() {
        let remote = TempDir::new().unwrap();
        let sha = init_repo_with_commit(remote.path());

        let project = TempDir::new().unwrap();
        let manifest_path = project.path().join("mlua-pkg.toml");
        let cache_dir = project.path().join(".mlua-pkgs/cache");
        let vendored_dir = project.path().join(".mlua-pkgs/vendored");
        let lock_path = project.path().join("mlua-pkg.lock");

        let url = format!("file://{}", remote.path().display());
        write_file(
            &manifest_path,
            &format!(
                "[package]\nname = \"test\"\nversion = \"0.1.0\"\n\n\
                 [deps]\nmylib = {{ git = \"{url}\", rev = \"{sha}\" }}\n"
            ),
        );

        run_install(&manifest_path, &cache_dir, &vendored_dir, &lock_path).unwrap();

        // Lockfile exists and has one entry.
        assert!(lock_path.exists(), "lockfile must be written");
        let lf = Lockfile::read(&lock_path).unwrap();
        assert_eq!(lf.pkg.len(), 1, "one locked package");
        assert_eq!(lf.pkg[0].name, "mylib");
        assert_eq!(lf.pkg[0].sha, sha);
        assert_eq!(lf.pkg[0].source, format!("git+{url}"));

        // Vendored symlink exists.
        let symlink = vendored_dir.join("mylib");
        assert!(
            symlink.symlink_metadata().is_ok(),
            "symlink .mlua-pkgs/vendored/mylib must exist"
        );
        // Symlink target must be relative (not absolute).
        let target = std::fs::read_link(&symlink).unwrap();
        assert!(
            target.is_relative(),
            "symlink target must be a relative path, got: {}",
            target.display()
        );
    }

    #[test]
    fn install_with_target_dir_physically_copies() {
        let remote = TempDir::new().unwrap();
        let sha = init_repo_with_commit(remote.path());

        let project = TempDir::new().unwrap();
        let manifest_path = project.path().join("mlua-pkg.toml");
        let cache_dir = project.path().join(".mlua-pkgs/cache");
        let vendored_dir = project.path().join(".mlua-pkgs/vendored");
        let lock_path = project.path().join("mlua-pkg.lock");

        let url = format!("file://{}", remote.path().display());
        write_file(
            &manifest_path,
            &format!(
                "[package]\nname = \"test\"\nversion = \"0.1.0\"\n\n\
                 [deps]\nmylib = {{ git = \"{url}\", rev = \"{sha}\", \
                 target_dir = \"lua/mylib\" }}\n"
            ),
        );

        run_install(&manifest_path, &cache_dir, &vendored_dir, &lock_path).unwrap();

        // target_dir holds a real file (not a symlink).
        let vendored_file = project.path().join("lua/mylib/main.lua");
        assert!(
            vendored_file.exists(),
            "vendored file must exist at target_dir"
        );
        let meta = std::fs::symlink_metadata(&vendored_file).unwrap();
        assert!(
            !meta.file_type().is_symlink(),
            "vendored output must be a regular file, not a symlink"
        );

        // Default symlink path must NOT be created when target_dir is set.
        assert!(
            vendored_dir.join("mylib").symlink_metadata().is_err(),
            ".mlua-pkgs/vendored/<name> must not be created when target_dir is set"
        );

        // Lockfile entry still recorded.
        let lf = Lockfile::read(&lock_path).unwrap();
        assert_eq!(lf.pkg.len(), 1);
        assert_eq!(lf.pkg[0].sha, sha);
    }

    #[test]
    fn install_with_target_dir_is_idempotent() {
        let remote = TempDir::new().unwrap();
        let sha = init_repo_with_commit(remote.path());

        let project = TempDir::new().unwrap();
        let manifest_path = project.path().join("mlua-pkg.toml");
        let cache_dir = project.path().join(".mlua-pkgs/cache");
        let vendored_dir = project.path().join(".mlua-pkgs/vendored");
        let lock_path = project.path().join("mlua-pkg.lock");

        let url = format!("file://{}", remote.path().display());
        write_file(
            &manifest_path,
            &format!(
                "[package]\nname = \"test\"\nversion = \"0.1.0\"\n\n\
                 [deps]\nmylib = {{ git = \"{url}\", rev = \"{sha}\", \
                 target_dir = \"lua/mylib\" }}\n"
            ),
        );

        run_install(&manifest_path, &cache_dir, &vendored_dir, &lock_path).unwrap();
        run_install(&manifest_path, &cache_dir, &vendored_dir, &lock_path).unwrap();

        assert!(project.path().join("lua/mylib/main.lua").exists());
    }

    #[test]
    fn install_missing_manifest_returns_error() {
        let project = TempDir::new().unwrap();
        let result = run_install(
            &project.path().join("mlua-pkg.toml"),
            &project.path().join(".mlua-pkgs/cache"),
            &project.path().join(".mlua-pkgs/vendored"),
            &project.path().join("mlua-pkg.lock"),
        );
        assert!(result.is_err(), "must fail when mlua-pkg.toml is absent");
    }

    #[test]
    fn install_is_idempotent() {
        // Running install twice must succeed (symlink replaced, lockfile overwritten).
        let remote = TempDir::new().unwrap();
        let sha = init_repo_with_commit(remote.path());

        let project = TempDir::new().unwrap();
        let manifest_path = project.path().join("mlua-pkg.toml");
        let cache_dir = project.path().join(".mlua-pkgs/cache");
        let vendored_dir = project.path().join(".mlua-pkgs/vendored");
        let lock_path = project.path().join("mlua-pkg.lock");

        let url = format!("file://{}", remote.path().display());
        write_file(
            &manifest_path,
            &format!(
                "[package]\nname = \"test\"\nversion = \"0.1.0\"\n\n\
                 [deps]\nlib = {{ git = \"{url}\", rev = \"{sha}\" }}\n"
            ),
        );

        run_install(&manifest_path, &cache_dir, &vendored_dir, &lock_path).unwrap();
        run_install(&manifest_path, &cache_dir, &vendored_dir, &lock_path).unwrap();

        let lf = Lockfile::read(&lock_path).unwrap();
        assert_eq!(lf.pkg.len(), 1);
    }

    // ── add ───────────────────────────────────────────────────────────────────

    #[test]
    fn add_creates_manifest_with_dep() {
        let project = TempDir::new().unwrap();
        let manifest_path = project.path().join("mlua-pkg.toml");

        run_add(
            &manifest_path,
            "mylib".to_string(),
            "https://github.com/x/mylib".to_string(),
            Some("v1.0.0".to_string()),
            None,
            None,
            None,
            None,
        )
        .unwrap();

        let manifest = Manifest::from_path(&manifest_path).unwrap();
        assert!(manifest.deps.contains_key("mylib"), "dep must be present");
        let dep = &manifest.deps["mylib"];
        assert_eq!(dep.git, "https://github.com/x/mylib");
        assert_eq!(dep.tag.as_deref(), Some("v1.0.0"));
        assert!(dep.rev.is_none());
        assert!(dep.branch.is_none());
    }

    #[test]
    fn add_to_existing_manifest_preserves_other_deps() {
        let project = TempDir::new().unwrap();
        let manifest_path = project.path().join("mlua-pkg.toml");

        write_file(
            &manifest_path,
            "[package]\nname = \"test\"\nversion = \"0.1.0\"\n\n\
             [deps]\nexisting = { git = \"https://github.com/a/b\", branch = \"main\" }\n",
        );

        run_add(
            &manifest_path,
            "newdep".to_string(),
            "https://github.com/x/newdep".to_string(),
            None,
            Some("abc1234567890123456789012345678901234567890".to_string()),
            None,
            None,
            None,
        )
        .unwrap();

        let manifest = Manifest::from_path(&manifest_path).unwrap();
        assert_eq!(manifest.deps.len(), 2, "both deps must be present");
        assert!(manifest.deps.contains_key("existing"));
        assert!(manifest.deps.contains_key("newdep"));
    }

    #[test]
    fn add_rejects_multiple_ref_fields() {
        let project = TempDir::new().unwrap();
        let manifest_path = project.path().join("mlua-pkg.toml");

        let result = run_add(
            &manifest_path,
            "lib".to_string(),
            "https://github.com/x/lib".to_string(),
            Some("v1.0.0".to_string()),
            Some("abc123".to_string()),
            None,
            None,
            None,
        );
        assert!(result.is_err(), "tag + rev together must be rejected");
    }

    // ── update ────────────────────────────────────────────────────────────────

    #[test]
    fn classify_tag_pin_full_semver_is_exact() {
        assert_eq!(classify_tag_pin("v1.2.3"), Some(TagPin::Exact));
        assert_eq!(classify_tag_pin("1.2.3"), Some(TagPin::Exact));
        assert_eq!(classify_tag_pin("v0.1.0"), Some(TagPin::Exact));
        assert_eq!(classify_tag_pin("1.0.0-rc1"), Some(TagPin::Exact));
    }

    #[test]
    fn classify_tag_pin_partial_is_prefix() {
        assert_eq!(classify_tag_pin("v1.0"), Some(TagPin::Prefix(vec![1, 0])));
        assert_eq!(classify_tag_pin("v1"), Some(TagPin::Prefix(vec![1])));
        assert_eq!(classify_tag_pin("2.5"), Some(TagPin::Prefix(vec![2, 5])));
    }

    #[test]
    fn classify_tag_pin_non_semver_is_none() {
        assert_eq!(classify_tag_pin("latest"), None);
        assert_eq!(classify_tag_pin("release-candidate"), None);
        assert_eq!(classify_tag_pin(""), None);
    }

    #[test]
    fn pick_latest_for_pin_matches_prefix_max() {
        let tags = vec![
            "v1.0.0".to_string(),
            "v1.0.1".to_string(),
            "v1.0.5".to_string(),
            "v1.1.0".to_string(),
            "v2.0.0".to_string(),
        ];
        assert_eq!(
            pick_latest_for_pin(&tags, &[1, 0]),
            Some("v1.0.5".to_string())
        );
        assert_eq!(pick_latest_for_pin(&tags, &[1]), Some("v1.1.0".to_string()));
        assert_eq!(pick_latest_for_pin(&tags, &[3]), None);
    }

    #[test]
    fn pick_latest_for_pin_excludes_prerelease() {
        let tags = vec![
            "v1.0.0".to_string(),
            "v1.0.1-rc1".to_string(),
            "v1.0.1".to_string(),
        ];
        assert_eq!(
            pick_latest_for_pin(&tags, &[1, 0]),
            Some("v1.0.1".to_string())
        );
    }

    #[test]
    fn pick_latest_for_pin_ignores_unrelated_tags() {
        let tags = vec![
            "v1.0.0".to_string(),
            "release-2024".to_string(),
            "v1.0.1".to_string(),
        ];
        assert_eq!(
            pick_latest_for_pin(&tags, &[1, 0]),
            Some("v1.0.1".to_string())
        );
    }

    #[test]
    fn pick_latest_overall_takes_global_max() {
        let tags = vec![
            "v0.9.9".to_string(),
            "v1.0.5".to_string(),
            "v2.0.0".to_string(),
            "v1.9.9".to_string(),
        ];
        assert_eq!(pick_latest_overall(&tags), Some("v2.0.0".to_string()));
    }

    #[test]
    fn set_dep_tag_preserves_inline_layout() {
        let toml = "[package]\nname = \"x\"\nversion = \"0.1.0\"\n\n\
                    [deps]\nfoo = { git = \"https://example.com/foo\", tag = \"v1.0.0\" }\n";
        let mut doc: toml_edit::DocumentMut = toml.parse().unwrap();
        set_dep_tag(&mut doc, "foo", "v1.0.5").unwrap();
        let out = doc.to_string();
        assert!(
            out.contains("tag = \"v1.0.5\""),
            "new tag must be written:\n{out}"
        );
        assert!(
            out.contains("git = \"https://example.com/foo\""),
            "git URL preserved"
        );
    }

    /// Add an annotated tag to HEAD of the repo at `dir`.
    fn add_tag(dir: &Path, tag: &str) {
        use git2::{Repository, Signature};
        let repo = Repository::open(dir).unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        let sig = Signature::now("Test", "test@example.com").unwrap();
        repo.tag(tag, head.as_object(), &sig, tag, false).unwrap();
    }

    #[test]
    fn update_prefix_pin_bumps_to_latest_patch() {
        let remote = TempDir::new().unwrap();
        init_repo_with_commit(remote.path());
        add_tag(remote.path(), "v1.0.0");
        add_tag(remote.path(), "v1.0.1");
        add_tag(remote.path(), "v1.0.5");
        add_tag(remote.path(), "v1.1.0");

        let project = TempDir::new().unwrap();
        let manifest_path = project.path().join("mlua-pkg.toml");
        let cache_dir = project.path().join(".mlua-pkgs/cache");
        let vendored_dir = project.path().join(".mlua-pkgs/vendored");
        let lock_path = project.path().join("mlua-pkg.lock");

        let url = format!("file://{}", remote.path().display());
        write_file(
            &manifest_path,
            &format!(
                "[package]\nname = \"test\"\nversion = \"0.1.0\"\n\n\
                 [deps]\nmylib = {{ git = \"{url}\", tag = \"v1.0\" }}\n"
            ),
        );

        run_update(
            None,
            &manifest_path,
            &cache_dir,
            &vendored_dir,
            &lock_path,
            false,
            false,
        )
        .unwrap();

        let updated = std::fs::read_to_string(&manifest_path).unwrap();
        assert!(
            updated.contains("tag = \"v1.0.5\""),
            "prefix pin v1.0 must bump to v1.0.5 (skipping v1.1.0):\n{updated}"
        );
    }

    #[test]
    fn update_dry_run_leaves_manifest_unmodified() {
        let remote = TempDir::new().unwrap();
        init_repo_with_commit(remote.path());
        add_tag(remote.path(), "v1.0.0");
        add_tag(remote.path(), "v1.0.5");

        let project = TempDir::new().unwrap();
        let manifest_path = project.path().join("mlua-pkg.toml");
        let url = format!("file://{}", remote.path().display());
        let original = format!(
            "[package]\nname = \"test\"\nversion = \"0.1.0\"\n\n\
             [deps]\nmylib = {{ git = \"{url}\", tag = \"v1.0\" }}\n"
        );
        write_file(&manifest_path, &original);

        run_update(
            None,
            &manifest_path,
            &project.path().join(".mlua-pkgs/cache"),
            &project.path().join(".mlua-pkgs/vendored"),
            &project.path().join("mlua-pkg.lock"),
            true,
            false,
        )
        .unwrap();

        let after = std::fs::read_to_string(&manifest_path).unwrap();
        assert_eq!(after, original, "dry-run must not modify manifest");
    }

    #[test]
    fn update_exact_pin_without_force_is_noop() {
        let remote = TempDir::new().unwrap();
        init_repo_with_commit(remote.path());
        add_tag(remote.path(), "v1.0.0");
        add_tag(remote.path(), "v1.0.5");

        let project = TempDir::new().unwrap();
        let manifest_path = project.path().join("mlua-pkg.toml");
        let url = format!("file://{}", remote.path().display());
        let original = format!(
            "[package]\nname = \"test\"\nversion = \"0.1.0\"\n\n\
             [deps]\nmylib = {{ git = \"{url}\", tag = \"v1.0.0\" }}\n"
        );
        write_file(&manifest_path, &original);

        run_update(
            None,
            &manifest_path,
            &project.path().join(".mlua-pkgs/cache"),
            &project.path().join(".mlua-pkgs/vendored"),
            &project.path().join("mlua-pkg.lock"),
            false,
            false,
        )
        .unwrap();

        let after = std::fs::read_to_string(&manifest_path).unwrap();
        assert!(
            after.contains("tag = \"v1.0.0\""),
            "exact pin must remain v1.0.0 without --force:\n{after}"
        );
    }

    #[test]
    fn update_exact_pin_with_force_bumps_to_latest() {
        let remote = TempDir::new().unwrap();
        init_repo_with_commit(remote.path());
        add_tag(remote.path(), "v1.0.0");
        add_tag(remote.path(), "v1.0.5");
        add_tag(remote.path(), "v2.0.0");

        let project = TempDir::new().unwrap();
        let manifest_path = project.path().join("mlua-pkg.toml");
        let url = format!("file://{}", remote.path().display());
        write_file(
            &manifest_path,
            &format!(
                "[package]\nname = \"test\"\nversion = \"0.1.0\"\n\n\
                 [deps]\nmylib = {{ git = \"{url}\", tag = \"v1.0.0\" }}\n"
            ),
        );

        run_update(
            None,
            &manifest_path,
            &project.path().join(".mlua-pkgs/cache"),
            &project.path().join(".mlua-pkgs/vendored"),
            &project.path().join("mlua-pkg.lock"),
            false,
            true,
        )
        .unwrap();

        let after = std::fs::read_to_string(&manifest_path).unwrap();
        assert!(
            after.contains("tag = \"v2.0.0\""),
            "--force must bump exact pin to global max v2.0.0:\n{after}"
        );
    }

    #[test]
    fn update_unknown_name_returns_error() {
        let project = TempDir::new().unwrap();
        let manifest_path = project.path().join("mlua-pkg.toml");

        write_file(
            &manifest_path,
            "[package]\nname = \"test\"\nversion = \"0.1.0\"\n",
        );

        let result = run_update(
            Some("nonexistent".to_string()),
            &manifest_path,
            &project.path().join("cache"),
            &project.path().join("vendored"),
            &project.path().join("mlua-pkg.lock"),
            false,
            false,
        );
        assert!(result.is_err(), "unknown dep name must return error");
    }

    // ── clean ─────────────────────────────────────────────────────────────────

    #[test]
    fn clean_all_removes_cache() {
        let project = TempDir::new().unwrap();
        let cache_dir = project.path().join(".mlua-pkgs/cache");
        let git_dir = cache_dir.join("git/example.com/org/repo");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("sentinel"), "data").unwrap();

        run_clean(true, &cache_dir, &project.path().join("mlua-pkg.lock")).unwrap();

        assert!(!cache_dir.exists(), "cache directory must be removed");
    }

    #[test]
    fn clean_all_on_empty_dir_is_noop() {
        let project = TempDir::new().unwrap();
        let cache_dir = project.path().join(".mlua-pkgs/cache");

        // Does not exist — must succeed without error.
        run_clean(true, &cache_dir, &project.path().join("mlua-pkg.lock")).unwrap();
    }

    #[test]
    fn clean_without_lockfile_is_noop() {
        let project = TempDir::new().unwrap();

        run_clean(
            false,
            &project.path().join(".mlua-pkgs/cache"),
            &project.path().join("mlua-pkg.lock"),
        )
        .unwrap();
    }

    #[test]
    fn clean_removes_stale_sha_dirs_only() {
        let project = TempDir::new().unwrap();
        let cache_dir = project.path().join(".mlua-pkgs/cache");
        let git_base = cache_dir.join("git/gh.com/org/repo");

        let sha_in_use = "a".repeat(40);
        let sha_stale = "b".repeat(40);

        std::fs::create_dir_all(git_base.join(&sha_in_use)).unwrap();
        std::fs::create_dir_all(git_base.join(&sha_stale)).unwrap();

        // Write a lockfile that references only sha_in_use.
        let lock_path = project.path().join("mlua-pkg.lock");
        let lf = Lockfile {
            version: 1,
            pkg: vec![LockedPkg {
                name: "lib".to_string(),
                source: "git+https://gh.com/org/repo".to_string(),
                tag: None,
                rev: None,
                branch: None,
                sha: sha_in_use.clone(),
                entry: PathBuf::from("."),
            }],
        };
        lf.write(&lock_path).unwrap();

        run_clean(false, &cache_dir, &lock_path).unwrap();

        assert!(
            git_base.join(&sha_in_use).exists(),
            "in-use SHA dir must be retained"
        );
        assert!(
            !git_base.join(&sha_stale).exists(),
            "stale SHA dir must be removed"
        );
    }

    // ── relative_path ─────────────────────────────────────────────────────────

    #[test]
    fn relative_path_sibling_dirs() {
        let tmp = TempDir::new().unwrap();
        let from_dir = tmp.path().join("a/b");
        let to_dir = tmp.path().join("a/c/d");
        std::fs::create_dir_all(&from_dir).unwrap();
        std::fs::create_dir_all(&to_dir).unwrap();

        let rel = relative_path(&from_dir, &to_dir).unwrap();
        // Expect: "../c/d"
        assert_eq!(rel, PathBuf::from("../c/d"));
    }

    #[test]
    fn relative_path_vendored_to_cache() {
        let tmp = TempDir::new().unwrap();
        let vendored = tmp.path().join(".mlua-pkgs/vendored");
        let entry = tmp
            .path()
            .join(".mlua-pkgs/cache/git/gh.com/org/repo/aaaa1234/src");
        std::fs::create_dir_all(&vendored).unwrap();
        std::fs::create_dir_all(&entry).unwrap();

        let rel = relative_path(&vendored, &entry).unwrap();
        assert!(
            rel.starts_with(".."),
            "must navigate up from vendored first"
        );
        assert!(
            rel.to_string_lossy().contains("cache"),
            "must contain 'cache' segment"
        );
    }

    // ── entry_rel_to_pkg ──────────────────────────────────────────────────────

    #[test]
    fn entry_rel_to_pkg_subdir() {
        let cache = PathBuf::from("/tmp/repo");
        let entry = PathBuf::from("/tmp/repo/src");
        assert_eq!(entry_rel_to_pkg(&cache, &entry), PathBuf::from("src"));
    }

    #[test]
    fn entry_rel_to_pkg_root() {
        let cache = PathBuf::from("/tmp/repo");
        let entry = PathBuf::from("/tmp/repo");
        assert_eq!(entry_rel_to_pkg(&cache, &entry), PathBuf::from("."));
    }

    // ── CLI parse smoke ───────────────────────────────────────────────────────

    #[test]
    fn cli_debug_assert() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    // ── cargo subcommand arg strip ────────────────────────────────────────────

    #[test]
    fn strip_cargo_subcommand_drops_redundant_arg() {
        let input = vec![
            "cargo-mlua-pkg".to_string(),
            "mlua-pkg".to_string(),
            "install".to_string(),
        ];
        let out = strip_cargo_subcommand(input);
        assert_eq!(
            out,
            vec!["cargo-mlua-pkg".to_string(), "install".to_string()]
        );
    }

    #[test]
    fn strip_cargo_subcommand_leaves_standalone_invocation_alone() {
        let input = vec!["mlua-pkg".to_string(), "install".to_string()];
        let out = strip_cargo_subcommand(input);
        assert_eq!(out, vec!["mlua-pkg".to_string(), "install".to_string()]);
    }

    #[test]
    fn strip_cargo_subcommand_handles_empty() {
        let out = strip_cargo_subcommand(Vec::<String>::new());
        assert!(out.is_empty());
    }
}
