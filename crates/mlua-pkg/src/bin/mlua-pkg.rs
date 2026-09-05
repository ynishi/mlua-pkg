//! `mlua-pkg` CLI — install / add / update / clean.
//!
//! Each subcommand delegates to a free function (`run_*`) that accepts
//! explicit path arguments, which makes them unit-testable without touching
//! the process working directory.

use std::{
    collections::{HashMap, HashSet, VecDeque},
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
    /// Base directory for cache + vendored output.
    ///
    /// Resolution order: this flag > `MLUA_PKG_DIR` env > auto-detect.
    /// Auto-detect picks `target/mlua-pkgs` when a `target/` directory
    /// exists in the current working directory (so Rust crates avoid
    /// littering the workspace and `cargo publish`'s VCS walker skips it),
    /// otherwise falls back to `.mlua-pkgs`.
    #[arg(long, global = true, value_name = "PATH")]
    mlua_pkgs_dir: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

/// Resolve the base `.mlua-pkgs` directory.
///
/// Priority: explicit `--mlua-pkgs-dir` flag > `MLUA_PKG_DIR` env >
/// `target/mlua-pkgs` when `target/` exists > `.mlua-pkgs` fallback.
fn resolve_mlua_pkgs_dir(flag: Option<&Path>) -> PathBuf {
    if let Some(p) = flag {
        return p.to_path_buf();
    }
    if let Ok(p) = std::env::var("MLUA_PKG_DIR") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    if Path::new("target").is_dir() {
        PathBuf::from("target/mlua-pkgs")
    } else {
        PathBuf::from(".mlua-pkgs")
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Fetch every dep in mlua-pkg.toml and write mlua-pkg.lock.
    ///
    /// Resolution per [deps.<name>]:
    ///   tag = "v1.0.0"  exact pin, fetched literally
    ///   tag = "v1.0"    prefix pin, resolves to the SemVer-max
    ///                   matching release (excludes pre-releases)
    ///   branch = "..."  remote branch HEAD
    ///   rev = "..."     specific commit SHA
    ///
    /// Output per dep:
    ///   default                 → relative symlink at
    ///                              .mlua-pkgs/vendored/<name>
    ///   target_dir = "<path>"   → physical copy of the entry into
    ///                              <manifest_dir>/<path>/ (idempotent;
    ///                              versionable in your git tree)
    ///
    /// The lockfile records the *resolved* concrete tag (e.g. v1.0.5 even
    /// when the manifest says v1.0) so it is always clear which release is
    /// installed.
    #[command(verbatim_doc_comment)]
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

    /// Refresh deps and bump tag pins as appropriate, then re-install.
    ///
    /// Per-pin behaviour:
    ///   tag = "v1.0.0"  exact pin → skip (use --force to bump to the
    ///                                SemVer-max release on the remote)
    ///   tag = "v1.0"    prefix pin → refresh; resolves to the latest
    ///                                v1.0.x and updates the lock. The
    ///                                manifest is *not* rewritten, so the
    ///                                prefix keeps auto-following future
    ///                                patches.
    ///   branch = "..."  refresh; re-install picks up new HEAD
    ///   rev = "..."     skip
    ///
    /// With --dry-run the plan is printed but neither the manifest nor the
    /// lockfile is modified.
    #[command(verbatim_doc_comment)]
    Update {
        /// Package name to update.  When omitted, all deps are considered.
        name: Option<String>,
        /// Show planned changes without writing the manifest or running install.
        #[arg(long)]
        dry_run: bool,
        /// Also bump exact (full SemVer) tag pins to the SemVer-max release
        /// available on the remote.
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
    let base = resolve_mlua_pkgs_dir(cli.mlua_pkgs_dir.as_deref());
    let cache_dir = base.join("cache");
    let vendored_dir = base.join("vendored");
    match cli.cmd {
        Cmd::Install => run_install(
            Path::new("mlua-pkg.toml"),
            &cache_dir,
            &vendored_dir,
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
            &cache_dir,
            &vendored_dir,
            Path::new("mlua-pkg.lock"),
            dry_run,
            force,
        ),
        Cmd::Clean { all } => run_clean(all, &cache_dir, Path::new("mlua-pkg.lock")),
    }
}

// ── install ───────────────────────────────────────────────────────────────────

/// Core install logic — testable with explicit paths.
///
/// Reads `manifest_path`, fetches each `[deps.<name>]` entry via
/// [`GitFetcher`] under `cache_dir`, places the dep into either
/// `vendored_dir/<name>` (relative symlink, default) or
/// `<manifest_dir>/<dep.target_dir>` (idempotent physical copy), and
/// writes the resolved `(tag, sha)` pairs into `lock_path`.
///
/// Same-name conflicts within `[deps]` are rejected up front (defence in
/// depth for future transitive resolution); prefix tag pins are resolved
/// inside the fetcher, so this function does not see a difference between
/// `tag = "v1.0"` and `tag = "v1.0.5"`.
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

    // Worklist over (name, dep, requested_by). Direct deps seed it; every fetched
    // package that ships its own `mlua-pkg.toml` appends its `[deps]` (transitive
    // resolution, breadth-first). A name reached twice must carry an identical
    // `Dep` (same git URL, pin, entry, target_dir) or the install fails with
    // `PkgError::DepConflict` — there is no version unification.
    let mut resolved: HashMap<String, (Dep, String)> = HashMap::new();
    let mut queue: VecDeque<(String, Dep, String)> = VecDeque::new();
    let mut direct: Vec<(&String, &Dep)> = manifest.deps.iter().collect();
    direct.sort_by(|a, b| a.0.cmp(b.0));
    for (name, dep) in direct {
        queue.push_back((name.clone(), dep.clone(), "<manifest>".to_string()));
    }

    while let Some((name, dep, requested_by)) = queue.pop_front() {
        if let Some((prev, prev_by)) = resolved.get(&name) {
            if *prev == dep {
                continue; // same package reached via another path
            }
            return Err(PkgError::DepConflict {
                name: name.clone(),
                first: prev_by.clone(),
                second: requested_by,
            }
            .into());
        }
        resolved.insert(name.clone(), (dep.clone(), requested_by.clone()));
        let name = &name;
        let dep = &dep;

        let fetched = fetcher
            .fetch(dep)
            .with_context(|| format!("fetching '{name}' (requested by {requested_by})"))?;

        // Transitive deps: enqueue the author's own `[deps]`, resolved later in order.
        if let Some(author) = &fetched.manifest {
            let mut sub: Vec<(&String, &Dep)> = author.deps.iter().collect();
            sub.sort_by(|a, b| a.0.cmp(b.0));
            for (sub_name, sub_dep) in sub {
                queue.push_back((sub_name.clone(), sub_dep.clone(), name.clone()));
            }
        }

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

        // Record the *resolved* tag in the lockfile (concrete release for
        // prefix pins; falls back to whatever the manifest declared).
        let locked_tag = fetched.resolved_tag.clone().or_else(|| dep.tag.clone());
        locked_pkgs.push(LockedPkg {
            name: name.clone(),
            source: format!("git+{}", dep.git),
            tag: locked_tag,
            rev: dep.rev.clone(),
            branch: dep.branch.clone(),
            sha: fetched.sha,
            entry,
        });
    }

    // Deterministic lockfile order regardless of HashMap iteration / discovery order.
    locked_pkgs.sort_by(|a, b| a.name.cmp(&b.name));
    let n = locked_pkgs.len();
    let lockfile = Lockfile {
        version: 1,
        pkg: locked_pkgs,
    };
    lockfile.write(lock_path)?;

    let transitive = n.saturating_sub(manifest.deps.len());
    if transitive > 0 {
        println!("installed {n} package(s) ({transitive} transitive)");
    } else {
        println!("installed {n} package(s)");
    }
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

/// Recursively copy every entry under `src` into `dest`, materialising
/// symlinks into regular files / directories so the vendored output is
/// self-contained and never depends on the cache layout.
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

/// Core `add` logic — testable with explicit paths.
///
/// Inserts (or replaces) a `[deps.<name>]` entry in the manifest at
/// `manifest_path`.  Performs no network I/O and never invokes the
/// fetcher; callers run `mlua-pkg install` afterwards.  Synthesises a
/// minimal `[package]` block if the manifest file does not yet exist.
/// Mutual exclusivity of `tag` / `rev` / `branch` is enforced up front.
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

use mlua_pkg::version::{classify_tag_pin, pick_latest_for_pin, pick_latest_overall, TagPin};

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
    /// Exact tag pin rewritten from `old` to `new` (manifest is mutated;
    /// only emitted under `--force`).
    TagBumped { old: String, new: String },
    /// Prefix tag pin re-resolves to a concrete release.  The manifest is
    /// **not** mutated — the prefix stays so it keeps auto-following future
    /// patches.  `resolved` is shown in dry-run output for transparency.
    PrefixResolved { pin: String, resolved: String },
    /// Branch / no-pin dep: just re-install so lock picks up new HEAD.
    Refresh,
    /// Skipped (rev pin, exact tag without --force, unparseable tag, no remote match).
    Skipped(String),
}

/// Core `update` logic — testable with explicit paths.
///
/// Walks `[deps]` (or just `name` when `Some`), classifies each entry via
/// [`update_dep`], prints a one-line plan per dep, and — unless
/// `dry_run` — applies the changes:
///
/// - `TagBumped` (exact pin under `--force`): rewrite the manifest's
///   `tag` value in place via [`set_dep_tag`], then re-install.
/// - `PrefixResolved` (prefix pin): **do not** mutate the manifest; just
///   re-install so the fetcher resolves the prefix afresh and the
///   lockfile picks up the new concrete tag / SHA.
/// - `Refresh` (branch / no pin): re-install only.
/// - `Skipped`: no-op.
///
/// Manifest edits go through `toml_edit::DocumentMut` so comments,
/// formatting, and key order survive a rewrite.  When the update touched
/// the manifest the file is replaced atomically before `run_install` is
/// invoked.
#[allow(clippy::too_many_arguments)]
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
            UpdateOutcome::PrefixResolved { pin, resolved } => {
                summary.push(format!(
                    "{dep_name}: refresh (prefix '{pin}' → {resolved}; manifest unchanged)"
                ));
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

/// Decide what `mlua-pkg update` should do for a single dep.
///
/// Pure policy: lists remote tags via the fetcher when needed but does
/// not write to disk.  Returns an [`UpdateOutcome`] for `run_update` to
/// act on.
///
/// Decision table:
///
/// | dep pin              | `force` | outcome                                     |
/// | -------------------- | ------- | ------------------------------------------- |
/// | `rev = "..."`        | any     | `Skipped("rev pin")`                        |
/// | `branch = "..."`     | any     | `Refresh`                                   |
/// | `tag = "..."`, exact | `false` | `Skipped("exact tag pin …")`                |
/// | `tag = "..."`, exact | `true`  | `TagBumped` (or `Skipped("already at …")`)  |
/// | `tag = "..."`, prefix| any     | `PrefixResolved` (or `Skipped("no match")`) |
/// | `tag = "..."`, junk  | any     | `Skipped("tag '…' is not SemVer")`          |
/// | no pin               | any     | `Refresh`                                   |
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

    // Exact pin without --force: nothing to do (without listing remote tags).
    if matches!(pin, TagPin::Exact) && !force {
        return Ok(UpdateOutcome::Skipped(
            "exact tag pin (pass --force to bump)".into(),
        ));
    }

    let tags = fetcher
        .list_tags(&dep.git)
        .with_context(|| format!("listing tags for '{name}'"))?;

    match pin {
        TagPin::Exact => {
            // --force path: bump to the SemVer-max release on the remote.
            let Some(new_tag) = pick_latest_overall(&tags) else {
                return Ok(UpdateOutcome::Skipped(
                    "no matching SemVer release tag on remote".into(),
                ));
            };
            if &new_tag == current_tag {
                Ok(UpdateOutcome::Skipped(format!("already at {new_tag}")))
            } else {
                Ok(UpdateOutcome::TagBumped {
                    old: current_tag.clone(),
                    new: new_tag,
                })
            }
        }
        TagPin::Prefix(p) => {
            // Prefix pin: resolve to a concrete tag but leave the manifest
            // untouched so the prefix keeps auto-following future patches.
            let Some(resolved) = pick_latest_for_pin(&tags, &p) else {
                return Ok(UpdateOutcome::Skipped(
                    "no matching SemVer release tag on remote".into(),
                ));
            };
            Ok(UpdateOutcome::PrefixResolved {
                pin: current_tag.clone(),
                resolved,
            })
        }
    }
}

// ── clean ─────────────────────────────────────────────────────────────────────

/// Core `clean` logic — testable with explicit paths.
///
/// With `all = true`, removes the entire `cache_dir` and returns.
/// Otherwise reads the lockfile at `lock_path`, collects the set of
/// in-use SHAs, and recursively deletes any 40-hex SHA directory under
/// `<cache_dir>/git/` that is *not* in that set.  A missing lockfile is
/// treated as a no-op rather than an error.
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

    /// Initialise a git repository at `dir` with the given files committed;
    /// return the commit SHA.
    fn init_repo_with_files(dir: &Path, files: &[(&str, &str)]) -> String {
        use git2::{Repository, Signature};

        let repo = Repository::init(dir).unwrap();
        {
            let mut cfg = repo.config().unwrap();
            cfg.set_str("user.name", "Test").unwrap();
            cfg.set_str("user.email", "test@example.com").unwrap();
        }
        let mut index = repo.index().unwrap();
        for (rel, content) in files {
            write_file(&dir.join(rel), content);
            index.add_path(Path::new(rel)).unwrap();
        }
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = Signature::now("Test", "test@example.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap()
            .to_string()
    }

    // ── transitive deps ───────────────────────────────────────────────────────

    #[test]
    fn install_resolves_transitive_deps_from_author_manifest() {
        // leaf: plain package
        let leaf = TempDir::new().unwrap();
        let leaf_sha = init_repo_with_files(leaf.path(), &[("main.lua", "return { leaf = true }\n")]);
        let leaf_url = format!("file://{}", leaf.path().display());

        // mid: depends on leaf via its own mlua-pkg.toml
        let mid = TempDir::new().unwrap();
        let mid_manifest = format!(
            "[package]\nname = \"mid\"\nversion = \"0.1.0\"\n\n\
             [deps]\nleaf = {{ git = \"{leaf_url}\", rev = \"{leaf_sha}\" }}\n"
        );
        let mid_sha = init_repo_with_files(
            mid.path(),
            &[("main.lua", "return require('leaf')\n"), ("mlua-pkg.toml", &mid_manifest)],
        );
        let mid_url = format!("file://{}", mid.path().display());

        // root: only declares mid
        let project = TempDir::new().unwrap();
        let manifest_path = project.path().join("mlua-pkg.toml");
        let cache_dir = project.path().join(".mlua-pkgs/cache");
        let vendored_dir = project.path().join(".mlua-pkgs/vendored");
        let lock_path = project.path().join("mlua-pkg.lock");
        write_file(
            &manifest_path,
            &format!(
                "[package]\nname = \"root\"\nversion = \"0.1.0\"\n\n\
                 [deps]\nmid = {{ git = \"{mid_url}\", rev = \"{mid_sha}\" }}\n"
            ),
        );

        run_install(&manifest_path, &cache_dir, &vendored_dir, &lock_path).unwrap();

        let lf = Lockfile::read(&lock_path).unwrap();
        let names: Vec<&str> = lf.pkg.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["leaf", "mid"], "both packages locked, sorted by name");
        assert_eq!(lf.pkg[0].sha, leaf_sha);
        assert_eq!(lf.pkg[1].sha, mid_sha);
        assert!(vendored_dir.join("leaf").symlink_metadata().is_ok(), "leaf vendored");
        assert!(vendored_dir.join("mid").symlink_metadata().is_ok(), "mid vendored");

        // Idempotent.
        run_install(&manifest_path, &cache_dir, &vendored_dir, &lock_path).unwrap();
        assert_eq!(Lockfile::read(&lock_path).unwrap().pkg.len(), 2);
    }

    #[test]
    fn install_same_dep_reached_twice_with_identical_spec_is_fine() {
        let leaf = TempDir::new().unwrap();
        let leaf_sha = init_repo_with_files(leaf.path(), &[("main.lua", "return {}\n")]);
        let leaf_url = format!("file://{}", leaf.path().display());

        let mid = TempDir::new().unwrap();
        let mid_manifest = format!(
            "[package]\nname = \"mid\"\nversion = \"0.1.0\"\n\n\
             [deps]\nleaf = {{ git = \"{leaf_url}\", rev = \"{leaf_sha}\" }}\n"
        );
        let mid_sha = init_repo_with_files(
            mid.path(),
            &[("main.lua", "return {}\n"), ("mlua-pkg.toml", &mid_manifest)],
        );
        let mid_url = format!("file://{}", mid.path().display());

        let project = TempDir::new().unwrap();
        let manifest_path = project.path().join("mlua-pkg.toml");
        let cache_dir = project.path().join(".mlua-pkgs/cache");
        let vendored_dir = project.path().join(".mlua-pkgs/vendored");
        let lock_path = project.path().join("mlua-pkg.lock");
        // root declares leaf itself with the *same* spec mid uses.
        write_file(
            &manifest_path,
            &format!(
                "[package]\nname = \"root\"\nversion = \"0.1.0\"\n\n\
                 [deps]\nmid = {{ git = \"{mid_url}\", rev = \"{mid_sha}\" }}\n\
                 leaf = {{ git = \"{leaf_url}\", rev = \"{leaf_sha}\" }}\n"
            ),
        );

        run_install(&manifest_path, &cache_dir, &vendored_dir, &lock_path).unwrap();
        assert_eq!(Lockfile::read(&lock_path).unwrap().pkg.len(), 2);
    }

    #[test]
    fn install_conflicting_transitive_spec_fails() {
        let leaf = TempDir::new().unwrap();
        let leaf_sha = init_repo_with_files(leaf.path(), &[("main.lua", "return {}\n")]);
        let leaf_url = format!("file://{}", leaf.path().display());

        // mid pins leaf by rev; root pins the same rev but with an `entry` override
        // -> a different `Dep` spec, which is a conflict (no unification).
        let mid = TempDir::new().unwrap();
        let mid_manifest = format!(
            "[package]\nname = \"mid\"\nversion = \"0.1.0\"\n\n\
             [deps]\nleaf = {{ git = \"{leaf_url}\", rev = \"{leaf_sha}\" }}\n"
        );
        let mid_sha = init_repo_with_files(
            mid.path(),
            &[("main.lua", "return {}\n"), ("mlua-pkg.toml", &mid_manifest)],
        );
        let mid_url = format!("file://{}", mid.path().display());

        let project = TempDir::new().unwrap();
        let manifest_path = project.path().join("mlua-pkg.toml");
        let cache_dir = project.path().join(".mlua-pkgs/cache");
        let vendored_dir = project.path().join(".mlua-pkgs/vendored");
        let lock_path = project.path().join("mlua-pkg.lock");
        write_file(
            &manifest_path,
            &format!(
                "[package]\nname = \"root\"\nversion = \"0.1.0\"\n\n\
                 [deps]\nleaf = {{ git = \"{leaf_url}\", rev = \"{leaf_sha}\", entry = \".\" }}\n\
                 mid = {{ git = \"{mid_url}\", rev = \"{mid_sha}\" }}\n"
            ),
        );

        let err = run_install(&manifest_path, &cache_dir, &vendored_dir, &lock_path).unwrap_err();
        let conflict = err
            .downcast_ref::<PkgError>()
            .map(|e| matches!(e, PkgError::DepConflict { name, .. } if name == "leaf"))
            .unwrap_or(false);
        assert!(conflict, "expected DepConflict for 'leaf', got: {err:#}");
        assert!(!lock_path.exists(), "lockfile must not be written on conflict");
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
    // Pure helpers (classify_tag_pin / pick_latest_*) are covered in
    // mlua_pkg::version unit tests.  The cases here exercise CLI-level
    // wiring: lock + manifest mutation behaviour around run_update.

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
    fn update_prefix_pin_refreshes_lock_without_mutating_manifest() {
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
        let original = format!(
            "[package]\nname = \"test\"\nversion = \"0.1.0\"\n\n\
             [deps]\nmylib = {{ git = \"{url}\", tag = \"v1.0\" }}\n"
        );
        write_file(&manifest_path, &original);

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

        // Manifest stays as the prefix — auto-follow intent is preserved.
        let after = std::fs::read_to_string(&manifest_path).unwrap();
        assert_eq!(
            after, original,
            "prefix pin manifest must not be rewritten:\n{after}"
        );

        // Lockfile records the resolved concrete tag (v1.0.5, not v1.1.0).
        let lf = Lockfile::read(&lock_path).unwrap();
        assert_eq!(lf.pkg.len(), 1, "one locked package");
        assert_eq!(
            lf.pkg[0].tag.as_deref(),
            Some("v1.0.5"),
            "lock must record resolved concrete tag"
        );
    }

    #[test]
    fn install_with_prefix_pin_resolves_to_concrete_tag() {
        let remote = TempDir::new().unwrap();
        init_repo_with_commit(remote.path());
        add_tag(remote.path(), "v1.0.0");
        add_tag(remote.path(), "v1.0.5");
        add_tag(remote.path(), "v2.0.0");

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

        run_install(&manifest_path, &cache_dir, &vendored_dir, &lock_path).unwrap();

        let lf = Lockfile::read(&lock_path).unwrap();
        assert_eq!(
            lf.pkg[0].tag.as_deref(),
            Some("v1.0.5"),
            "install must resolve prefix v1.0 to concrete v1.0.5 (excluding v2.0.0):\n{lf:?}"
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

    // ── resolve_mlua_pkgs_dir ─────────────────────────────────────────────────

    #[test]
    fn resolve_mlua_pkgs_dir_explicit_flag_wins() {
        let p = PathBuf::from("/some/custom/path");
        assert_eq!(resolve_mlua_pkgs_dir(Some(&p)), p);
    }

    #[test]
    fn resolve_mlua_pkgs_dir_auto_detect_target() {
        // Verify auto-detect honours the cwd: when target/ exists pick it,
        // otherwise fall back to .mlua-pkgs.  Run in an isolated TempDir to
        // avoid clobbering whatever the test runner's cwd looked like.
        let tmp = TempDir::new().unwrap();
        let prev = std::env::current_dir().unwrap();
        // Clear env to take the env branch out of the picture.
        let saved_env = std::env::var("MLUA_PKG_DIR").ok();
        std::env::remove_var("MLUA_PKG_DIR");

        std::env::set_current_dir(tmp.path()).unwrap();
        assert_eq!(resolve_mlua_pkgs_dir(None), PathBuf::from(".mlua-pkgs"));

        std::fs::create_dir(tmp.path().join("target")).unwrap();
        assert_eq!(
            resolve_mlua_pkgs_dir(None),
            PathBuf::from("target/mlua-pkgs")
        );

        std::env::set_current_dir(prev).unwrap();
        if let Some(v) = saved_env {
            std::env::set_var("MLUA_PKG_DIR", v);
        }
    }

    #[test]
    fn strip_cargo_subcommand_handles_empty() {
        let out = strip_cargo_subcommand(Vec::<String>::new());
        assert!(out.is_empty());
    }
}
