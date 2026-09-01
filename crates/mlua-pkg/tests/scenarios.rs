use mlua::{Lua, Result, Value};
use mlua_pkg::resolvers::*;
#[cfg(feature = "sandbox-cap-std")]
use mlua_pkg::sandbox::CapSandbox;
use mlua_pkg::sandbox::{InitError, SymlinkAwareSandbox};
use mlua_pkg::{Registry, ResolveError, Resolver};
use std::io::Write;

// -- 1. preload: require in-memory Lua sources --

#[test]
fn preload_module() {
    let lua = Lua::new();
    let mut reg = Registry::new();
    reg.add(MemoryResolver::new().add("mylib", "return { version = 42 }"));
    reg.install(&lua).unwrap();

    let v: i32 = lua
        .load(r#"return require("mylib").version"#)
        .eval()
        .unwrap();
    assert_eq!(v, 42);
}

// -- 2. embedded: include_str! equivalent embedded modules --

#[test]
fn embedded_framework_modules() {
    let lua = Lua::new();
    let mut reg = Registry::new();

    reg.add(
        MemoryResolver::new()
            .add(
                "framework",
                r#"
                local cli = require("framework.cli")
                return { cli = cli }
            "#,
            )
            .add(
                "framework.cli",
                "return { parse = function() return 'parsed' end }",
            ),
    );
    reg.install(&lua).unwrap();

    let v: String = lua
        .load(r#"return require("framework").cli.parse()"#)
        .eval()
        .unwrap();
    assert_eq!(v, "parsed");
}

// -- 3. native: build tables from Rust functions --

#[test]
fn native_rust_module() {
    let lua = Lua::new();
    let mut reg = Registry::new();
    reg.add(NativeResolver::new().add("@std/http", |lua| {
        let t = lua.create_table()?;
        let get =
            lua.create_function(|lua, url: String| lua.create_string(format!("GET {url}")))?;
        t.set("get", get)?;
        Ok(Value::Table(t))
    }));
    reg.install(&lua).unwrap();

    let v: String = lua
        .load(
            r#"
            local http = require("@std/http")
            return http.get("https://example.com")
        "#,
        )
        .eval()
        .unwrap();
    assert_eq!(v, "GET https://example.com");
}

// -- 4. filesystem: sandboxed FS + init.lua --

#[test]
fn fs_sandbox_and_init_lua() {
    let dir = tempfile::tempdir().unwrap();

    let lib_dir = dir.path().join("lib");
    std::fs::create_dir_all(&lib_dir).unwrap();
    std::fs::write(lib_dir.join("helper.lua"), "return { name = 'helper' }").unwrap();

    let pkg_dir = dir.path().join("mypkg");
    std::fs::create_dir_all(&pkg_dir).unwrap();
    std::fs::write(pkg_dir.join("init.lua"), "return { name = 'mypkg' }").unwrap();

    let lua = Lua::new();
    let mut reg = Registry::new();
    reg.add(FsResolver::new(dir.path()).unwrap());
    reg.install(&lua).unwrap();

    let v: String = lua
        .load(r#"return require("lib.helper").name"#)
        .eval()
        .unwrap();
    assert_eq!(v, "helper");

    let v: String = lua.load(r#"return require("mypkg").name"#).eval().unwrap();
    assert_eq!(v, "mypkg");
}

// -- 5. sandbox: path traversal blocking (FsResolver) --

#[test]
fn fs_blocks_traversal() {
    let dir = tempfile::tempdir().unwrap();

    let outside = dir.path().join("outside.lua");
    std::fs::write(&outside, "return 'escaped'").unwrap();

    let sandbox = dir.path().join("sandbox");
    std::fs::create_dir_all(&sandbox).unwrap();

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, sandbox.join("escape.lua")).unwrap();
    }

    let lua = Lua::new();
    let mut reg = Registry::new();
    reg.add(FsResolver::new(&sandbox).unwrap());
    reg.install(&lua).unwrap();

    let result: Result<Value> = lua.load(r#"return require("..outside")"#).eval();
    assert!(result.is_err());
}

// -- 6. asset: JSON -> Lua Table --

#[test]
fn json_asset_to_table() {
    let dir = tempfile::tempdir().unwrap();
    let mut f = std::fs::File::create(dir.path().join("config.json")).unwrap();
    write!(f, r#"{{"port": 8080, "host": "localhost"}}"#).unwrap();

    let lua = Lua::new();
    let mut reg = Registry::new();
    reg.add(
        AssetResolver::new(dir.path())
            .unwrap()
            .parser("json", json_parser()),
    );
    reg.install(&lua).unwrap();

    let port: i32 = lua
        .load(r#"return require("config.json").port"#)
        .eval()
        .unwrap();
    assert_eq!(port, 8080);

    let host: String = lua
        .load(r#"return require("config.json").host"#)
        .eval()
        .unwrap();
    assert_eq!(host, "localhost");
}

// -- 7. asset: text -> Lua String --

#[test]
fn text_asset_to_string() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("query.sql"), "SELECT * FROM users").unwrap();

    let lua = Lua::new();
    let mut reg = Registry::new();
    reg.add(
        AssetResolver::new(dir.path())
            .unwrap()
            .parser("sql", text_parser()),
    );
    reg.install(&lua).unwrap();

    let sql: String = lua.load(r#"return require("query.sql")"#).eval().unwrap();
    assert_eq!(sql, "SELECT * FROM users");
}

// -- 8. priority: first match wins --

#[test]
fn first_resolver_wins() {
    let lua = Lua::new();
    let mut reg = Registry::new();
    reg.add(MemoryResolver::new().add("config", "return 'from memory'"));
    reg.add(NativeResolver::new().add("config", |lua| {
        lua.create_string("from native").map(Value::String)
    }));
    reg.install(&lua).unwrap();

    let v: String = lua.load(r#"return require("config")"#).eval().unwrap();
    assert_eq!(v, "from memory");
}

// -- 9. cache: second require hits Lua's package.loaded --

#[test]
fn lua_caches_in_package_loaded() {
    let lua = Lua::new();
    let mut reg = Registry::new();
    reg.add(MemoryResolver::new().add(
        "counter",
        r#"
        _G.__counter = (_G.__counter or 0) + 1
        return { count = _G.__counter }
    "#,
    ));
    reg.install(&lua).unwrap();

    let v: i32 = lua
        .load(
            r#"
            local a = require("counter").count
            local b = require("counter").count
            return a + b  -- 1 + 1 = 2 (cached, not re-evaluated)
        "#,
        )
        .eval()
        .unwrap();
    assert_eq!(v, 2);
}

// -- 10. composite: full-stack configuration --

#[test]
fn full_stack_orcs_like() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("user_plugin.lua"),
        r#"
            local http = require("@std/http")
            local config = require("app.config")
            return {
                run = function()
                    return http.get(config.endpoint)
                end
            }
        "#,
    )
    .unwrap();

    let lua = Lua::new();
    let mut reg = Registry::new();

    reg.add(NativeResolver::new().add("@std/http", |lua| {
        let t = lua.create_table()?;
        t.set(
            "get",
            lua.create_function(|lua, url: String| lua.create_string(format!("GET {url}")))?,
        )?;
        Ok(Value::Table(t))
    }));

    reg.add(MemoryResolver::new().add(
        "app.config",
        r#"return { endpoint = "https://api.example.com" }"#,
    ));

    reg.add(FsResolver::new(dir.path()).unwrap());

    reg.install(&lua).unwrap();

    let v: String = lua
        .load(r#"return require("user_plugin").run()"#)
        .eval()
        .unwrap();
    assert_eq!(v, "GET https://api.example.com");
}

// -- 11. AssetResolver: path traversal blocking --

#[test]
fn asset_blocks_traversal() {
    let dir = tempfile::tempdir().unwrap();

    let outside = dir.path().join("secret.json");
    std::fs::write(&outside, r#"{"secret": true}"#).unwrap();

    let sandbox = dir.path().join("assets");
    std::fs::create_dir_all(&sandbox).unwrap();

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, sandbox.join("escape.json")).unwrap();
    }

    let lua = Lua::new();
    let mut reg = Registry::new();
    reg.add(
        AssetResolver::new(&sandbox)
            .unwrap()
            .parser("json", json_parser()),
    );
    reg.install(&lua).unwrap();

    let result: Result<Value> = lua.load(r#"return require("escape.json")"#).eval();
    assert!(result.is_err());
}

// -- 12. fail-fast: immediate error on nonexistent root --

#[test]
fn fs_resolver_rejects_nonexistent_root() {
    let result = FsResolver::new("/nonexistent/path/that/does/not/exist");
    let Err(err) = result else {
        panic!("expected RootNotFound error");
    };
    assert!(
        matches!(err, InitError::RootNotFound { .. }),
        "expected RootNotFound, got: {err}"
    );
}

#[test]
fn asset_resolver_rejects_nonexistent_root() {
    let result = AssetResolver::new("/nonexistent/path/that/does/not/exist");
    let Err(err) = result else {
        panic!("expected RootNotFound error");
    };
    assert!(
        matches!(err, InitError::RootNotFound { .. }),
        "expected RootNotFound, got: {err}"
    );
}

// -- 13. JSON parse error propagates structurally --

#[test]
fn asset_json_parse_error_is_structured() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("broken.json"), "{ invalid json }").unwrap();

    let lua = Lua::new();
    let mut reg = Registry::new();
    reg.add(
        AssetResolver::new(dir.path())
            .unwrap()
            .parser("json", json_parser()),
    );
    reg.install(&lua).unwrap();

    let result: Result<Value> = lua.load(r#"return require("broken.json")"#).eval();
    let err = result.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("asset parse error"), "got: {msg}");
}

// -- 14. PrefixResolver: namespace mounting --

#[test]
fn prefix_mounts_directory_namespace() {
    let dir = tempfile::tempdir().unwrap();

    // sm/ directory layout
    let sm_dir = dir.path().join("sm");
    std::fs::create_dir_all(&sm_dir).unwrap();
    std::fs::write(sm_dir.join("helper.lua"), "return { name = 'helper' }").unwrap();
    std::fs::write(
        sm_dir.join("engine.lua"),
        r#"
        local helper = require("sm.helper")
        return { engine = true, helper_name = helper.name }
    "#,
    )
    .unwrap();

    // sm/init.lua (for top-level require("sm"))
    std::fs::write(
        sm_dir.join("init.lua"),
        r#"
        local helper = require("sm.helper")
        return { init = true, helper_name = helper.name }
    "#,
    )
    .unwrap();

    let lua = Lua::new();
    let mut reg = Registry::new();

    // PrefixResolver: sm.xxx -> sm/xxx.lua
    reg.add(PrefixResolver::new("sm", FsResolver::new(&sm_dir).unwrap()));
    // FsResolver: sm -> sm/init.lua (init.lua fallback)
    reg.add(FsResolver::new(dir.path()).unwrap());

    reg.install(&lua).unwrap();

    // sm.helper -> PrefixResolver -> sm/helper.lua
    let name: String = lua
        .load(r#"return require("sm.helper").name"#)
        .eval()
        .unwrap();
    assert_eq!(name, "helper");

    // sm.engine -> PrefixResolver -> sm/engine.lua (internally requires sm.helper)
    let helper_name: String = lua
        .load(r#"return require("sm.engine").helper_name"#)
        .eval()
        .unwrap();
    assert_eq!(helper_name, "helper");

    // sm -> FsResolver -> sm/init.lua
    let init: bool = lua.load(r#"return require("sm").init"#).eval().unwrap();
    assert!(init);
}

// -- 15. PrefixResolver + NativeResolver: composite configuration --

#[test]
fn prefix_with_native_and_fs() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("plugin.lua"),
        r#"
        local http = require("@std/http")
        local cfg = require("app.config")
        return { url = http.base .. cfg.path }
    "#,
    )
    .unwrap();

    let app_dir = dir.path().join("app");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::write(app_dir.join("config.lua"), r#"return { path = "/api/v1" }"#).unwrap();

    let lua = Lua::new();
    let mut reg = Registry::new();

    // @std/* -> NativeResolver (no PrefixResolver needed; NativeResolver matches full name)
    reg.add(NativeResolver::new().add("@std/http", |lua| {
        let t = lua.create_table()?;
        t.set("base", "https://example.com")?;
        Ok(Value::Table(t))
    }));

    // app.xxx -> app/xxx.lua
    reg.add(PrefixResolver::new(
        "app",
        FsResolver::new(&app_dir).unwrap(),
    ));

    // plugin -> dir/plugin.lua
    reg.add(FsResolver::new(dir.path()).unwrap());

    reg.install(&lua).unwrap();

    let url: String = lua.load(r#"return require("plugin").url"#).eval().unwrap();
    assert_eq!(url, "https://example.com/api/v1");
}

// -- 16. CapSandbox: capability-based sandboxed read --

#[cfg(feature = "sandbox-cap-std")]
#[test]
fn cap_sandbox_reads_file() {
    let dir = tempfile::tempdir().unwrap();

    let lib_dir = dir.path().join("lib");
    std::fs::create_dir_all(&lib_dir).unwrap();
    std::fs::write(lib_dir.join("helper.lua"), "return { name = 'cap' }").unwrap();

    let pkg_dir = dir.path().join("mypkg");
    std::fs::create_dir_all(&pkg_dir).unwrap();
    std::fs::write(pkg_dir.join("init.lua"), "return { name = 'cap-init' }").unwrap();

    let lua = Lua::new();
    let mut reg = Registry::new();
    reg.add(FsResolver::with_sandbox(
        CapSandbox::new(dir.path()).unwrap(),
    ));
    reg.install(&lua).unwrap();

    let v: String = lua
        .load(r#"return require("lib.helper").name"#)
        .eval()
        .unwrap();
    assert_eq!(v, "cap");

    let v: String = lua.load(r#"return require("mypkg").name"#).eval().unwrap();
    assert_eq!(v, "cap-init");
}

// -- 17. CapSandbox: file not found returns None (falls through) --

#[cfg(feature = "sandbox-cap-std")]
#[test]
fn cap_sandbox_miss_falls_through() {
    let dir = tempfile::tempdir().unwrap();

    let lua = Lua::new();
    let mut reg = Registry::new();
    reg.add(FsResolver::with_sandbox(
        CapSandbox::new(dir.path()).unwrap(),
    ));
    reg.add(MemoryResolver::new().add("fallback", "return 'from memory'"));
    reg.install(&lua).unwrap();

    let v: String = lua.load(r#"return require("fallback")"#).eval().unwrap();
    assert_eq!(v, "from memory");
}

// -- 18. CapSandbox: path traversal blocked by OS --

#[cfg(all(feature = "sandbox-cap-std", unix))]
#[test]
fn cap_sandbox_blocks_traversal() {
    let dir = tempfile::tempdir().unwrap();

    let outside = dir.path().join("secret.lua");
    std::fs::write(&outside, "return 'escaped'").unwrap();

    let sandbox = dir.path().join("sandbox");
    std::fs::create_dir_all(&sandbox).unwrap();

    // Symlink pointing outside the sandbox
    std::os::unix::fs::symlink(&outside, sandbox.join("escape.lua")).unwrap();

    let lua = Lua::new();
    let mut reg = Registry::new();
    reg.add(FsResolver::with_sandbox(CapSandbox::new(&sandbox).unwrap()));
    reg.install(&lua).unwrap();

    // Symlink escape should be blocked by cap-std
    let result: Result<Value> = lua.load(r#"return require("escape")"#).eval();
    assert!(result.is_err());
}

// -- 19. CapSandbox: rejects nonexistent root --

#[cfg(feature = "sandbox-cap-std")]
#[test]
fn cap_sandbox_rejects_nonexistent_root() {
    let result = CapSandbox::new("/nonexistent/path/that/does/not/exist");
    let Err(err) = result else {
        panic!("expected RootNotFound error");
    };
    assert!(
        matches!(err, InitError::RootNotFound { .. }),
        "expected RootNotFound, got: {err}"
    );
}

// -- 20. CapSandbox: AssetResolver integration --

#[cfg(feature = "sandbox-cap-std")]
#[test]
fn cap_sandbox_asset_json() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.json"), r#"{"key": "value"}"#).unwrap();

    let lua = Lua::new();
    let mut reg = Registry::new();
    reg.add(
        AssetResolver::with_sandbox(CapSandbox::new(dir.path()).unwrap())
            .parser("json", json_parser()),
    );
    reg.install(&lua).unwrap();

    let v: String = lua
        .load(r#"return require("data.json").key"#)
        .eval()
        .unwrap();
    assert_eq!(v, "value");
}

// -- 21. SymlinkAwareSandbox: follows symlinks in root --

#[cfg(unix)]
#[test]
fn symlink_aware_sandbox_follows_root_symlinks() {
    let dir = tempfile::tempdir().unwrap();

    // Package source that lives outside the sandbox root
    let external = dir.path().join("external_pkg");
    std::fs::create_dir_all(&external).unwrap();
    std::fs::write(external.join("init.lua"), "return { linked = true }").unwrap();

    // Sandbox root, as a linking package manager would lay it out
    let sandbox = dir.path().join("sandbox");
    std::fs::create_dir_all(&sandbox).unwrap();

    // Symlink: sandbox/my_pkg -> external_pkg (like alc_pkg_link)
    std::os::unix::fs::symlink(&external, sandbox.join("my_pkg")).unwrap();

    let lua = Lua::new();
    let mut reg = Registry::new();
    reg.add(FsResolver::with_sandbox(
        SymlinkAwareSandbox::new(&sandbox).unwrap(),
    ));
    reg.install(&lua).unwrap();

    let v: bool = lua
        .load(r#"return require("my_pkg").linked"#)
        .eval()
        .unwrap();
    assert!(v);
}

// -- 22. SymlinkAwareSandbox: blocks traversal outside symlink targets --

#[cfg(unix)]
#[test]
fn symlink_aware_sandbox_blocks_non_target_traversal() {
    let dir = tempfile::tempdir().unwrap();

    // Secret file outside sandbox, NOT a symlink target
    let secret = dir.path().join("secret.lua");
    std::fs::write(&secret, "return 'escaped'").unwrap();

    let sandbox = dir.path().join("sandbox");
    std::fs::create_dir_all(&sandbox).unwrap();

    // Symlink directly to a file outside (not a directory in root)
    std::os::unix::fs::symlink(&secret, sandbox.join("escape.lua")).unwrap();

    let lua = Lua::new();
    let mut reg = Registry::new();
    reg.add(FsResolver::with_sandbox(
        SymlinkAwareSandbox::new(&sandbox).unwrap(),
    ));
    reg.install(&lua).unwrap();

    // ../secret should be blocked
    let result: Result<Value> = lua.load(r#"return require("..secret")"#).eval();
    assert!(result.is_err());
}

// -- 23. SymlinkAwareSandbox: non-symlink files work normally --

#[cfg(unix)]
#[test]
fn symlink_aware_sandbox_normal_files() {
    let dir = tempfile::tempdir().unwrap();
    let sandbox = dir.path().join("sandbox");
    std::fs::create_dir_all(sandbox.join("real_pkg")).unwrap();
    std::fs::write(
        sandbox.join("real_pkg").join("init.lua"),
        "return { real = true }",
    )
    .unwrap();

    let lua = Lua::new();
    let mut reg = Registry::new();
    reg.add(FsResolver::with_sandbox(
        SymlinkAwareSandbox::new(&sandbox).unwrap(),
    ));
    reg.install(&lua).unwrap();

    let v: bool = lua
        .load(r#"return require("real_pkg").real"#)
        .eval()
        .unwrap();
    assert!(v);
}

// -- 24. SymlinkAwareSandbox: submodules within symlinked package --

#[cfg(unix)]
#[test]
fn symlink_aware_sandbox_submodule_in_linked_pkg() {
    let dir = tempfile::tempdir().unwrap();

    let external = dir.path().join("ext_pkg");
    std::fs::create_dir_all(external.join("sub")).unwrap();
    std::fs::write(external.join("init.lua"), "return { root = true }").unwrap();
    std::fs::write(
        external.join("sub").join("init.lua"),
        "return { sub = true }",
    )
    .unwrap();

    let sandbox = dir.path().join("sandbox");
    std::fs::create_dir_all(&sandbox).unwrap();
    std::os::unix::fs::symlink(&external, sandbox.join("ext_pkg")).unwrap();

    let lua = Lua::new();
    let mut reg = Registry::new();
    reg.add(FsResolver::with_sandbox(
        SymlinkAwareSandbox::new(&sandbox).unwrap(),
    ));
    reg.install(&lua).unwrap();

    let v: bool = lua
        .load(r#"return require("ext_pkg.sub").sub"#)
        .eval()
        .unwrap();
    assert!(v);
}

// -- 25. new_symlink_aware: constructor resolves a linked package --

#[cfg(unix)]
#[test]
fn new_symlink_aware_resolves_linked_package() {
    let dir = tempfile::tempdir().unwrap();
    let external = dir.path().join("external_pkg");
    std::fs::create_dir_all(&external).unwrap();
    std::fs::write(external.join("init.lua"), "return { linked = true }").unwrap();

    let root = dir.path().join("vendored");
    std::fs::create_dir_all(&root).unwrap();
    std::os::unix::fs::symlink(&external, root.join("my_pkg")).unwrap();

    let lua = Lua::new();
    let mut reg = Registry::new();
    reg.add(FsResolver::new_symlink_aware(&root).unwrap());
    reg.install(&lua).unwrap();

    let v: bool = lua
        .load(r#"return require("my_pkg").linked"#)
        .eval()
        .unwrap();
    assert!(v);
}

// -- 26. default FsSandbox: traversal shadows the chain for that name only --

#[cfg(unix)]
#[test]
fn traversal_shadows_only_the_rejected_name() {
    let dir = tempfile::tempdir().unwrap();
    let external = dir.path().join("external_pkg");
    std::fs::create_dir_all(&external).unwrap();
    std::fs::write(external.join("init.lua"), "return { from = 'symlink' }").unwrap();

    let root = dir.path().join("vendored");
    std::fs::create_dir_all(&root).unwrap();
    std::os::unix::fs::symlink(&external, root.join("my_pkg")).unwrap();

    let lua = Lua::new();

    // The default constructor picks the strict FsSandbox, which classifies the
    // symlink target as an escape: Some(Err), not None.
    let strict = FsResolver::new(&root).unwrap();
    let err = strict
        .resolve(&lua, "my_pkg")
        .expect("resolver must claim the name")
        .unwrap_err();
    assert!(
        matches!(
            err.downcast_ref::<ResolveError>(),
            Some(ResolveError::PathTraversal { .. })
        ),
        "expected PathTraversal, got {err:?}"
    );

    let mut reg = Registry::new();
    reg.add(FsResolver::new(&root).unwrap());
    reg.add(
        MemoryResolver::new()
            .add("my_pkg", "return { from = 'memory' }")
            .add("other", "return { from = 'memory' }"),
    );
    reg.install(&lua).unwrap();

    // The rejected name does not fall through to MemoryResolver.
    let blocked: Result<String> = lua.load(r#"return require("my_pkg").from"#).eval();
    assert!(blocked.is_err());

    // Other names in the same chain are unaffected.
    let ok: String = lua.load(r#"return require("other").from"#).eval().unwrap();
    assert_eq!(ok, "memory");
}

// -- 27. SymlinkAwareSandbox: symlink created after construction is picked up --

#[cfg(unix)]
#[test]
fn symlink_aware_sandbox_rescans_for_late_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("vendored");
    std::fs::create_dir_all(&root).unwrap();

    // Sandbox built while the root is still empty.
    let resolver = FsResolver::new_symlink_aware(&root).unwrap();

    // Package linked in afterwards (a later `mlua-pkg install`).
    let external = dir.path().join("late_pkg");
    std::fs::create_dir_all(&external).unwrap();
    std::fs::write(external.join("init.lua"), "return { late = true }").unwrap();
    std::os::unix::fs::symlink(&external, root.join("late")).unwrap();

    let lua = Lua::new();
    let mut reg = Registry::new();
    reg.add(resolver);
    reg.install(&lua).unwrap();

    let v: bool = lua.load(r#"return require("late").late"#).eval().unwrap();
    assert!(v);
}

// -- 28. SymlinkAwareSandbox: rescan does not widen to unlinked outside paths --

#[cfg(unix)]
#[test]
fn symlink_aware_sandbox_rescan_still_blocks_escapes() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("secret.lua");
    std::fs::write(&secret, "return 'escaped'").unwrap();

    let root = dir.path().join("vendored");
    std::fs::create_dir_all(&root).unwrap();

    // '/' as the separator so a `..` component survives into the candidate
    // path. With the default '.' separator, "..secret" becomes "//secret",
    // which `Path::join` treats as absolute — the read then misses at
    // `canonicalize` and never reaches the boundary check at all.
    let resolver = FsResolver::new_symlink_aware(&root)
        .unwrap()
        .with_module_separator('/');

    // A legitimate link appears later; it must not authorize unrelated paths.
    let external = dir.path().join("late_pkg");
    std::fs::create_dir_all(&external).unwrap();
    std::fs::write(external.join("init.lua"), "return { late = true }").unwrap();
    std::os::unix::fs::symlink(&external, root.join("late")).unwrap();

    let lua = Lua::new();

    // Prime the rescan with the legitimate name.
    let ok = resolver.resolve(&lua, "late").expect("late must resolve");
    assert!(ok.is_ok(), "late failed: {:?}", ok.unwrap_err());

    // The sibling secret exists and canonicalizes outside every allowed root,
    // so it must be rejected as a traversal — not merely "not found".
    let err = resolver
        .resolve(&lua, "../secret")
        .expect("resolver must claim the escaping name")
        .unwrap_err();
    assert!(
        matches!(
            err.downcast_ref::<ResolveError>(),
            Some(ResolveError::PathTraversal { .. })
        ),
        "expected PathTraversal, got {err:?}"
    );
}

// -- 29. AssetResolver::new_symlink_aware: assets through a linked directory --

#[cfg(unix)]
#[test]
fn asset_resolver_new_symlink_aware() {
    let dir = tempfile::tempdir().unwrap();
    let external = dir.path().join("external_assets");
    std::fs::create_dir_all(&external).unwrap();
    std::fs::write(external.join("config.json"), r#"{"key":"value"}"#).unwrap();

    let root = dir.path().join("assets");
    std::fs::create_dir_all(&root).unwrap();
    std::os::unix::fs::symlink(&external, root.join("linked")).unwrap();

    let lua = Lua::new();
    let mut reg = Registry::new();
    reg.add(
        AssetResolver::new_symlink_aware(&root)
            .unwrap()
            .parser("json", json_parser()),
    );
    reg.install(&lua).unwrap();

    let v: String = lua
        .load(r#"return require("linked/config.json").key"#)
        .eval()
        .unwrap();
    assert_eq!(v, "value");
}
