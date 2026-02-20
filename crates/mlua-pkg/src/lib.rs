//! # mlua-pkg
//!
//! Composable Lua module loader built in Rust.
//!
//! # Design philosophy
//!
//! Lua's `require("name")` is a `name -> value` transformation.
//! This crate defines that transformation as a **composable abstraction**,
//! allowing multiple sources (memory, filesystem, Rust functions, assets)
//! to be handled uniformly.
//!
//! # Resolution model
//!
//! ## Abstractions
//!
//! | Concept | Type | Role |
//! |---------|------|------|
//! | Resolution unit | [`Resolver`] | `name -> Option<Result<Value>>` |
//! | Composition (Chain) | [`Registry`] | Resolvers in priority order, first match wins |
//! | Composition (Prefix) | [`resolvers::PrefixResolver`] | Strip prefix and delegate to inner Resolver |
//!
//! Resolvers come in two kinds: **leaf** (directly produce values) and
//! **combinator** (compose other Resolvers). Both implement the same
//! [`Resolver`] trait, enabling infinite composition.
//!
//! ## Leaf Resolvers
//!
//! | Resolver | Source | Match condition |
//! |----------|--------|----------------|
//! | [`resolvers::MemoryResolver`] | `HashMap<String, String>` | Name is registered |
//! | [`resolvers::NativeResolver`] | `Fn(&Lua) -> Result<Value>` | Name is registered |
//! | [`resolvers::FsResolver`] | Filesystem | File exists |
//! | [`resolvers::AssetResolver`] | Filesystem | Known extension + file exists |
//!
//! ## Combinators
//!
//! | Combinator | Behavior |
//! |------------|----------|
//! | [`Registry`] (Chain) | Try `[R1, R2, ..., Rn]` in order, adopt first `Some` |
//! | [`resolvers::PrefixResolver`] | `"prefix.rest"` -> strip prefix -> delegate `"rest"` to inner Resolver |
//!
//! ## Resolution flow
//!
//! ```text
//! require("name")
//!   |
//!   v
//! package.searchers[1]  <- Registry inserts its hook here
//!   |
//!   +- Resolver A: resolve(lua, "name") -> None (not responsible)
//!   +- Resolver B: resolve(lua, "name") -> Some(Ok(Value)) (first match wins)
//!   |
//!   v
//! package.loaded["name"] = Value  <- Lua standard require auto-caches
//! ```
//!
//! # Return value protocol
//!
//! | Return value | Meaning | Next Resolver |
//! |-------------|---------|---------------|
//! | `None` | Not this Resolver's responsibility | Tried |
//! | `Some(Ok(value))` | Resolution succeeded | Skipped |
//! | `Some(Err(e))` | Responsible but load failed | **Skipped** |
//!
//! `Some(Err)` intentionally does not fall through to the next Resolver.
//! If a module was "found but broken", having another Resolver return
//! something different would be a source of bugs.
//!
//! # Naming conventions
//!
//! | Name pattern | Example | Responsible Resolver |
//! |-------------|---------|---------------------|
//! | `@scope/name` | `@std/http` | [`resolvers::NativeResolver`] -- exact name match |
//! | `prefix.name` | `game.engine` | [`resolvers::PrefixResolver`] -> delegates to inner Resolver |
//! | `dot.separated` | `lib.helper` | [`resolvers::FsResolver`] -- `lib/helper.lua` |
//! | `name.ext` | `config.json` | [`resolvers::AssetResolver`] -- auto-convert by extension |
//!
//! [`resolvers::FsResolver`] converts dot separators to path separators
//! (`lib.helper` -> `lib/helper.lua`).
//! [`resolvers::AssetResolver`] treats filenames literally
//! (`config.json` -> `config.json`).
//! The two naturally partition by the presence of a file extension.
//!
//! # Composition example
//!
//! ```text
//! Registry (Chain)
//! +- NativeResolver            @std/http  -> factory(lua)
//! +- Prefix("sm", FsResolver)  sm.helper  -> strip -> helper.lua
//! +- FsResolver(root/)         sm         -> sm/init.lua
//! |                             lib.utils  -> lib/utils.lua
//! +- AssetResolver              config.json -> parse -> Table
//! ```
//!
//! [`resolvers::PrefixResolver`] acts as a namespace mount point.
//! `require("sm")` (init.lua) is handled by the outer [`resolvers::FsResolver`],
//! while `require("sm.helper")` is handled by [`resolvers::PrefixResolver`].
//! Responsibilities are clearly separated.
//!
//! # Usage
//!
//! ```rust
//! use mlua_pkg::{Registry, resolvers::*};
//! use mlua::Lua;
//!
//! # fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let lua = Lua::new();
//! let mut reg = Registry::new();
//!
//! // 1st: Rust native modules (highest priority)
//! reg.add(NativeResolver::new().add("@std/http", |lua| {
//!     let t = lua.create_table()?;
//!     t.set("version", 1)?;
//!     Ok(mlua::Value::Table(t))
//! }));
//!
//! // 2nd: Embedded Lua sources
//! reg.add(MemoryResolver::new().add("utils", "return { pi = 3.14 }"));
//!
//! // 3rd: Filesystem (sandboxed)
//! # let plugins = std::env::temp_dir().join("mlua_pkg_doctest_plugins");
//! # std::fs::create_dir_all(&plugins)?;
//! # let assets = std::env::temp_dir().join("mlua_pkg_doctest_assets");
//! # std::fs::create_dir_all(&assets)?;
//! reg.add(FsResolver::new(&plugins)?);
//!
//! // 4th: Assets (register parsers explicitly)
//! reg.add(AssetResolver::new(&assets)?
//!     .parser("json", json_parser())
//!     .parser("sql", text_parser()));
//! # std::fs::remove_dir_all(&plugins).ok();
//! # std::fs::remove_dir_all(&assets).ok();
//!
//! reg.install(&lua)?;
//!
//! // Lua side: require("@std/http"), require("utils"), etc.
//! # Ok(())
//! # }
//! ```
//!
//! # Lua integration
//!
//! [`Registry::install()`] inserts a hook at the front of Lua's
//! `package.searchers` table. It takes priority over the standard
//! `package.preload`, so registered Resolvers are tried first.
//!
//! Caching is delegated to Lua's standard `package.loaded`.
//! On the second and subsequent `require` calls for the same module,
//! Lua's cache hits and the Resolver is not invoked.
//!
//! # Error design
//!
//! | Error type | When raised | Defined in |
//! |-----------|-------------|-----------|
//! | [`ResolveError`] | During `resolve()` execution | This module |
//! | [`sandbox::InitError`] | During `FsSandbox::new()` construction | [`sandbox`] |
//! | [`sandbox::ReadError`] | During `SandboxedFs::read()` | [`sandbox`] |
//!
//! By separating construction-time and runtime errors at the type level,
//! callers can choose the appropriate recovery strategy.

