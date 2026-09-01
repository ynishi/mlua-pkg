# mlua-pkg

Composable Lua module loader for [mlua](https://github.com/mlua-rs/mlua).

Turns `require("name")` into a composable `name -> Value` resolution chain,
unifying in-memory sources, filesystem, Rust-native modules, and non-Lua assets
under a single `Resolver` trait.

## Quick start

```rust
use mlua::Lua;
use mlua_pkg::{Registry, resolvers::*};

fn setup(lua: &Lua) -> Result<(), Box<dyn std::error::Error>> {
    let mut reg = Registry::new();

    // Rust-native module (highest priority)
    reg.add(NativeResolver::new().add("@std/http", |lua| {
        let t = lua.create_table()?;
        t.set("version", 1)?;
        Ok(mlua::Value::Table(t))
    }));

    // Embedded Lua source
    reg.add(MemoryResolver::new().add("utils", "return { pi = 3.14 }"));

    // Filesystem with sandbox (dot-separated -> path)
    reg.add(FsResolver::new("./scripts")?);

    // Non-Lua assets with pluggable parsers
    reg.add(AssetResolver::new("./assets")?
        .parser("json", json_parser())
        .parser("sql", text_parser()));

    reg.install(lua)?;
    Ok(())
}

// Lua side: require("@std/http"), require("utils"), etc.
```

## Resolver chain

Resolvers are tried in registration order. First `Some` wins.

```text
require("name")
  |
  v
package.searchers[1]  <- Registry hook
  |
  +- Resolver A: resolve(lua, "name") -> None (skip)
  +- Resolver B: resolve(lua, "name") -> Some(Ok(Value)) (done)
  |
  v
package.loaded["name"] = Value  <- Lua standard cache
```

| Return value    | Meaning                     | Next resolver? |
|-----------------|-----------------------------|----------------|
| `None`          | Not my responsibility       | Tried          |
| `Some(Ok(v))`   | Resolved                    | Skipped        |
| `Some(Err(e))`  | Responsible but failed      | **Skipped**    |

`Some(Err)` intentionally does not fall through.
"Found but broken" should not silently resolve to something else.

## Built-in resolvers

### Leaf resolvers

| Resolver         | Source               | Match condition            |
|------------------|----------------------|----------------------------|
| `MemoryResolver` | `HashMap<String, String>` | Name is registered    |
| `NativeResolver` | `Fn(&Lua) -> Result<Value>` | Name is registered  |
| `FsResolver`     | Filesystem (sandboxed)    | File exists            |
| `AssetResolver`  | Filesystem (sandboxed)    | Known extension + file exists |

### Combinators

| Combinator       | Behavior                                        |
|-------------------|-------------------------------------------------|
| `Registry` (chain) | Try resolvers in order, take first `Some`     |
| `PrefixResolver`  | Strip prefix, delegate to inner resolver        |

## Filesystem resolution

`FsResolver` converts dot-separated module names to paths:

```text
require("lib.helper") -> lib/helper.lua
require("mypkg")      -> mypkg.lua, then mypkg/init.lua
```

Configurable via `LuaConvention` or individual methods:

```rust
use mlua_pkg::{LuaConvention, resolvers::FsResolver};

// Luau convention
let r = FsResolver::new("./src")?
    .with_convention(LuaConvention::LUAU);

// Custom
let r = FsResolver::new("./src")?
    .with_extension("lua")
    .with_init_name("mod")
    .with_module_separator('/');
```

## Asset parsing

`AssetResolver` dispatches to registered parsers by file extension:

```rust
use mlua_pkg::resolvers::{AssetResolver, json_parser, text_parser};

let r = AssetResolver::new("./assets")?
    .parser("json", json_parser())   // JSON -> Lua Table
    .parser("sql", text_parser())    // raw text -> Lua String
    .parser("csv", |lua, content| {  // custom parser
        let t = lua.create_table()?;
        for (i, line) in content.lines().enumerate() {
            t.set(i + 1, lua.create_string(line)?)?;
        }
        Ok(mlua::Value::Table(t))
    });
```

## Namespace mounting

`PrefixResolver` creates mount points for module namespaces:

```rust
use mlua_pkg::{Registry, resolvers::*};

let mut reg = Registry::new();

// "game.engine" -> strip "game." -> FsResolver resolves "engine"
reg.add(PrefixResolver::new("game",
    FsResolver::new("./game_modules")?));

// "game" (init.lua) -> outer FsResolver
reg.add(FsResolver::new("./scripts")?);
```

## Sandbox

All filesystem access goes through the `SandboxedFs` trait.
Three implementations are provided:

| Implementation | Follows symlinks out of root | TOCTOU safe | Dependency |
|---------------|------------------------------|-------------|------------|
| `FsSandbox` (default) | No | No | None |
| `SymlinkAwareSandbox` | Yes, for symlinks directly under root | No | None |
| `CapSandbox` | No | Yes | `cap-std` (opt-in) |

`FsSandbox` canonicalizes paths and blocks traversal, but has a TOCTOU gap
between `canonicalize()` and `read_to_string()`.

`SymlinkAwareSandbox` treats the targets of symlinks directly under the root
as additional allowed roots. **Pick this whenever the root is populated by a
linking package manager** — `mlua-pkg install` symlinks each dep under
`<mlua-pkgs-dir>/vendored/<name>`, and `npm link` / `alc_pkg_link` behave the
same way. With the default `FsSandbox`, every `require` through such a
symlink fails with `ResolveError::PathTraversal`, and because that is a
`Some(Err)` it does not fall through to later resolvers for those names.
The allowed set is refreshed lazily, so packages linked in after the sandbox
was constructed still resolve.

```rust
use mlua_pkg::resolvers::FsResolver;

// root contains symlinks to external package sources
let resolver = FsResolver::new_symlink_aware("./blocks")?;
```

`VendoredResolver` already uses this backend, so the
`VendoredResolver::from_lockfile` path in [Use in Rust](#use-in-rust) needs no
extra wiring.

`CapSandbox` eliminates the TOCTOU gap via OS-level capability-based file
access (`openat2` / `RESOLVE_BENEATH` on Linux, equivalent on other platforms).

```toml
# Enable CapSandbox
mlua-pkg = { version = "0.1", features = ["sandbox-cap-std"] }
```

```rust
use mlua_pkg::{resolvers::FsResolver, sandbox::CapSandbox};

let resolver = FsResolver::with_sandbox(CapSandbox::new("./scripts")?);
```

For test mocking, implement `SandboxedFs` on your own type
and inject it via `FsResolver::with_sandbox()` / `AssetResolver::with_sandbox()`.

## Package manager (v0.4.0+)

`mlua-pkg` includes a built-in package manager for installing Lua packages from
git repositories.

### Manifest (`mlua-pkg.toml`)

```toml
[package]
name = "my-project"
version = "0.1.0"

[deps]
# Exact pin — never moves unless --force.
lshape = { git = "https://github.com/ynishi/lshape", tag = "v0.1.0" }

# Prefix pin — auto-follows the latest matching SemVer release
# (here: latest v0.1.x, not v0.2.x).  Manifest stays as "v0.1";
# the resolved tag is recorded in mlua-pkg.lock.
lshape = { git = "https://github.com/ynishi/lshape", tag = "v0.1" }

# Physical vendoring — copy the entry into a manifest-relative
# directory you can commit to git, instead of symlinking under
# .mlua-pkgs/vendored/.
lshape = { git = "https://github.com/ynishi/lshape", tag = "v0.2",
           entry = "lshape", target_dir = "lua/lshape" }
```

### Install via CLI

```sh
mlua-pkg install
```

Fetches every dep in `mlua-pkg.toml`, resolves prefix tag pins to a concrete
release, writes `mlua-pkg.lock` with pinned SHAs, and either symlinks each
dep under `<mlua-pkgs-dir>/vendored/<name>` (default) or copies the entry
contents into the per-dep `target_dir` when one is set.

#### Where the cache lives

`mlua-pkg` picks the base directory in this order:

1. `--mlua-pkgs-dir <PATH>` (global flag, applies to every subcommand).
2. `MLUA_PKG_DIR` env variable.
3. `target/mlua-pkgs/` when a `target/` directory exists in the cwd —
   typical for Rust crates that vendor Lua deps.  This keeps the cache out
   of the git tree (cargo unconditionally ignores `target/`) and prevents
   `cargo publish`'s VCS walker from descending into the nested git clones
   under `cache/` and aborting with "uncommitted changes".
4. `.mlua-pkgs/` fallback (legacy default) — add this entry to `.gitignore`
   if you keep it.

### Refresh deps (`mlua-pkg update`)

```sh
mlua-pkg update              # bump prefix pins, refresh branch pins
mlua-pkg update --dry-run    # print the plan, do not modify anything
mlua-pkg update --force      # also bump exact pins to the SemVer-max release
```

Per-pin behaviour:

| pin form          | `update` (no flag)                                       | `update --force`            |
| ----------------- | -------------------------------------------------------- | --------------------------- |
| `tag = "v1.0.0"`  | skip (use `--force` to bump)                             | bump to SemVer-max release  |
| `tag = "v1.0"`    | refresh — resolves to latest `v1.0.x`, manifest unchanged | (same)                     |
| `branch = "..."`  | refresh — re-install picks up new HEAD                   | (same)                     |
| `rev = "..."`     | skip                                                     | (same)                     |

### Use in Rust

```rust
use mlua::Lua;
use mlua_pkg::{resolvers::VendoredResolver, Registry};

fn setup(lua: &Lua) -> Result<(), Box<dyn std::error::Error>> {
    // Read the lockfile written by `mlua-pkg install`.
    let resolver = VendoredResolver::from_lockfile(
        "mlua-pkg.lock",
        ".mlua-pkgs/vendored",
    )?;

    let mut reg = Registry::new();
    reg.add(resolver);
    reg.install(lua)?;

    Ok(())
}

// Lua side:
// local lshape = require("lshape")
```

## License

Licensed under either of

- [MIT license](LICENSE-MIT)
- [Apache License, Version 2.0](LICENSE-APACHE)

at your option.
