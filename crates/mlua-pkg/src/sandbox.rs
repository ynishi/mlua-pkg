//! Sandboxed file I/O abstraction and implementation.
//!
//! The [`SandboxedFs`] trait defines the I/O interface, and
//! [`FsSandbox`] provides the real filesystem implementation.
//!
//! During testing, inject a mock implementation for I/O-free verification.
//!
//! # Design
//!
//! ```text
//! FsResolver / AssetResolver
//!       |
//!       v
//! Box<dyn SandboxedFs>   <- Dependency inversion. Implementation is swappable
//!       |
//!   +---+---+
//!   |       |
//! FsSandbox  CapSandbox (cap-std)  MockSandbox (for testing)
//! ```
//!
//! Rationale for using `Box<dyn SandboxedFs>` (dynamic dispatch):
//! - [`Resolver`](crate::Resolver) itself uses `Vec<Box<dyn Resolver>>` with dynamic dispatch
//! - Making it generic would ultimately be converted to a trait object anyway, providing no benefit
//! - vtable overhead (~ns) is negligible compared to I/O (~us to ~ms)
//!
//! # Error type separation
//!
//! Construction-time and read-time errors are separated by type:
//! - [`InitError`] -- returned from [`FsSandbox::new()`]. Root directory validation errors.
//! - [`ReadError`] -- returned from [`SandboxedFs::read()`]. Individual file access errors.
//!
//! Rationale: construction failure is a configuration error (should be fixed at startup),
//! while read failure is a runtime error (fallback or retry may be possible).
//! This separation lets callers choose the appropriate recovery strategy.
//!
//! # NotFound representation
//!
//! File not found is returned as `Ok(None)` (not `Err`).
//! [`SandboxedFs::read()`] is a "search" operation where absence is a normal result.
//! This fits naturally with [`FsResolver`](crate::resolvers::FsResolver)'s candidate chain
//! (`{name}.lua` -> `{name}/init.lua`).

use std::path::{Path, PathBuf};

/// File read result.
pub struct FileContent {
    /// File content (UTF-8 text).
    pub content: String,
    /// Canonicalized real path. Used as source name in error messages.
    pub resolved_path: PathBuf,
}

/// Error during sandbox construction.
///
/// Returned from [`FsSandbox::new()`].
/// Contains only errors related to root directory validation.
#[derive(Debug, thiserror::Error)]
pub enum InitError {
    /// Root directory does not exist.
    #[error("root directory not found: {}", path.display())]
    RootNotFound { path: PathBuf },

    /// I/O error on root directory (e.g. permission denied).
    #[error("I/O error on {}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Error during file read.
///
/// Returned from [`SandboxedFs::read()`].
/// Contains only errors related to individual file access.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// Access outside the sandbox boundary detected.
    #[error("path traversal detected: {}", attempted.display())]
    Traversal { attempted: PathBuf },

    /// File I/O error (e.g. permission denied, reading a directory).
    ///
    /// `NotFound` is not included here (represented as `Ok(None)`).
    #[error("I/O error on {}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Interface for sandboxed file reading.
///
/// An I/O abstraction. Swap the implementation for test mocks or
/// alternative backends (in-memory FS, embedded assets, etc.).
pub trait SandboxedFs: Send + Sync {
    /// Read a file by relative path.
    ///
    /// - `Ok(Some(file))`: Read succeeded
    /// - `Ok(None)`: File does not exist
    /// - `Err(Traversal)`: Access outside sandbox boundary
    /// - `Err(Io)`: I/O error (e.g. permission denied)
    fn read(&self, relative: &Path) -> Result<Option<FileContent>, ReadError>;
}

/// Real filesystem-based sandbox implementation.
///
/// Canonicalizes the root at construction time and performs traversal
/// validation on every read.
///
/// # Security boundary
///
/// This sandbox provides **casual escape prevention for trusted directories**,
/// not a security guarantee for adversarial environments.
///
/// ## Known limitations
///
/// - **TOCTOU**: Vulnerable to symlink swap attacks between `canonicalize()`
///   and `read_to_string()`. For adversarial inputs, use [`CapSandbox`]
///   (requires the `sandbox-cap-std` feature) which eliminates the gap via
///   OS-level capability-based file access.
///
/// - **Windows device names**: No defense against reserved device names like
///   `NUL`, `CON`, `PRN`, etc. Risk of DoS/hang on Windows.
pub struct FsSandbox {
    root: PathBuf,
}

impl FsSandbox {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, InitError> {
        let raw = root.into();
        let canonical = match raw.canonicalize() {
            Ok(p) => p,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(InitError::RootNotFound { path: raw });
            }
            Err(e) => {
                return Err(InitError::Io {
                    path: raw,
                    source: e,
                });
            }
        };
        Ok(Self { root: canonical })
    }
}

