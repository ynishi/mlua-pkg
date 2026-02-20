//! Resolver implementations: leaves and combinators.
//!
//! # Leaf Resolvers
//!
//! Terminal resolvers that directly produce values.
//!
//! | Resolver | Source | Match condition | Use case |
//! |----------|--------|----------------|----------|
//! | [`MemoryResolver`] | `HashMap<String, String>` | Name is registered | `include_str!` embedding, preload |
//! | [`NativeResolver`] | `Fn(&Lua) -> Result<Value>` | Name is registered | Build tables from Rust (`@std/*`, etc.) |
//! | [`FsResolver`] | Filesystem | File exists | Sandboxed, `init.lua` fallback |
//! | [`AssetResolver`] | Filesystem | Known extension + file exists | Auto-convert non-Lua resources (JSON->Table, etc.) |
//!
//! # Combinators
//!
//! Resolvers that compose other Resolvers. Since they implement the [`Resolver`] trait,
//! they can be added to a Registry just like leaves, and combinators can nest.
//!
//! | Combinator | Behavior | Use case |
//! |------------|----------|----------|
//! | [`PrefixResolver`] | `"prefix.rest"` -> strip prefix -> delegate to inner Resolver | Namespace mounting |
//!
//! # Composition patterns
//!
//! ```text
//! Registry (Chain)
//! +- NativeResolver                 @std/http  -> Rust factory
//! +- PrefixResolver("game", ...)    game.xxx   -> delegate to inner Resolver
//! |   +- FsResolver(game_dir/)      xxx        -> game_dir/xxx.lua
//! +- FsResolver(scripts/)           game       -> scripts/game/init.lua
//! |                                  lib.utils  -> scripts/lib/utils.lua
//! +- AssetResolver(assets/)         config.json -> JSON parse -> Table
//! ```
//!
//! [`PrefixResolver`] acts as a namespace mount point.
//! `require("game")` (init.lua) is handled by the outer [`FsResolver`],
//! while `require("game.engine")` is handled by [`PrefixResolver`].
//! Responsibilities are clearly separated.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use mlua::{Lua, LuaSerdeExt, Result, Value};

use crate::sandbox::{FsSandbox, InitError, ReadError, SandboxedFs};
use crate::{ResolveError, Resolver};

type NativeFactory = Box<dyn Fn(&Lua) -> Result<Value> + Send + Sync>;

/// Domain conversion from ReadError to ResolveError.
///
/// Attaches module name domain context to infrastructure-layer errors
/// that occur during `resolve()` execution.
///
/// `sanitized_path` should be a relative path within the sandbox.
/// Absolute paths generated inside FsSandbox (containing host OS information)
/// are replaced with relative paths during conversion to prevent leaking
/// to the Lua side.
fn read_to_resolve_error(err: ReadError, name: &str, sanitized_path: &Path) -> ResolveError {
    match err {
        ReadError::Traversal { .. } => ResolveError::PathTraversal {
            name: name.to_owned(),
        },
        ReadError::Io { source, .. } => ResolveError::Io {
            path: sanitized_path.to_path_buf(),
            source,
        },
    }
}

// -- MemoryResolver --

/// Resolver that holds Lua source strings in memory.
///
/// Makes modules embedded via `include_str!` or dynamically generated
/// sources available through `require`.
///
/// Cross-module `require` chains also work
/// (delegated to other Resolvers via the Registry).
///
/// ```rust
/// use mlua_pkg::resolvers::MemoryResolver;
///
/// let r = MemoryResolver::new()
///     .add("mylib", "return { version = 1 }")
///     .add("mylib.utils", "return { helper = true }");
/// ```
pub struct MemoryResolver {
    modules: HashMap<String, String>,
}

impl Default for MemoryResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryResolver {
    pub fn new() -> Self {
        Self {
            modules: HashMap::new(),
        }
    }

    /// Register a module. Duplicate names are overwritten.
    pub fn add(mut self, name: impl Into<String>, source: impl Into<String>) -> Self {
        self.modules.insert(name.into(), source.into());
        self
    }
}

impl Resolver for MemoryResolver {
    fn resolve(&self, lua: &Lua, name: &str) -> Option<Result<Value>> {
        let source = self.modules.get(name)?;
        Some(lua.load(source.as_str()).set_name(name).eval())
    }
}

