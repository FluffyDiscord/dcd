//! Embedded-Lua plugin host (spec §6). Plugins register tasks and before/after
//! hooks at load time; at run time a hook fires with a `ctx`. The `ctx` carries
//! engine-routed effects (run, in_release, exec_in, docker, compose, cp_*,
//! read_file, write_file, file_exists, env — all dry-run-safe and redacted),
//! utilities (json/yaml encode+decode, log, warn), and data (`cfg` and a `vars`
//! scratch space as persistent tables shared across hooks, plus a `state` snapshot).
//! All effects go through `ctx`, never raw os/io (sandboxed), so they stay observable.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use mlua::{Function, Lua, LuaSerdeExt, RegistryKey, Table, Value};
use serde::Serialize;

use crate::config::Config;

#[derive(Serialize)]
pub struct ReleaseView {
    pub container: String,
    pub status: String,
    pub app_image: Option<String>,
    pub ran_migrations: bool,
}

#[derive(Serialize)]
pub struct StateView {
    pub current: Option<String>,
    pub releases: Vec<ReleaseView>,
}

impl StateView {
    pub fn from_stage(stage: Option<&crate::state::StageState>) -> StateView {
        match stage {
            Some(s) => StateView {
                current: s.current.clone(),
                releases: s
                    .releases
                    .iter()
                    .map(|r| ReleaseView {
                        container: r.container.clone(),
                        status: format!("{:?}", r.status),
                        app_image: r.app_image().map(String::from),
                        ran_migrations: r.ran_migrations,
                    })
                    .collect(),
            },
            None => StateView {
                current: None,
                releases: Vec::new(),
            },
        }
    }
}

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
    fn state_view(&self) -> StateView;
}

enum HookRef {
    Task(String),
    Inline(RegistryKey),
}

pub struct LuaHost {
    lua: Lua,
    tasks: Rc<RefCell<HashMap<String, RegistryKey>>>,
    hooks: Rc<RefCell<HashMap<String, Vec<HookRef>>>>,
    overrides: Rc<RefCell<Vec<(String, String)>>>,
    cfg_key: RegistryKey,
    vars_key: RegistryKey,
}

impl LuaHost {
    pub fn load(config: &Config, plugins: &[(String, String)]) -> Result<LuaHost, String> {
        let lua = Lua::new();
        let cfg_value = to_lua(&lua, config).map_err(|e| format!("config to lua: {e}"))?;
        let cfg_key = lua.create_registry_value(cfg_value).map_err(|e| e.to_string())?;
        let vars = lua.create_table().map_err(|e| e.to_string())?;
        let vars_key = lua.create_registry_value(vars).map_err(|e| e.to_string())?;
        let host = LuaHost {
            tasks: Rc::new(RefCell::new(HashMap::new())),
            hooks: Rc::new(RefCell::new(HashMap::new())),
            overrides: Rc::new(RefCell::new(Vec::new())),
            cfg_key,
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

    /// Overrides collected by `ctx.set_config(path, value)` in the configure hook,
    /// as `path=value` strings fed back through `load()` so ordering + validation hold.
    pub fn config_overrides(&self) -> Vec<String> {
        self.overrides.borrow().iter().map(|(path, value)| format!("{path}={value}")).collect()
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
        let vars: Table = self.lua.registry_value(&self.vars_key).map_err(|e| e.to_string())?;
        let state = to_lua(&self.lua, &host.state_view()).map_err(|e| e.to_string())?;
        let overrides = Rc::clone(&self.overrides);
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

                ctx.set("set_config", scope.create_function(move |_, (path, value): (String, Value)| {
                    overrides.borrow_mut().push((path, stringify(&value)));
                    Ok(())
                })?)?;
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

        let overrides = Rc::clone(&self.overrides);
        globals.set(
            "set_config",
            self.lua.create_function(move |_, (path, value): (String, Value)| {
                overrides.borrow_mut().push((path, stringify(&value)));
                Ok(())
            })?,
        )?;

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

fn stringify(value: &Value) -> String {
    match value {
        Value::String(s) => s.to_str().map(|s| s.to_string()).unwrap_or_default(),
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Boolean(b) => b.to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        fn state_view(&self) -> StateView {
            StateView {
                current: Some("black-0".into()),
                releases: vec![ReleaseView {
                    container: "black-0".into(),
                    status: "Active".into(),
                    app_image: Some("img-0".into()),
                    ran_migrations: false,
                }],
            }
        }
    }

    fn config() -> Config {
        let src = r#"
version: 1
project: demo
network: net
images: { app: a }
compose: { files: [c.yml], env_file: e }
release:
  image: app
  container_prefix: demo-app
  healthcheck: { exec_in: x, cmd: 'curl {container}' }
cutover: { backend_port: 80, reload: { exec_in: x, cmd: 'r' } }
services: { x: { container: x, recreate: never } }
stages: { prod: {} }
"#;
        crate::config::load(src, Some("prod"), &[], &Map::new()).unwrap().config
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
    fn configure_collects_config_overrides() {
        let plugin = r#"
            configure(function(ctx)
              if ctx.cfg.project == 'demo' then
                ctx.set_config('retention.keep_releases', 7)
              end
            end)
        "#;
        let host = LuaHost::load(&config(), &[("p".into(), plugin.into())]).unwrap();
        assert!(host.has_hook("configure"));
        host.fire(&FakeHost::default(), "configure").unwrap();
        assert_eq!(host.config_overrides(), vec!["retention.keep_releases=7".to_string()]);
    }

    #[test]
    fn dump_logs_formatted_state_and_cfg() {
        let plugin = r#"
            after('cutover', function(ctx)
              ctx.dump(ctx.state)
            end)
        "#;
        let host = LuaHost::load(&config(), &[("p".into(), plugin.into())]).unwrap();
        let fake = FakeHost::default();
        host.fire(&fake, "after_cutover").unwrap();
        let logged = fake.logs.borrow().join("\n");
        assert!(logged.contains("[dump]"));
        assert!(logged.contains("current: black-0"));
    }

    #[test]
    fn unknown_task_reference_errors() {
        let host = LuaHost::load(&config(), &[("p".into(), "after('cutover', 'ghost')".into())]).unwrap();
        let err = host.fire(&FakeHost::default(), "after_cutover").unwrap_err();
        assert!(err.contains("ghost"));
    }

    #[test]
    fn sandbox_removes_process_escapes() {
        assert!(LuaHost::load(&config(), &[("p".into(), "os.execute('rm -rf /')".into())]).is_err());
        assert!(LuaHost::load(&config(), &[("p".into(), "io.popen('ls')".into())]).is_err());
    }
}