pub mod resolvers;
pub mod sandbox;

use mlua::{Lua, Result, Value};
use std::path::PathBuf;

/// Configuration bundle for Lua dialect naming conventions.
///
/// Apply to [`FsResolver`](resolvers::FsResolver) and
/// [`PrefixResolver`](resolvers::PrefixResolver) via `with_convention()`
/// to prevent convention settings from scattering.
///
/// Individual `with_extension()` / `with_init_name()` / `with_separator()`
/// methods remain available. Calling them after `with_convention()` overrides
/// the corresponding field.
///
/// # Predefined conventions
///
/// | Constant | Extension | Init name | Separator |
/// |----------|-----------|-----------|-----------|
/// | [`LUA54`](Self::LUA54) | `lua` | `init` | `.` |
/// | [`LUAU`](Self::LUAU) | `luau` | `init` | `.` |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LuaConvention {
    /// File extension (`"lua"`, `"luau"`, etc.).
    pub extension: &'static str,
    /// Package entry point name (`"init"`, `"mod"`, etc.).
    pub init_name: &'static str,
    /// Module name separator. The `.` in `require("a.b")`.
    pub module_separator: char,
}

impl LuaConvention {
    /// Lua 5.4 standard convention.
    pub const LUA54: Self = Self {
        extension: "lua",
        init_name: "init",
        module_separator: '.',
    };