// -- NativeResolver --

/// Resolver that builds Lua Values directly from Rust functions.
///
/// Provides native modules like `@std/http`.
/// Since the factory function returns a Lua Value, table construction
/// and function registration are fully controlled on the Rust side.
///
/// ```rust
/// use mlua_pkg::resolvers::NativeResolver;
/// use mlua::Value;
///
/// let r = NativeResolver::new().add("@std/version", |lua| {
///     lua.create_string("1.0.0").map(Value::String)
/// });
/// ```
pub struct NativeResolver {
    modules: HashMap<String, NativeFactory>,
}

impl Default for NativeResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeResolver {
    pub fn new() -> Self {
        Self {
            modules: HashMap::new(),
        }
    }

    /// Register a native module.
    pub fn add(
        mut self,
        name: impl Into<String>,
        factory: impl Fn(&Lua) -> Result<Value> + Send + Sync + 'static,
    ) -> Self {
        self.modules.insert(name.into(), Box::new(factory));
        self
    }
}

impl Resolver for NativeResolver {
    fn resolve(&self, lua: &Lua, name: &str) -> Option<Result<Value>> {
        let factory = self.modules.get(name)?;
        Some(factory(lua))
    }
}

// -- FsResolver --

/// Sandboxed filesystem Resolver.
///
/// Resolves `require("lib.helper")` to `{root}/lib/helper.lua`.
/// Converts module separator to path separator and searches in order:
///
/// 1. `{root}/{name}.{extension}`
/// 2. `{root}/{name}/{init_name}.{extension}`
///
/// Defaults to [`LuaConvention::LUA54`](crate::LuaConvention::LUA54).
/// Use [`with_convention()`](FsResolver::with_convention) for bulk changes,
/// or individual methods for partial overrides.
///
/// I/O goes through the [`SandboxedFs`] trait. Use [`with_sandbox`](FsResolver::with_sandbox)
/// to inject test mocks or alternative backends.
///
/// # Errors
///
/// `new()` returns [`InitError::RootNotFound`] if the root does not exist.
pub struct FsResolver {
    sandbox: Box<dyn SandboxedFs>,
    extension: String,
    init_name: String,
    module_separator: char,
}

impl FsResolver {
    /// Build an FsResolver backed by the real filesystem.
    pub fn new(root: impl Into<PathBuf>) -> std::result::Result<Self, InitError> {
        let fs = FsSandbox::new(root)?;
        Ok(Self::with_sandbox(fs))
    }

    /// Inject an arbitrary [`SandboxedFs`] implementation.
    pub fn with_sandbox(sandbox: impl SandboxedFs + 'static) -> Self {
        let conv = crate::LuaConvention::default();
        Self {
            sandbox: Box::new(sandbox),
            extension: conv.extension.to_owned(),
            init_name: conv.init_name.to_owned(),
            module_separator: conv.module_separator,
        }
    }

    /// Apply a [`LuaConvention`](crate::LuaConvention) in bulk.
    pub fn with_convention(self, conv: crate::LuaConvention) -> Self {
        Self {
            extension: conv.extension.to_owned(),
            init_name: conv.init_name.to_owned(),
            module_separator: conv.module_separator,
            ..self
        }
    }

    /// Change the file extension (default: `lua`).
    pub fn with_extension(mut self, ext: impl Into<String>) -> Self {
        self.extension = ext.into();
        self
    }

    /// Change the package entry point filename (default: `init`).
    ///
    /// `require("pkg")` resolves to `pkg/{init_name}.{extension}`.
    pub fn with_init_name(mut self, name: impl Into<String>) -> Self {
        self.init_name = name.into();
        self
    }

    /// Change the module name separator (default: `.`).
    ///
    /// `require("a{sep}b")` is converted to `a/b.{extension}`.
    pub fn with_module_separator(mut self, sep: char) -> Self {
        self.module_separator = sep;
        self
    }
}

