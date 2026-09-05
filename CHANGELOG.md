# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

### Changed

### Deprecated

### Removed

### Fixed

### Security

## [0.8.0] - 2026-09-05

### Changed

- **mlua 0.11 -> 0.12.**  `mlua` types appear in the public API (`Resolver`,
  `Registry::install`, native module closures), so a consumer on mlua 0.12
  could not link this crate alongside its own `mlua` (`mlua-sys` declares
  `links`, which forbids two versions in one build).  No source changes were
  needed; the full test suite passes unchanged on 0.12.
- **MSRV 1.85 -> 1.88**, required by mlua 0.12.

## [0.7.0] - 2026-09-01

### Added

- **`FsResolver::new_symlink_aware` / `AssetResolver::new_symlink_aware`.**
  `SymlinkAwareSandbox` already existed but was only reachable through
  `with_sandbox` or `VendoredResolver`, so consumers rooting a resolver at a
  directory of symlinks got the strict `FsSandbox` with no signal that an
  alternative existed — and hit `ResolveError::PathTraversal` on every
  `require` through a link.  The sandbox choice is now visible in the
  constructor surface.

### Changed

- **`SymlinkAwareSandbox` refreshes its allowed set lazily.**  The set of
  symlink targets was fixed at construction, so a package linked in
  afterwards (a later `mlua-pkg install` in a long-running host) was rejected
  until the sandbox was rebuilt.  The root is now rescanned once before a
  read is rejected as a traversal; successful reads are unaffected, so the
  `read_dir` cost stays on the would-be-error path.  A scan that fails
  outright (unreadable root) leaves the cached targets intact, so a
  transient I/O problem cannot silently demote known-good packages to
  traversal rejections.

### Documentation

- README lists `SymlinkAwareSandbox` alongside `FsSandbox` / `CapSandbox`,
  with guidance on picking a backend for linked package roots.
- The `Some(Err)` resolver contract now states its consequence for sandbox
  rejections: a boundary error shadows later resolvers for the names it
  rejects (other names in the chain are unaffected).

## [0.6.0] - 2026-06-25

### Added

- **Configurable `.mlua-pkgs` base directory.**  New global flag
  `--mlua-pkgs-dir <PATH>` and env var `MLUA_PKG_DIR` move cache + vendored
  output off the manifest root.  Resolution order: flag > env > auto.
  Auto-detect picks `target/mlua-pkgs` when a `target/` directory exists in
  the current working directory, otherwise falls back to the previous
  `.mlua-pkgs` default.

### Fixed

- **`cargo publish` no longer trips on the package cache.**  Cached git
  clones under `.mlua-pkgs/cache/` are nested git repositories, and cargo's
  VCS walker descends into them and flags upstream working-tree files as
  uncommitted — aborting `cargo publish` even when `.mlua-pkgs` is in
  `.gitignore` and excluded from the crate's `include` list.  With the new
  auto-detected `target/mlua-pkgs/` location cargo skips the cache entirely
  (cargo unconditionally ignores `target/`), removing the need for
  `--allow-dirty`.

## [0.5.0] - 2026-06-25

### Added

- **Per-dep `target_dir` (physical vendoring).**  When a manifest entry sets
  `target_dir = "<rel/path>"`, `mlua-pkg install` copies the resolved entry
  contents into that directory (manifest-relative) instead of creating the
  default `.mlua-pkgs/vendored/<name>` symlink.  Vendored output is
  versionable in the consumer's git tree and is removed + re-populated on
  each run (idempotent).  `mlua-pkg add --target-dir=<path>` writes the
  field for new deps.
- **`mlua-pkg update`** — refresh deps and bump tag pins.  Pin semantics:
  - `tag = "v1.0"` / `"v1"` (prefix) — interpreted as a SemVer range.  Both
    `install` and `update` resolve the prefix to the SemVer-max matching
    release on the remote (pre-release tags excluded).  The manifest stays
    as the prefix; only the lockfile records the concrete tag, so the pin
    keeps auto-following future patches.
  - `tag = "v1.0.0"` (full SemVer) — exact pin.  `update` leaves it alone
    by default; `--force` bumps to the SemVer-max release overall.
  - `branch = "..."` — refresh only (lock picks up new HEAD on re-install).
  - `rev = "..."` — skipped.
  - `--dry-run` prints the plan without writing the manifest or running
    install.
