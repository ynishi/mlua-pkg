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
//! FsSandbox  MockSandbox (for testing)
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
///   and `read_to_string()`. For adversarial inputs, use the `cap-std` crate.
///   The [`SandboxedFs`] trait makes backend replacement straightforward.
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