impl Resolver for FsResolver {
    fn resolve(&self, lua: &Lua, name: &str) -> Option<Result<Value>> {
        let relative = name.replace(self.module_separator, "/");

        let candidates = [
            PathBuf::from(format!("{relative}.{}", self.extension)),
            PathBuf::from(format!("{relative}/{}.{}", self.init_name, self.extension)),
        ];

        for candidate in &candidates {
            match self.sandbox.read(candidate) {
                Ok(Some(file)) => {
                    let source_name = candidate.display().to_string();
                    return Some(lua.load(file.content).set_name(source_name).eval());
                }
                Ok(None) => continue,
                Err(e) => {
                    return Some(Err(mlua::Error::external(read_to_resolve_error(
                        e, name, candidate,
                    ))));
                }
            }
        }

        None
    }
}

// -- AssetResolver --

type AssetParserFn = Box<dyn Fn(&Lua, &str) -> Result<Value> + Send + Sync>;

/// Resolver that registers parsers by extension and auto-converts non-Lua resources.
///
/// Parsers are registered per extension via `.parser()`.
/// For unregistered extensions, no I/O is performed and `None` is returned.
///
/// Filenames are treated literally (no dot-to-path conversion).
///
/// # Built-in parsers
///
/// | Factory function | Conversion |
/// |-----------------|------------|
/// | [`json_parser()`] | Parse with `serde_json` -> Lua Table |
/// | [`text_parser()`] | Return as-is as Lua String |
///
/// # Examples
///
/// ```rust
/// use mlua_pkg::resolvers::{AssetResolver, json_parser, text_parser};
///
/// # fn example() -> Result<(), mlua_pkg::sandbox::InitError> {
/// let resolver = AssetResolver::new("./assets")?
///     .parser("json", json_parser())
///     .parser("sql", text_parser())
///     .parser("css", text_parser());
/// # Ok(())
/// # }
/// ```
///
/// Custom parsers can also be registered as closures:
///
/// ```rust
/// use mlua_pkg::resolvers::{AssetResolver, json_parser};
///
/// # fn example() -> Result<(), mlua_pkg::sandbox::InitError> {
/// let resolver = AssetResolver::new("./assets")?
///     .parser("json", json_parser())
///     .parser("csv", |lua, content| {
///         // Split by lines and convert to a Lua table
///         let t = lua.create_table()?;
///         for (i, line) in content.lines().enumerate() {
///             t.set(i + 1, lua.create_string(line)?)?;
///         }
///         Ok(mlua::Value::Table(t))
///     });
/// # Ok(())
/// # }
/// ```
///
/// I/O goes through the [`SandboxedFs`] trait. Use [`with_sandbox`](AssetResolver::with_sandbox)
/// to inject test mocks or alternative backends.
///
/// # Design decision: why extension keys are `String`
///
/// Parser registration uses `HashMap<String, BoxFn>`.
/// String keys are chosen over enums to prioritize extensibility (Open/Closed),
/// allowing users to freely register custom parsers for any extension.
///
/// Impact of a typo: `parsers.get(ext)` returns `None` -> safely falls through to the
/// next Resolver. No panic/UB occurs. Setup code is small, so typos surface immediately in tests.
///
/// # Errors
///
/// `new()` returns [`InitError::RootNotFound`] if the root does not exist.
pub struct AssetResolver {
    sandbox: Box<dyn SandboxedFs>,
    parsers: HashMap<String, AssetParserFn>,
}

impl AssetResolver {
    /// Build an AssetResolver backed by the real filesystem.
    pub fn new(root: impl Into<PathBuf>) -> std::result::Result<Self, InitError> {
        let fs = FsSandbox::new(root)?;
        Ok(Self::with_sandbox(fs))
    }

    /// Inject an arbitrary [`SandboxedFs`] implementation.
    pub fn with_sandbox(sandbox: impl SandboxedFs + 'static) -> Self {
        Self {
            sandbox: Box::new(sandbox),
            parsers: HashMap::new(),
        }
    }

    /// Register a parser for an extension. Duplicate extensions are overwritten.
    pub fn parser(
        mut self,
        ext: impl Into<String>,
        f: impl Fn(&Lua, &str) -> Result<Value> + Send + Sync + 'static,
    ) -> Self {
        self.parsers.insert(ext.into(), Box::new(f));
        self
    }
}

