//! Embedded-Lua plugin host (spec §6). Plugins register tasks and before/after
//! hooks at load time; at run time a hook fires with a `ctx`. The `ctx` carries
//! engine-routed effects (run, in_release, exec_in, docker, compose, cp_*,
//! read_file, write_file, file_exists, env — all dry-run-safe),
//! utilities (json/yaml encode+decode, log, warn, dump, inspect), and data: `cfg`,
//! `state` and a `vars` scratch space — all persistent tables shared across hooks.
//! `cfg` and `state` are live: the engine `refresh`es them from the typed config and
//! deploy state before each hook and reads any direct mutation back, so a plugin
//! assigning `ctx.cfg.x` / `ctx.state.x` changes the deploy itself. All effects go
//! through `ctx`, never raw os/io (sandboxed), so they stay observable.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use mlua::{Function, Lua, LuaSerdeExt, RegistryKey, Table, Value};
use serde::Serialize;

use crate::config::Config;
use crate::state::StageState;

/// What a Lua `ctx` can ask the host to do; implemented by the engine (deploy time)
/// and a lighter host (configure time).
pub trait HookHost {
    fn run_host(&self, cmd: &str) -> Result<String, String>;
    fn in_release(&self, cmd: &str) -> Result<String, String>;
    fn exec_in(&self, service: &str, cmd: &str) -> Result<String, String>;
    fn docker(&self, args: Vec<String>) -> Result<String, String>;
    fn compose(&self, args: Vec<String>) -> Result<String, String>;
    fn cp_from_release(&self, from: &str, to: &str) -> Result<(), String>;
    fn cp_to_release(&self, from: &str, to: &str) -> Result<(), String>;
    fn read_file(&self, path: &str) -> Result<String, String>;
    fn write_file(&self, path: &str, content: &str) -> Result<(), String>;
    fn file_exists(&self, path: &str) -> bool;
    fn env(&self, name: &str) -> Option<String>;
    fn log(&self, message: &str);
    fn warn(&self, message: &str);
    fn container(&self) -> String;
    fn stage(&self) -> String;
}

enum HookRef {
    Task(String),
    Inline(RegistryKey),
}

pub struct LuaHost {
    lua: Lua,
    tasks: Rc<RefCell<HashMap<String, RegistryKey>>>,
    hooks: Rc<RefCell<HashMap<String, Vec<HookRef>>>>,
    cfg_key: RegistryKey,
    state_key: RegistryKey,
    vars_key: RegistryKey,
}

impl LuaHost {
    pub fn load(config: &Config, plugins: &[(String, String)]) -> Result<LuaHost, String> {
        let lua = Lua::new();
        let cfg_value = to_lua(&lua, config).map_err(|e| format!("config to lua: {e}"))?;
        let cfg_key = lua.create_registry_value(cfg_value).map_err(|e| e.to_string())?;
        let state = lua.create_table().map_err(|e| e.to_string())?;
        let state_key = lua.create_registry_value(state).map_err(|e| e.to_string())?;
        let vars = lua.create_table().map_err(|e| e.to_string())?;
        let vars_key = lua.create_registry_value(vars).map_err(|e| e.to_string())?;
        let host = LuaHost {
            tasks: Rc::new(RefCell::new(HashMap::new())),
            hooks: Rc::new(RefCell::new(HashMap::new())),
            cfg_key,
            state_key,
            vars_key,
            lua,
        };
        host.register_globals().map_err(|e| e.to_string())?;
        for (name, source) in plugins {
            host.lua
                .load(source.as_str())
                .set_name(name.as_str())
                .exec()
                .map_err(|e| format!("{name}: {e}"))?;
        }
        Ok(host)
    }

    pub fn has_hook(&self, slot: &str) -> bool {
        self.hooks.borrow().contains_key(slot)
    }

