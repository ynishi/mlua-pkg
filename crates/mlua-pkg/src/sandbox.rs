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
        let dir = cap_std::fs::Dir::open_ambient_dir(&raw, cap_std::ambient_authority()).map_err(
            |e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    InitError::RootNotFound { path: raw.clone() }
                } else {
                    InitError::Io {
                        path: raw.clone(),
                        source: e,
                    }
                }
            },
        )?;
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