/// JSON -> Lua Table parser.
///
/// Parses with `serde_json` and converts to a Lua Table via [`LuaSerdeExt::to_value`].
/// Returns [`ResolveError::AssetParse`] on parse failure.
pub fn json_parser() -> impl Fn(&Lua, &str) -> Result<Value> + Send + Sync {
    |lua, content| {
        let json: serde_json::Value = serde_json::from_str(content).map_err(|e| {
            mlua::Error::external(ResolveError::AssetParse {
                source: Box::new(e),
            })
        })?;
        lua.to_value(&json)
    }
}

/// Text -> Lua String parser.
///
/// Returns the file content as-is as a Lua String.
/// Use for `.txt`, `.sql`, `.html`, `.css`, etc.
pub fn text_parser() -> impl Fn(&Lua, &str) -> Result<Value> + Send + Sync {
    |lua, content| lua.create_string(content).map(Value::String)
}

impl Resolver for AssetResolver {
    fn resolve(&self, lua: &Lua, name: &str) -> Option<Result<Value>> {
        let ext = Path::new(name).extension()?.to_str()?;
        let parser = self.parsers.get(ext)?;

        let asset_path = Path::new(name);
        let file = match self.sandbox.read(asset_path) {
            Ok(Some(file)) => file,
            Ok(None) => return None,
            Err(e) => {
                return Some(Err(mlua::Error::external(read_to_resolve_error(
                    e, name, asset_path,
                ))));
            }
        };

        Some(parser(lua, &file.content))
    }
}

// -- PrefixResolver --

/// Combinator that routes to an inner Resolver by name prefix.
///
/// Receives `require("{prefix}{sep}{rest}")`, strips the prefix and separator,
/// and delegates `{rest}` to the inner Resolver.
/// Returns `None` for names that don't match the prefix.
///
/// # Match rules
///
/// | Input | prefix="sm", sep='.' | Result |
/// |-------|---------------------|--------|
/// | `"sm.helper"` | Strip `"sm."` -> `"helper"` | Delegate to inner Resolver |
/// | `"sm.ui.btn"` | Strip `"sm."` -> `"ui.btn"` | Delegate to inner Resolver (multi-level) |
/// | `"sm"` | No separator -> no match | `None` (handled by outer Resolver) |
/// | `"smtp"` | Does not start with `"sm."` | `None` |
/// | `"other.x"` | Prefix mismatch | `None` |
///
/// # Design intent
///
/// `require("sm")` (package root = init.lua) is **outside** PrefixResolver's scope.
/// The outer [`FsResolver`] handles it via init.lua fallback.
/// This clearly separates responsibilities:
///
/// - **PrefixResolver**: `sm.xxx` -> submodules within the namespace
/// - **FsResolver**: `sm` -> `sm/init.lua` (package entry point)
///
/// # Composition example
///
/// ```rust
/// use mlua_pkg::{Registry, resolvers::*};
/// use mlua::Lua;
///
/// let lua = Lua::new();
/// let mut reg = Registry::new();
///
/// // "game.xxx" -> resolve within game_modules/
/// reg.add(PrefixResolver::new("game",
///     MemoryResolver::new()
///         .add("engine", "return { version = 2 }")
///         .add("utils", "return { helper = true }")));
///
/// // "game" -> init.lua provided directly via MemoryResolver
/// reg.add(MemoryResolver::new()
///     .add("game", "return { name = 'game' }"));
///
/// reg.install(&lua).unwrap();
///
/// // require("game.engine") -> PrefixResolver -> MemoryResolver("engine")
/// // require("game")        -> MemoryResolver("game")
/// ```
pub struct PrefixResolver {
    prefix: String,
    separator: char,
    inner: Box<dyn Resolver>,
}

impl PrefixResolver {
    /// Build a prefix router with `.` separator.
    ///
    /// `require("{prefix}.{rest}")` -> `inner.resolve("{rest}")`
    pub fn new(prefix: impl Into<String>, inner: impl Resolver + 'static) -> Self {
        Self {
            prefix: prefix.into(),
            separator: crate::LuaConvention::default().module_separator,
            inner: Box::new(inner),
        }
    }

    /// Apply a [`LuaConvention`](crate::LuaConvention) in bulk.
    pub fn with_convention(mut self, conv: crate::LuaConvention) -> Self {
        self.separator = conv.module_separator;
        self
    }