impl SandboxedFs for FsSandbox {
    fn read(&self, relative: &Path) -> Result<Option<FileContent>, ReadError> {
        let path = self.root.join(relative);
        let canonical = match path.canonicalize() {
            Ok(p) => p,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(ReadError::Io { path, source: e });
            }
        };

        if !canonical.starts_with(&self.root) {
            return Err(ReadError::Traversal {
                attempted: canonical,
            });
        }

        match std::fs::read_to_string(&canonical) {
            Ok(content) => Ok(Some(FileContent {
                content,
                resolved_path: canonical,
            })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ReadError::Io {
                path: canonical,
                source: e,
            }),
        }
    }
}

// -- SymlinkAwareSandbox --

/// Sandbox that follows symlinks in the root directory.
///
/// Like [`FsSandbox`], but also allows access to targets of symlinks
/// found directly under the root. This is designed for package managers
/// (e.g. `npm link` / `alc_pkg_link`) where the root directory contains
/// symlinks pointing to external source directories.
///
/// # How it works
///
/// At construction time, scans the root for symlink entries and records
/// their canonical targets as additional allowed roots. During `read()`,
/// a file is permitted if its canonical path is under the root **or**
/// under any of the recorded symlink targets.
///
/// If both checks fail, the root is rescanned once before the read is
/// rejected, so symlinks created *after* construction (e.g. a later
/// `mlua-pkg install` in a long-running host) are picked up without
/// rebuilding the sandbox. The rescan runs only on the path that would
/// otherwise return [`ReadError::Traversal`]; successful reads never
/// touch the filesystem beyond the file itself.
///
/// Rejection is therefore no longer free: it costs one `read_dir` plus a
/// `canonicalize` per root entry. Reads that miss because the file does not
/// exist are unaffected (they return `Ok(None)` before the boundary check),
/// so the cost falls only on names that resolve to a real file outside the
/// root — which then fail anyway.
///
/// # Security boundary
///
/// Same as [`FsSandbox`]: casual escape prevention for trusted directories.
/// Note that the rescan widens the allowed set to whatever symlinks exist
/// under the root at read time — the root directory itself must stay
/// trusted.
pub struct SymlinkAwareSandbox {
    root: PathBuf,
    /// Canonical paths of symlink targets found under root.
    ///
    /// Seeded at construction and refreshed lazily when a read would
    /// otherwise be rejected as a traversal.
    allowed_targets: std::sync::RwLock<Vec<PathBuf>>,
}

impl SymlinkAwareSandbox {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, InitError> {
        let raw = root.into();
        let canonical = match raw.canonicalize() {
            Ok(p) => p,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(InitError::RootNotFound { path: raw });
            }
            Err(e) => {
                return Err(InitError::Io {
                    path: raw,
                    source: e,
                });
            }
        };

        let allowed_targets = scan_symlink_targets(&canonical).unwrap_or_default();

        Ok(Self {
            root: canonical,
            allowed_targets: std::sync::RwLock::new(allowed_targets),
        })
    }

    /// Check `canonical` against the currently cached symlink targets.
    fn is_allowed(&self, canonical: &Path) -> bool {
        let targets = self
            .allowed_targets
            .read()
            .unwrap_or_else(|e| e.into_inner());
        targets.iter().any(|t| canonical.starts_with(t))
    }

    /// Rescan the root and re-check `canonical` against the refreshed set.
    ///
    /// Called only when [`is_allowed`](Self::is_allowed) already failed, so
    /// the `read_dir` cost is confined to the would-be-error path.
    ///
    /// A scan that fails outright (unreadable root) leaves the cached set
    /// untouched: "the root could not be listed" must not be recorded as
    /// "the root has no symlinks", which would drop known-good targets and
    /// turn a transient I/O problem into a permanent traversal rejection.
    fn rescan_and_check(&self, canonical: &Path) -> bool {
        let Some(fresh) = scan_symlink_targets(&self.root) else {
            return false;
        };
        let allowed = fresh.iter().any(|t| canonical.starts_with(t));
        let mut targets = self
            .allowed_targets
            .write()
            .unwrap_or_else(|e| e.into_inner());
        *targets = fresh;
        allowed
    }
}

/// Collect canonical targets of symlinks located directly under `root`.
///
/// Returns `None` if `root` itself could not be listed, so callers can tell
/// that apart from a root that genuinely holds no symlinks. Individual
/// entries that cannot be inspected are skipped: an unresolvable target
/// simply stays outside the allowed set and the read is rejected.
fn scan_symlink_targets(root: &Path) -> Option<Vec<PathBuf>> {
    let entries = std::fs::read_dir(root).ok()?;
    let mut targets = Vec::new();
    for entry in entries.flatten() {
        let meta = match entry.path().symlink_metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            if let Ok(target) = entry.path().canonicalize() {
                targets.push(target);
            }
        }
    }
    Some(targets)
}

