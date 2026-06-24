# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