- **Lockfile records the resolved concrete tag.**  Prefix pins write the
  picked release (e.g. `tag = "v1.0.5"`) rather than the prefix string, so
  it is always clear which version is currently installed.
- New `mlua_pkg::version` module exposing `TagPin`, `classify_tag_pin`,
  `pick_latest_for_pin`, and `pick_latest_overall` for reuse.

### Changed

- `FetchedPkg` gains `resolved_tag: Option<String>` — the concrete tag name
  actually checked out (equals the manifest `tag` for exact pins, the
  picked release for prefix pins, `None` for rev / branch / HEAD).

## [0.4.1] - 2026-06-25

### Fixed

- **`GitFetcher` now checks out the resolved commit.** v0.4.0 cloned the
  repository and pinned the SHA correctly in the cache directory name and
  lockfile, but left the worktree on the cloned default-branch `HEAD`.
  Consumers requesting `tag = "v0.1.0"` received the content of `main`
  instead, breaking reproducibility.  Fix resets the worktree hard to the
  resolved SHA right after `resolve_sha`.

### Added

- Regression unit test (`fetched_worktree_matches_resolved_tag_not_head`)
  that creates two commits, tags the first, and asserts the fetched
  worktree contains the tag-commit content.
- Network E2E (`install_lshape_v010_has_correct_content`, `#[ignore]`)
  that fetches `ynishi/lshape` v0.1.0 and asserts `M._VERSION = "0.1.0"`.

## [0.4.0] - 2026-06-24

### Added

- **PkgMgr facade** — end-to-end Lua package management from a single crate.
- `manifest` module — parses `mlua-pkg.toml` consumer and author manifests.
  `Manifest::from_path`, `Package`, `Dep` types with `deny_unknown_fields` serde.
- `lockfile` module — reads and writes `mlua-pkg.lock` with diff-stable sorted
  output.  `Lockfile::read`, `Lockfile::write`, `LockedPkg`.
- `fetcher` module — `GitFetcher` backed by `git2` (libgit2) for CI-stable
  clone-and-cache with `file://`, `https://`, and SSH remotes.
  Cache layout: `<cache_root>/git/<host>/<path>/<sha>/`.
- `resolvers::VendoredResolver` — wraps `FsResolver` over
  `.mlua-pkgs/vendored/` populated by the CLI.
  `VendoredResolver::from_lockfile` reads the lockfile and warns on missing
  symlinks.
- `resolve_entry` — entry fallback chain: `src/` → `lua/` → repo root.
- `mlua-pkg` CLI binary (`mlua-pkg install`, `add`, `update`, `clean`).
- `cargo-mlua-pkg` binary — same CLI exposed as a Cargo subcommand.
  `cargo install mlua-pkg` enables `cargo mlua-pkg install` in CI without
  managing PATH explicitly. Both binaries share the same source.
- Integration smoke tests in `tests/pkgmgr_smoke.rs` using local `file://`
  git fixtures (network-free by default; real GitHub test opt-in via
  `cargo test -- --ignored`).
- `git2 = "0.18"` in `[dependencies]`.

## [0.3.0] - 2026-06-17

### Added

- `SymlinkAwareSandbox` for symlink-following package resolution.

## [0.2.1] - 2026-06-10

### Fixed

- Downgraded Rust edition from 2024 to 2021 for broader toolchain compatibility.

## [0.2.0] - 2026-06-03

### Added

- Initial public release with `Registry`, `Resolver` trait, `MemoryResolver`,
  `FsResolver`, `NativeResolver`, `AssetResolver`, `PrefixResolver`, and
  `SandboxedFs` / `CapSandbox` sandbox support.