    /// Luau (Roblox Lua) convention.
    pub const LUAU: Self = Self {
        extension: "luau",
        init_name: "init",
        module_separator: '.',
    };
}

impl Default for LuaConvention {
    fn default() -> Self {
        Self::LUA54
    }
}

/// Error type for module resolution.
///
/// Structurally represents domain-specific errors that occur during `resolve()`.
/// Converted to a Lua error via [`mlua::Error::external()`] and can be
/// recovered on the caller side with `err.downcast_ref::<ResolveError>()`.
///
/// Construction-time errors (e.g. root directory not found) are returned as
/// [`sandbox::InitError`] and are not included here.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// Path access outside the sandbox detected.
    #[error("path traversal blocked: {name}")]
    PathTraversal { name: String },

    /// Asset parse failure.
    ///
    /// Generalized to hold different error types per parser.
    /// [`resolvers::json_parser()`] stores `serde_json::Error`;
    /// custom parsers can store any error type.
    #[error("asset parse error: {source}")]
    AssetParse {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// File I/O error.
    ///
    /// Raised when a file exists but cannot be read
    /// (e.g. permission denied, is a directory).
    #[error("I/O error on {}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Minimal abstraction for module resolution.
///
/// Receives `require(name)` and returns `Some(Result<Value>)` if this
/// Resolver is responsible. Returns `None` if not.
///
/// # Return value protocol
///
/// - `None` = "unknown name". The next Resolver gets a chance.
/// - `Some(Ok(v))` = resolution complete. This value is returned to Lua.
/// - `Some(Err(e))` = "responsible but failed". Propagated immediately as an error.
///
/// # Example
///
/// ```rust
/// use mlua_pkg::Resolver;
/// use mlua::{Lua, Result, Value};
///
/// struct VersionResolver;
///
/// impl Resolver for VersionResolver {
///     fn resolve(&self, lua: &Lua, name: &str) -> Option<Result<Value>> {
///         if name == "version" {
///             Some(lua.create_string("1.0.0").map(Value::String))
///         } else {
///             None
///         }
///     }
/// }
/// ```
pub trait Resolver: Send + Sync {
    fn resolve(&self, lua: &Lua, name: &str) -> Option<Result<Value>>;
}

/// Chain combinator for [`Resolver`]. Registration order = priority order. First match wins.
///
/// `install()` inserts a hook at the front (index 1) of Lua's `package.searchers`
/// table, routing all `require` calls through the registered Resolver chain.
/// Takes priority over Lua's standard `package.preload`.
///
/// Caching is delegated to Lua's standard `package.loaded`.
/// Resolvers do not need to manage their own cache.
/// On the second and subsequent `require` for the same module, the Resolver is not called.
///
/// # Lua searcher protocol
///
/// The hook conforms to the Lua 5.4 searcher protocol:
/// - If the searcher returns a `function`, `require` calls it as a loader
/// - If the searcher returns a `string`, it is collected as a "not found" reason in the error message
///
/// The loader receives `(name, loader_data)` (per Lua 5.4 spec).
///
/// # Thread safety
///
/// `Registry` itself is `Send + Sync` (all Resolvers must be `Send + Sync`).
/// After [`install()`](Registry::install), the Registry is wrapped in `Arc` and
/// shared via a Lua closure.
///
/// Thread safety of the installed hook depends on the `mlua` feature configuration:
///
/// | mlua feature | `Lua` bounds | Implication |
/// |-------------|-------------|-------------|
/// | (default) | `!Send` | `Lua` is confined to one thread. The hook is never called concurrently. |
/// | `send` | `Send + Sync` | `Lua` can be shared across threads. `Resolver: Send + Sync` ensures safe concurrent access. |
///
/// The `Send + Sync` bound on [`Resolver`] is required for forward compatibility
/// with mlua's `send` feature. Without the `send` feature, `Lua` is `!Send` and
/// the hook is inherently single-threaded.
pub struct Registry {
    resolvers: Vec<Box<dyn Resolver>>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        Self {
            resolvers: Vec::new(),
        }
    }

    /// Add a Resolver. Registration order = priority order.
    pub fn add(&mut self, resolver: impl Resolver + 'static) -> &mut Self {
        self.resolvers.push(Box::new(resolver));
        self
    }

    /// Insert a hook at the front of `package.searchers`.
    ///
    /// Consumes `self` and shares it via `Arc`.
    /// The Registry becomes immutable after install (Resolver priority is finalized).
    ///
    /// Returns an error if called more than once on the same Lua instance.
    /// Multiple Registries coexisting in the same searchers table would make
    /// priority order unpredictable, so this is intentionally prohibited.
    pub fn install(self, lua: &Lua) -> Result<()> {
        if lua.app_data_ref::<RegistryInstalled>().is_some() {
            return Err(mlua::Error::runtime(
                "Registry already installed on this Lua instance",
            ));
        }

        let searchers: mlua::Table = lua
            .globals()
            .get::<mlua::Table>("package")?
            .get("searchers")?;

        let registry = std::sync::Arc::new(self);
        let hook = lua.create_function(move |lua, name: String| {
            for resolver in &registry.resolvers {
                if let Some(result) = resolver.resolve(lua, &name) {
                    let value = result?;
                    let f = lua.create_function(move |_, (_name, _data): (String, Value)| {
                        Ok(value.clone())
                    })?;
                    return Ok(Value::Function(f));
                }
            }
            Ok(Value::String(
                lua.create_string(format!("\n\tno resolver for '{name}'"))?,
            ))
        })?;

        let len = searchers.raw_len();
        for i in (1..=len).rev() {
            let v: Value = searchers.raw_get(i)?;
            searchers.raw_set(i + 1, v)?;
        }
        searchers.raw_set(1, hook)?;
        lua.set_app_data(RegistryInstalled);

        Ok(())
    }
}

