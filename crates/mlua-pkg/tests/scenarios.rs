use mlua::{Lua, Result, Value};
use mlua_pkg::Registry;
use mlua_pkg::resolvers::*;
#[cfg(feature = "sandbox-cap-std")]
use mlua_pkg::sandbox::CapSandbox;
use mlua_pkg::sandbox::InitError;
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