    /// Change the separator (default: `.`).
    pub fn with_separator(mut self, separator: char) -> Self {
        self.separator = separator;
        self
    }
}

impl Resolver for PrefixResolver {
    fn resolve(&self, lua: &Lua, name: &str) -> Option<Result<Value>> {
        let mut prefix_with_sep = String::with_capacity(self.prefix.len() + 1);
        prefix_with_sep.push_str(&self.prefix);
        prefix_with_sep.push(self.separator);

        let rest = name.strip_prefix(&prefix_with_sep)?;
        self.inner.resolve(lua, rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{FileContent, ReadError};

    /// Asserts that `resolve()` returns `Some(Ok(value))` and returns the value.
    fn must_resolve(resolver: &dyn Resolver, lua: &Lua, name: &str) -> Value {
        match resolver.resolve(lua, name) {
            Some(Ok(v)) => v,
            Some(Err(e)) => panic!("resolve('{name}') returned Err: {e}"),
            None => panic!("resolve('{name}') returned None"),
        }
    }

    /// Asserts that `resolve()` returns `Some(Err(_))` and returns the error message.
    fn must_resolve_err(resolver: &dyn Resolver, lua: &Lua, name: &str) -> String {
        match resolver.resolve(lua, name) {
            Some(Err(e)) => e.to_string(),
            Some(Ok(_)) => panic!("resolve('{name}') returned Ok, expected Err"),
            None => panic!("resolve('{name}') returned None, expected Some(Err)"),
        }
    }

    /// Extracts a table field from a Value.
    fn get_field<V: mlua::FromLua>(value: &Value, key: impl mlua::IntoLua) -> V {
        value
            .as_table()
            .expect("expected Table value")
            .get::<V>(key)
            .expect("table field access failed")
    }

    /// Mock sandbox for I/O-free testing.
    struct MockSandbox {
        files: HashMap<PathBuf, String>,
    }

    impl MockSandbox {
        fn new() -> Self {
            Self {
                files: HashMap::new(),
            }
        }

        fn with_file(mut self, path: impl Into<PathBuf>, content: &str) -> Self {
            self.files.insert(path.into(), content.to_owned());
            self
        }
    }

    impl SandboxedFs for MockSandbox {
        fn read(&self, relative: &Path) -> std::result::Result<Option<FileContent>, ReadError> {
            match self.files.get(relative) {
                Some(content) => Ok(Some(FileContent {
                    content: content.clone(),
                    resolved_path: relative.to_path_buf(),
                })),
                None => Ok(None),
            }
        }
    }

    #[test]
    fn fs_resolver_dot_to_path_conversion() {
        let mock = MockSandbox::new().with_file("lib/helper.lua", "return { name = 'mocked' }");
        let resolver = FsResolver::with_sandbox(mock);

        let lua = mlua::Lua::new();
        let value = must_resolve(&resolver, &lua, "lib.helper");
        assert_eq!(get_field::<String>(&value, "name"), "mocked");
    }

    #[test]
    fn fs_resolver_init_lua_fallback() {
        let mock = MockSandbox::new().with_file("mypkg/init.lua", "return { from_init = true }");
        let resolver = FsResolver::with_sandbox(mock);

        let lua = mlua::Lua::new();
        let value = must_resolve(&resolver, &lua, "mypkg");
        assert!(get_field::<bool>(&value, "from_init"));
    }

    #[test]
    fn fs_resolver_miss_returns_none() {
        let mock = MockSandbox::new();
        let resolver = FsResolver::with_sandbox(mock);

        let lua = mlua::Lua::new();
        assert!(resolver.resolve(&lua, "nonexistent").is_none());
    }

    #[test]
    fn fs_resolver_custom_extension() {
        let mock = MockSandbox::new().with_file("lib/helper.luau", "return { name = 'luau_mod' }");
        let resolver = FsResolver::with_sandbox(mock).with_extension("luau");

        let lua = mlua::Lua::new();
        let value = must_resolve(&resolver, &lua, "lib.helper");
        assert_eq!(get_field::<String>(&value, "name"), "luau_mod");
    }

    #[test]
    fn fs_resolver_custom_init_name() {
        let mock = MockSandbox::new().with_file("mypkg/mod.lua", "return { from_mod = true }");
        let resolver = FsResolver::with_sandbox(mock).with_init_name("mod");

        let lua = mlua::Lua::new();
        let value = must_resolve(&resolver, &lua, "mypkg");
        assert!(get_field::<bool>(&value, "from_mod"));
    }

    #[test]
    fn fs_resolver_custom_extension_ignores_default() {
        // .lua is not resolved when .luau is configured
        let mock = MockSandbox::new().with_file("helper.lua", "return 'wrong'");
        let resolver = FsResolver::with_sandbox(mock).with_extension("luau");

        let lua = mlua::Lua::new();
        assert!(resolver.resolve(&lua, "helper").is_none());
    }

    #[test]
    fn fs_resolver_with_convention_luau() {
        let mock = MockSandbox::new()
            .with_file("lib/helper.luau", "return { name = 'luau' }")
            .with_file("pkg/init.luau", "return { pkg = true }");
        let resolver = FsResolver::with_sandbox(mock).with_convention(crate::LuaConvention::LUAU);

        let lua = mlua::Lua::new();

        let value = must_resolve(&resolver, &lua, "lib.helper");
        assert_eq!(get_field::<String>(&value, "name"), "luau");

        let value = must_resolve(&resolver, &lua, "pkg");
        assert!(get_field::<bool>(&value, "pkg"));
    }

    #[test]
    fn convention_then_override() {
        // Partial override via individual method after with_convention
        let mock = MockSandbox::new().with_file("pkg/mod.luau", "return { ok = true }");
        let resolver = FsResolver::with_sandbox(mock)
            .with_convention(crate::LuaConvention::LUAU)
            .with_init_name("mod");

        let lua = mlua::Lua::new();
        let value = must_resolve(&resolver, &lua, "pkg");
        assert!(get_field::<bool>(&value, "ok"));
    }

    #[test]
    fn lua_convention_default_is_lua54() {
        assert_eq!(crate::LuaConvention::default(), crate::LuaConvention::LUA54);
    }

    #[test]
    fn asset_resolver_json_to_table() {
        let mock = MockSandbox::new().with_file("config.json", r#"{"port": 8080}"#);
        let resolver = AssetResolver::with_sandbox(mock).parser("json", json_parser());

        let lua = mlua::Lua::new();
        let value = must_resolve(&resolver, &lua, "config.json");
        assert_eq!(get_field::<i32>(&value, "port"), 8080);
    }

    #[test]
    fn asset_resolver_text_to_string() {
        let mock = MockSandbox::new().with_file("query.sql", "SELECT 1");
        let resolver = AssetResolver::with_sandbox(mock).parser("sql", text_parser());

        let lua = mlua::Lua::new();
        let value = must_resolve(&resolver, &lua, "query.sql");
        let s: String = lua.unpack(value).expect("unpack String failed");
        assert_eq!(s, "SELECT 1");
    }

    #[test]
    fn asset_resolver_unregistered_ext_returns_none() {
        let mock = MockSandbox::new().with_file("data.xyz", "stuff");
        let resolver = AssetResolver::with_sandbox(mock).parser("json", json_parser());

        let lua = mlua::Lua::new();
        assert!(resolver.resolve(&lua, "data.xyz").is_none());
    }

    #[test]
    fn asset_resolver_no_ext_returns_none() {
        let mock = MockSandbox::new();
        let resolver = AssetResolver::with_sandbox(mock);

        let lua = mlua::Lua::new();
        assert!(resolver.resolve(&lua, "noext").is_none());
    }

    #[test]
    fn asset_resolver_custom_parser() {
        let mock = MockSandbox::new().with_file("data.csv", "a,b,c");
        let resolver = AssetResolver::with_sandbox(mock).parser("csv", |lua, content| {
            let t = lua.create_table()?;
            for (i, field) in content.split(',').enumerate() {
                t.set(i + 1, lua.create_string(field)?)?;
            }
            Ok(Value::Table(t))
        });

        let lua = mlua::Lua::new();
        let value = must_resolve(&resolver, &lua, "data.csv");
        assert_eq!(get_field::<String>(&value, 1), "a");
    }

    // -- I/O error propagation tests --

    /// Mock sandbox that returns I/O errors for all reads.
    struct IoErrorSandbox {
        kind: std::io::ErrorKind,
    }

    impl SandboxedFs for IoErrorSandbox {
        fn read(&self, relative: &Path) -> std::result::Result<Option<FileContent>, ReadError> {
            Err(ReadError::Io {
                path: relative.to_path_buf(),
                source: std::io::Error::new(self.kind, "mock I/O error"),
            })
        }
    }

    #[test]
    fn fs_resolver_propagates_io_error() {
        let resolver = FsResolver::with_sandbox(IoErrorSandbox {
            kind: std::io::ErrorKind::PermissionDenied,
        });

        let lua = mlua::Lua::new();
        let msg = must_resolve_err(&resolver, &lua, "anything");
        assert!(
            msg.contains("I/O error"),
            "expected ResolveError::Io message: {msg}"
        );
    }

    #[test]
    fn asset_resolver_propagates_io_error() {
        let resolver = AssetResolver::with_sandbox(IoErrorSandbox {
            kind: std::io::ErrorKind::PermissionDenied,
        })
        .parser("json", json_parser());

        let lua = mlua::Lua::new();
        let msg = must_resolve_err(&resolver, &lua, "data.json");
        assert!(
            msg.contains("I/O error"),
            "expected ResolveError::Io message: {msg}"
        );
    }

    // -- PrefixResolver tests --

    #[test]
    fn prefix_strips_and_delegates() {
        let inner = MemoryResolver::new().add("helper", "return 'from helper'");
        let resolver = PrefixResolver::new("sm", inner);

        let lua = mlua::Lua::new();
        let value = must_resolve(&resolver, &lua, "sm.helper");
        let s: String = lua.unpack(value).expect("unpack String failed");
        assert_eq!(s, "from helper");
    }

    #[test]
    fn prefix_non_matching_returns_none() {
        let inner = MemoryResolver::new().add("helper", "return 'x'");
        let resolver = PrefixResolver::new("sm", inner);

        let lua = mlua::Lua::new();
        assert!(resolver.resolve(&lua, "other.helper").is_none());
    }

    #[test]
    fn prefix_exact_match_without_separator_returns_none() {
        let inner = MemoryResolver::new().add("helper", "return 'x'");
        let resolver = PrefixResolver::new("sm", inner);

        let lua = mlua::Lua::new();
        // "sm" alone is outside PrefixResolver's scope (handled by outer Resolver)
        assert!(resolver.resolve(&lua, "sm").is_none());
    }

    #[test]
    fn prefix_no_substring_match() {
        let inner = MemoryResolver::new().add("tp", "return 'x'");
        let resolver = PrefixResolver::new("sm", inner);

        let lua = mlua::Lua::new();
        // "smtp" is not "sm" + "." + "tp"
        assert!(resolver.resolve(&lua, "smtp").is_none());
    }

    #[test]
    fn prefix_custom_separator() {
        let inner = MemoryResolver::new().add("http", "return 'http mod'");
        let resolver = PrefixResolver::new("@std", inner).with_separator('/');

        let lua = mlua::Lua::new();
        let value = must_resolve(&resolver, &lua, "@std/http");
        let s: String = lua.unpack(value).expect("unpack String failed");
        assert_eq!(s, "http mod");
    }

    #[test]
    fn prefix_nested_name() {
        let mock = MockSandbox::new().with_file("ui/button.lua", "return { name = 'button' }");
        let resolver = PrefixResolver::new("game", FsResolver::with_sandbox(mock));

        let lua = mlua::Lua::new();
        // "game.ui.button" -> strip "game." -> "ui.button" -> FsResolver: ui/button.lua
        let value = must_resolve(&resolver, &lua, "game.ui.button");
        assert_eq!(get_field::<String>(&value, "name"), "button");
    }

    #[test]
    fn prefix_inner_miss_returns_none() {
        let inner = MemoryResolver::new().add("helper", "return 'x'");
        let resolver = PrefixResolver::new("sm", inner);

        let lua = mlua::Lua::new();
        // "sm.nonexistent" -> strip -> "nonexistent" -> inner returns None -> None
        assert!(resolver.resolve(&lua, "sm.nonexistent").is_none());
    }
}