impl SandboxedFs for SymlinkAwareSandbox {
    fn read(&self, relative: &Path) -> Result<Option<FileContent>, ReadError> {
        let path = self.root.join(relative);
        let canonical = match path.canonicalize() {
            Ok(p) => p,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(ReadError::Io { path, source: e });
            }
        };

        // Allow if under root (normal case, no symlinks involved)
        if canonical.starts_with(&self.root) {
            return read_file(&canonical);
        }

        // Allow if under any known symlink target
        if self.is_allowed(&canonical) {
            return read_file(&canonical);
        }

        // The symlink may have appeared after construction — refresh once
        // before rejecting.
        if self.rescan_and_check(&canonical) {
            return read_file(&canonical);
        }

        Err(ReadError::Traversal {
            attempted: canonical,
        })
    }
}

/// Shared file reading logic.
fn read_file(canonical: &Path) -> Result<Option<FileContent>, ReadError> {
    match std::fs::read_to_string(canonical) {
        Ok(content) => Ok(Some(FileContent {
            content,
            resolved_path: canonical.to_path_buf(),
        })),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ReadError::Io {
            path: canonical.to_path_buf(),
            source: e,
        }),
    }
}

// -- CapSandbox --

/// Capability-based sandbox using [`cap_std`].
///
/// Eliminates the TOCTOU gap present in [`FsSandbox`] by using OS-level
/// capability-based file access (`openat2` / `RESOLVE_BENEATH` on Linux,
/// equivalent mechanisms on other platforms).
///
/// # Security properties
///
/// - **No TOCTOU gap**: Path resolution and file open happen atomically
///   within the OS kernel (on supported platforms).
/// - **Symlink escape prevention**: Handled by the OS, not userspace checks.
/// - **No `canonicalize()` step**: The directory capability itself defines
///   the sandbox boundary.
///
/// # Symlink behavior
///
/// Symlinks that resolve outside the sandbox are always blocked.
/// Handling of symlinks within the sandbox is platform-dependent
/// (Linux `RESOLVE_BENEATH` follows them; other platforms may not).
/// For portable behavior, avoid symlinks inside sandbox directories.
///
/// # Behavioral differences from [`FsSandbox`]
///
/// | Aspect | `FsSandbox` | `CapSandbox` |
/// |--------|-------------|--------------|
/// | Traversal error | `ReadError::Traversal` | `ReadError::Io` (OS-level denial) |
/// | `resolved_path` | Absolute canonical path | Relative path as given |
/// | TOCTOU | Vulnerable | Eliminated |
///
/// Traversal attempts are blocked by the OS before reaching userspace.
/// The returned `ReadError::Io` will carry the platform-specific error
/// (e.g. `EXDEV`, `EACCES`).
///
/// # Example
///
/// ```rust,no_run
/// use mlua_pkg::{resolvers::FsResolver, sandbox::CapSandbox};
///
/// let sandbox = CapSandbox::new("./scripts")?;
/// let resolver = FsResolver::with_sandbox(sandbox);
/// # Ok::<(), mlua_pkg::sandbox::InitError>(())
/// ```
///
/// # Availability
///
/// Requires the `sandbox-cap-std` feature:
///
/// ```toml
/// mlua-pkg = { version = "0.1", features = ["sandbox-cap-std"] }
/// ```
#[cfg(feature = "sandbox-cap-std")]
pub struct CapSandbox {
    dir: cap_std::fs::Dir,
}

#[cfg(feature = "sandbox-cap-std")]
impl CapSandbox {
    /// Open a directory as a capability-based sandbox.
    ///
    /// Uses [`cap_std::fs::Dir::open_ambient_dir`] to obtain a directory
    /// handle. All subsequent reads are confined to this directory by the OS.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, InitError> {
        let raw = root.into();
        let dir = match cap_std::fs::Dir::open_ambient_dir(&raw, cap_std::ambient_authority()) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(InitError::RootNotFound { path: raw });
            }
            Err(e) => {
                return Err(InitError::Io {
                    path: raw,
                    source: e,
                });
            }
        };
        Ok(Self { dir })
    }
}

#[cfg(feature = "sandbox-cap-std")]
impl SandboxedFs for CapSandbox {
    fn read(&self, relative: &Path) -> Result<Option<FileContent>, ReadError> {
        match self.dir.read_to_string(relative) {
            Ok(content) => Ok(Some(FileContent {
                content,
                resolved_path: relative.to_path_buf(),
            })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ReadError::Io {
                path: relative.to_path_buf(),
                source: e,
            }),
        }
    }
}