/// Marker for `install()` completion. Used to prevent double-install.
struct RegistryInstalled;

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    impl Resolver for Echo {
        fn resolve(&self, lua: &Lua, name: &str) -> Option<Result<Value>> {
            if name == "echo" {
                Some(lua.create_string("hello from echo").map(Value::String))
            } else {
                None
            }
        }
    }

    #[test]
    fn require_hits_resolver() {
        let lua = Lua::new();
        let mut reg = Registry::new();
        reg.add(Echo);
        reg.install(&lua).unwrap();

        let val: String = lua.load(r#"return require("echo")"#).eval().unwrap();
        assert_eq!(val, "hello from echo");
    }

    #[test]
    fn require_miss_falls_through() {
        let lua = Lua::new();
        let mut reg = Registry::new();
        reg.add(Echo);
        reg.install(&lua).unwrap();

        let result: mlua::Result<Value> = lua.load(r#"return require("nope")"#).eval();
        assert!(result.is_err());
    }

    #[test]
    fn registry_default() {
        let reg = Registry::default();
        assert_eq!(reg.resolvers.len(), 0);
    }

    #[test]
    fn double_install_rejected() {
        let lua = Lua::new();

        let reg1 = Registry::new();
        reg1.install(&lua).unwrap();

        let reg2 = Registry::new();
        let err = reg2.install(&lua).unwrap_err();
        assert!(
            err.to_string().contains("already installed"),
            "expected 'already installed' error, got: {err}"
        );
    }
}