    /// Overwrite the live `cfg` and `state` tables (in place, preserving table identity
    /// so the global `cfg` and any held references stay valid) from the engine's current
    /// typed config and stage. Called before each hook so plugins read up-to-date data.
    pub fn refresh(&self, config: &Config, stage: &StageState) -> Result<(), String> {
        self.overwrite_table(&self.cfg_key, config).map_err(|e| e.to_string())?;
        self.overwrite_table(&self.state_key, stage).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// The live `cfg` table as YAML, for the engine to read back after a hook may have
    /// mutated it. Compared against the pre-hook value to detect real changes.
    pub fn read_cfg(&self) -> Result<serde_yaml::Value, String> {
        self.read_table(&self.cfg_key)
    }

    /// The live `state` table as YAML (the current stage's deploy state), read back after
    /// a hook may have mutated it.
    pub fn read_state(&self) -> Result<serde_yaml::Value, String> {
        self.read_table(&self.state_key)
    }

    fn read_table(&self, key: &RegistryKey) -> Result<serde_yaml::Value, String> {
        let table: Table = self.lua.registry_value(key).map_err(|e| e.to_string())?;
        self.lua.from_value(Value::Table(table)).map_err(|e| e.to_string())
    }

    fn overwrite_table(&self, key: &RegistryKey, value: &impl Serialize) -> mlua::Result<()> {
        let table: Table = self.lua.registry_value(key)?;
        if let Value::Table(source) = to_lua(&self.lua, value)? {
            for pair in source.pairs::<Value, Value>() {
                let (k, v) = pair?;
                table.set(k, v)?;
            }
        }
        Ok(())
    }

    pub fn fire(&self, host: &dyn HookHost, slot: &str) -> Result<(), String> {
        for function in self.functions_for(slot)? {
            self.call(host, &function)?;
        }
        Ok(())
    }

    fn functions_for(&self, slot: &str) -> Result<Vec<Function>, String> {
        let hooks = self.hooks.borrow();
        let Some(list) = hooks.get(slot) else {
            return Ok(Vec::new());
        };
        let tasks = self.tasks.borrow();
        let mut functions = Vec::with_capacity(list.len());
        for href in list {
            let key = match href {
                HookRef::Task(name) => tasks
                    .get(name)
                    .ok_or_else(|| format!("hook references unknown task `{name}`"))?,
                HookRef::Inline(key) => key,
            };
            functions.push(self.lua.registry_value::<Function>(key).map_err(|e| e.to_string())?);
        }
        Ok(functions)
    }

    fn call(&self, host: &dyn HookHost, function: &Function) -> Result<(), String> {
        let cfg: Table = self.lua.registry_value(&self.cfg_key).map_err(|e| e.to_string())?;
        let state: Table = self.lua.registry_value(&self.state_key).map_err(|e| e.to_string())?;
        let vars: Table = self.lua.registry_value(&self.vars_key).map_err(|e| e.to_string())?;
        self.lua
            .scope(|scope| {
                let ctx = self.lua.create_table()?;
                let cfg_dump = cfg.clone();
                let state_dump = state.clone();
                ctx.set("cfg", cfg)?;
                ctx.set("vars", vars.clone())?;
                ctx.set("state", state)?;
                ctx.set("container", host.container())?;
                ctx.set("stage", host.stage())?;

                ctx.set("inspect", scope.create_function(|lua, value: Value| {
                    let yaml: serde_yaml::Value = lua.from_value(value)?;
                    serde_yaml::to_string(&yaml).map_err(runtime)
                })?)?;
                ctx.set("dump", scope.create_function(move |lua, value: Value| {
                    let target = if matches!(value, Value::Nil) {
                        let combined = lua.create_table()?;
                        combined.set("cfg", cfg_dump.clone())?;
                        combined.set("state", state_dump.clone())?;
                        Value::Table(combined)
                    } else {
                        value
                    };
                    let yaml: serde_yaml::Value = lua.from_value(target)?;
                    host.log(&format!("[dump]\n{}", serde_yaml::to_string(&yaml).map_err(runtime)?));
                    Ok(())
                })?)?;

                let setter = vars.clone();
                ctx.set("set", scope.create_function(move |_, (k, v): (String, Value)| setter.set(k, v))?)?;
                let getter = vars.clone();
                ctx.set("get", scope.create_function(move |_, k: String| getter.get::<Value>(k))?)?;

                ctx.set("json_decode", scope.create_function(|lua, s: String| {
                    let value: serde_json::Value = serde_json::from_str(&s).map_err(runtime)?;
                    to_lua(lua, &value)
                })?)?;
                ctx.set("json_encode", scope.create_function(|lua, v: Value| {
                    let value: serde_json::Value = lua.from_value(v)?;
                    serde_json::to_string(&value).map_err(runtime)
                })?)?;
                ctx.set("yaml_decode", scope.create_function(|lua, s: String| {
                    let value: serde_yaml::Value = serde_yaml::from_str(&s).map_err(runtime)?;
                    to_lua(lua, &value)
                })?)?;
                ctx.set("yaml_encode", scope.create_function(|lua, v: Value| {
                    let value: serde_yaml::Value = lua.from_value(v)?;
                    serde_yaml::to_string(&value).map_err(runtime)
                })?)?;

                ctx.set("run", scope.create_function(move |_, cmd: String| host.run_host(&cmd).map_err(mlua::Error::RuntimeError))?)?;
                ctx.set("in_release", scope.create_function(move |_, cmd: String| host.in_release(&cmd).map_err(mlua::Error::RuntimeError))?)?;
                ctx.set("exec_in", scope.create_function(move |_, (s, c): (String, String)| host.exec_in(&s, &c).map_err(mlua::Error::RuntimeError))?)?;
                ctx.set("docker", scope.create_function(move |_, a: Vec<String>| host.docker(a).map_err(mlua::Error::RuntimeError))?)?;
                ctx.set("compose", scope.create_function(move |_, a: Vec<String>| host.compose(a).map_err(mlua::Error::RuntimeError))?)?;
                ctx.set("cp_from_release", scope.create_function(move |_, (f, t): (String, String)| host.cp_from_release(&f, &t).map_err(mlua::Error::RuntimeError))?)?;
                ctx.set("cp_to_release", scope.create_function(move |_, (f, t): (String, String)| host.cp_to_release(&f, &t).map_err(mlua::Error::RuntimeError))?)?;
                ctx.set("read_file", scope.create_function(move |_, p: String| host.read_file(&p).map_err(mlua::Error::RuntimeError))?)?;
                ctx.set("write_file", scope.create_function(move |_, (p, c): (String, String)| host.write_file(&p, &c).map_err(mlua::Error::RuntimeError))?)?;
                ctx.set("file_exists", scope.create_function(move |_, p: String| Ok(host.file_exists(&p)))?)?;
                ctx.set("env", scope.create_function(move |_, n: String| Ok(host.env(&n)))?)?;
                ctx.set("log", scope.create_function(move |_, m: String| {
                    host.log(&m);
                    Ok(())
                })?)?;
                ctx.set("warn", scope.create_function(move |_, m: String| {
                    host.warn(&m);
                    Ok(())
                })?)?;

                function.call::<()>(ctx)
            })
            .map_err(|e| e.to_string())
    }

    fn register_globals(&self) -> mlua::Result<()> {
        let globals = self.lua.globals();

        let tasks = Rc::clone(&self.tasks);
        globals.set(
            "task",
            self.lua.create_function(move |lua, (name, func): (String, Function)| {
                tasks.borrow_mut().insert(name, lua.create_registry_value(func)?);
                Ok(())
            })?,
        )?;

        for (global, prefix) in [("before", "before"), ("after", "after")] {
            let hooks = Rc::clone(&self.hooks);
            globals.set(
                global,
                self.lua.create_function(move |lua, (task, hook): (String, Value)| {
                    let slot = format!("{prefix}_{}", task.replace(':', "_"));
                    let href = match hook {
                        Value::String(name) => HookRef::Task(name.to_str()?.to_string()),
                        Value::Function(func) => HookRef::Inline(lua.create_registry_value(func)?),
                        other => {
                            return Err(mlua::Error::RuntimeError(format!(
                                "hook must be a task name or function, got {}",
                                other.type_name()
                            )))
                        }
                    };
                    hooks.borrow_mut().entry(slot).or_default().push(href);
                    Ok(())
                })?,
            )?;
        }

        let vars: Table = self.lua.registry_value(&self.vars_key)?;
        let setter = vars.clone();
        globals.set("set", self.lua.create_function(move |_, (k, v): (String, Value)| setter.set(k, v))?)?;
        let getter = vars.clone();
        globals.set("get", self.lua.create_function(move |_, k: String| getter.get::<Value>(k))?)?;

        let hooks = Rc::clone(&self.hooks);
        globals.set(
            "configure",
            self.lua.create_function(move |lua, func: Function| {
                let key = lua.create_registry_value(func)?;
                hooks.borrow_mut().entry("configure".to_string()).or_default().push(HookRef::Inline(key));
                Ok(())
            })?,
        )?;

        let cfg: Table = self.lua.registry_value(&self.cfg_key)?;
        globals.set("cfg", cfg)?;
        let state: Table = self.lua.registry_value(&self.state_key)?;
        globals.set("state", state)?;

        self.sandbox(&globals)?;
        Ok(())
    }

    fn sandbox(&self, globals: &Table) -> mlua::Result<()> {
        if let Ok(os) = globals.get::<Table>("os") {
            for key in ["execute", "exit", "getenv", "remove", "rename", "tmpname"] {
                os.set(key, Value::Nil)?;
            }
        }
        if let Ok(io) = globals.get::<Table>("io") {
            for key in ["popen", "open", "lines", "input", "output"] {
                io.set(key, Value::Nil)?;
            }
        }
        globals.set("dofile", Value::Nil)?;
        globals.set("loadfile", Value::Nil)?;
        Ok(())
    }
}

fn runtime<E: std::fmt::Display>(error: E) -> mlua::Error {
    mlua::Error::RuntimeError(error.to_string())
}

/// Serialize to Lua with `None`/unit mapped to `nil` (not a null sentinel), so plugins
/// see idiomatic `nil` for absent values.
fn to_lua(lua: &Lua, value: &impl Serialize) -> mlua::Result<Value> {
    let options = mlua::SerializeOptions::new()
        .serialize_none_to_null(false)
        .serialize_unit_to_null(false);
    lua.to_value_with(value, options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Release, ReleaseStatus};
    use indexmap::IndexMap;
    use std::collections::HashMap as Map;

    #[derive(Default)]
    struct FakeHost {
        calls: RefCell<Vec<String>>,
        files: RefCell<Map<String, String>>,
        logs: RefCell<Vec<String>>,
    }

    impl HookHost for FakeHost {
        fn run_host(&self, cmd: &str) -> Result<String, String> {
            self.calls.borrow_mut().push(format!("run:{cmd}"));
            Ok(String::new())
        }
        fn in_release(&self, cmd: &str) -> Result<String, String> {
            self.calls.borrow_mut().push(format!("in_release:{cmd}"));
            Ok(r#"{"port": 8080}"#.into())
        }
        fn exec_in(&self, service: &str, cmd: &str) -> Result<String, String> {
            self.calls.borrow_mut().push(format!("exec_in:{service}:{cmd}"));
            Ok(String::new())
        }
        fn docker(&self, args: Vec<String>) -> Result<String, String> {
            self.calls.borrow_mut().push(format!("docker:{}", args.join(" ")));
            Ok(String::new())
        }
        fn compose(&self, args: Vec<String>) -> Result<String, String> {
            self.calls.borrow_mut().push(format!("compose:{}", args.join(" ")));
            Ok(String::new())
        }
        fn cp_from_release(&self, _from: &str, _to: &str) -> Result<(), String> {
            Ok(())
        }
        fn cp_to_release(&self, _from: &str, _to: &str) -> Result<(), String> {
            Ok(())
        }
        fn read_file(&self, path: &str) -> Result<String, String> {
            self.files.borrow().get(path).cloned().ok_or_else(|| "missing".into())
        }
        fn write_file(&self, path: &str, content: &str) -> Result<(), String> {
            self.files.borrow_mut().insert(path.into(), content.into());
            Ok(())
        }
        fn file_exists(&self, path: &str) -> bool {
            self.files.borrow().contains_key(path)
        }
        fn env(&self, name: &str) -> Option<String> {
            (name == "CDN_HOST").then(|| "cdn.example".into())
        }
        fn log(&self, message: &str) {
            self.logs.borrow_mut().push(message.to_string());
        }
        fn warn(&self, _message: &str) {}
        fn container(&self) -> String {
            "black-1".into()
        }
        fn stage(&self) -> String {
            "prod".into()
        }
    }

    fn stage() -> StageState {
        let mut images = IndexMap::new();
        images.insert("app".to_string(), "img-0".to_string());
        StageState {
            current: Some("black-0".into()),
            releases: vec![Release {
                id: 0,
                container: "black-0".into(),
                images,
                created_at: 0,
                status: ReleaseStatus::Active,
                ran_migrations: false,
                reason: None,
                env_keys: Vec::new(),
                reaped: false,
            }],
            pulled: Vec::new(),
        }
    }

    fn config() -> Config {
        let src = r#"
version: 2
project: demo
compose: { files: [c.yml] }
release:
  service: app
  container_prefix: demo-app
  healthcheck: { exec_in: x, cmd: 'curl {container}' }
cutover: { service: x, backend_port: 80, reload: { exec_in: x, cmd: 'r' } }
services: { x: { recreate: never } }
stages: { prod: {} }
"#;
        crate::config::load(src, Some("prod"), &[], &Map::new()).unwrap()
    }

    #[test]
    fn registers_task_and_fires_after_hook() {
        let plugin = r#"
            task('centrifugo', function(ctx)
              local cfg = ctx.json_decode(ctx.in_release('app config:dump'))
              ctx.write_file('out.json', ctx.json_encode({ port = cfg.port, host = ctx.env('CDN_HOST') }))
              ctx.compose({'up', '-d', 'centrifugo'})
            end)
            after('healthcheck', 'centrifugo')
        "#;
        let host = LuaHost::load(&config(), &[("p".into(), plugin.into())]).unwrap();
        assert!(host.has_hook("after_healthcheck"));
        let fake = FakeHost::default();
        host.fire(&fake, "after_healthcheck").unwrap();
        assert_eq!(fake.calls.borrow()[0], "in_release:app config:dump");
        assert_eq!(fake.calls.borrow()[1], "compose:up -d centrifugo");
        let written = fake.files.borrow().get("out.json").cloned().unwrap();
        assert!(written.contains("\"port\":8080"));
        assert!(written.contains("\"host\":\"cdn.example\""));
    }

    #[test]
    fn cfg_and_state_are_readable() {
        let plugin = r#"
            after('cutover', function(ctx)
              ctx.run(ctx.cfg.project .. ' ' .. ctx.stage .. ' ' .. ctx.state.current)
            end)
        "#;
        let host = LuaHost::load(&config(), &[("p".into(), plugin.into())]).unwrap();
        host.refresh(&config(), &stage()).unwrap();
        let fake = FakeHost::default();
        host.fire(&fake, "after_cutover").unwrap();
        assert_eq!(*fake.calls.borrow(), vec!["run:demo prod black-0".to_string()]);
    }

    #[test]
    fn vars_persist_across_hooks() {
        let plugin = r#"
            after('healthcheck', function(ctx) ctx.set('token', 'abc123') end)
            after('cutover', function(ctx) ctx.run('use ' .. ctx.get('token')) end)
        "#;
        let host = LuaHost::load(&config(), &[("p".into(), plugin.into())]).unwrap();
        let fake = FakeHost::default();
        host.fire(&fake, "after_healthcheck").unwrap();
        host.fire(&fake, "after_cutover").unwrap();
        assert_eq!(*fake.calls.borrow(), vec!["run:use abc123".to_string()]);
    }

    #[test]
    fn cfg_mutation_in_hook_flows_back() {
        // Direct assignment to ctx.cfg is read back as the new typed config — no helper.
        let plugin = r#"
            configure(function(ctx)
              if ctx.cfg.project == 'demo' then
                ctx.cfg.retention.keep_releases = 9
                ctx.cfg.project = 'renamed'
              end
            end)
        "#;
        let host = LuaHost::load(&config(), &[("p".into(), plugin.into())]).unwrap();
        assert!(host.has_hook("configure"));
        host.refresh(&config(), &stage()).unwrap();
        host.fire(&FakeHost::default(), "configure").unwrap();
        let synced = crate::config::from_lua_value(host.read_cfg().unwrap(), "prod").unwrap();
        assert_eq!(synced.retention.keep_releases, 9);
        assert_eq!(synced.project, "renamed");
    }

    #[test]
    fn state_mutation_in_hook_flows_back() {
        let plugin = r#"
            after('cutover', function(ctx)
              ctx.state.current = 'black-1'
              ctx.state.releases[1].status = 'superseded'
            end)
        "#;
        let host = LuaHost::load(&config(), &[("p".into(), plugin.into())]).unwrap();
        host.refresh(&config(), &stage()).unwrap();
        host.fire(&FakeHost::default(), "after_cutover").unwrap();
        let synced: StageState = serde_yaml::from_value(host.read_state().unwrap()).unwrap();
        assert_eq!(synced.current.as_deref(), Some("black-1"));
        assert_eq!(synced.releases[0].status, ReleaseStatus::Superseded);
    }

    #[test]
    fn dump_logs_formatted_state_and_cfg() {
        // Both halves the name promises: `ctx.state` AND `ctx.cfg`. Only state was
        // ever dumped, so the cfg half was untested.
        let plugin = r#"
            after('cutover', function(ctx)
              ctx.dump(ctx.state)
              ctx.dump(ctx.cfg)
            end)
        "#;
        let host = LuaHost::load(&config(), &[("p".into(), plugin.into())]).unwrap();
        host.refresh(&config(), &stage()).unwrap();
        let fake = FakeHost::default();
        host.fire(&fake, "after_cutover").unwrap();
        let logged = fake.logs.borrow().join("\n");
        assert!(logged.contains("[dump]"));
        assert!(logged.contains("current: black-0"), "the state dump: {logged}");
        assert!(logged.contains("project:"), "the cfg dump: {logged}");
    }

    #[test]
    fn unknown_task_reference_errors() {
        // A distinctive name, not a word that could appear in an unrelated message:
        // `ghost` is five letters and could match by accident, which would let this
        // pass on the wrong error entirely.
        let missing = "no-such-task-zqx";
        let plugin = format!("after('cutover', '{missing}')");
        let host = LuaHost::load(&config(), &[("p".into(), plugin)]).unwrap();
        let err = host.fire(&FakeHost::default(), "after_cutover").unwrap_err();
        assert!(err.contains(missing), "the error must name the task: {err}");
        assert!(
            err.contains("task") || err.contains("unknown"),
            "and say what was wrong with it: {err}"
        );
    }

    /// `LuaHost::load` EXECUTES the chunk, so the payload must be harmless if the
    /// sandbox ever regresses — a destructive one would run for real on the machine
    /// running the tests. And `is_err()` alone is the weakest possible check: a
    /// syntax error satisfies it just as well as a removed `os.execute`, so the
    /// reason is asserted too.
    #[test]
    fn sandbox_removes_process_escapes() {
        for escape in ["os.execute('true')", "io.popen('true')"] {
            let outcome = LuaHost::load(&config(), &[("p".into(), escape.into())]);
            let Err(error) = outcome else {
                panic!("{escape} must not be reachable");
            };
            let error = error.to_string();
            assert!(
                error.contains("nil value") || error.contains("attempt to index"),
                "{escape} failed for the wrong reason: {error}"
            );
        }
    }
}
