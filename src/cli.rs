//! CLI surface (spec §8): parse, then orchestrate host guard, lock, state I/O,
//! and the engine. Command bodies are thin; the work lives in the typed modules.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

use crate::config::{self, Loaded};
use crate::effects::{
    Access, Argv, CommandRunner, DryRunRunner, FileSystem, RunOpts, SystemClock, SystemFs, SystemRunner,
};
use crate::engine::{Engine, Options, DEPLOY_STEPS};
use crate::error::{DcdError, Result};
use crate::lua::{HookHost, LuaHost, StateView};
use crate::redact::Redactor;
use crate::signal::Interrupt;
use crate::state::State;
use crate::ui::Reporter;

#[derive(Parser)]
#[command(name = "dcd", version, about = "Zero-downtime red-black Docker deploys from a YAML config")]
pub struct Cli {
    #[arg(short, long, global = true, default_value = "dcd.yaml")]
    config: std::path::PathBuf,
    #[arg(long, global = true)]
    json: bool,
    #[arg(long, global = true)]
    dry_run: bool,
    #[arg(long, global = true)]
    resume: bool,
    #[arg(long = "set", global = true, value_name = "PATH=VALUE")]
    sets: Vec<String>,
    #[arg(long = "image", global = true, value_name = "LOGICAL=TAG")]
    images: Vec<String>,
    #[arg(short = 'y', long, global = true)]
    yes: bool,
    #[arg(long, global = true)]
    reason: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the red-black deploy (use --resume to recover an incomplete release).
    Deploy { stage: Option<String> },
    /// Roll back to the previous release (code only; no migrations).
    Rollback { stage: Option<String> },
    /// Show the current release and history.
    Status { stage: Option<String> },
    /// Print the resolved task plan without executing.
    Tasks { stage: Option<String> },
    /// Validate the config (and stage merge, interpolation, overrides).
    Check { stage: Option<String> },
    /// Scaffold a starter dcd.yaml.
    Init {
        #[arg(long)]
        force: bool,
        #[arg(long)]
        with_plugin: bool,
    },
}

pub fn run() -> Result<()> {
    dispatch(Cli::parse())
}

fn stage_of(command: &Command) -> Option<&str> {
    match command {
        Command::Deploy { stage }
        | Command::Rollback { stage }
        | Command::Status { stage }
        | Command::Tasks { stage }
        | Command::Check { stage } => stage.as_deref(),
        Command::Init { .. } => None,
    }
}

fn dispatch(cli: Cli) -> Result<()> {
    if let Command::Init { force, with_plugin } = &cli.command {
        return init(&cli.config, *force, *with_plugin);
    }

    let env: std::collections::HashMap<String, String> = std::env::vars().collect();
    let mut sets = cli.sets.clone();
    for image in &cli.images {
        let (logical, tag) = image
            .split_once('=')
            .ok_or_else(|| DcdError::Config(format!("--image `{image}` must be logical=tag")))?;
        sets.push(format!("images.{logical}={tag}"));
    }

    let source = std::fs::read_to_string(&cli.config)
        .map_err(|e| DcdError::Config(format!("cannot read {}: {e}", cli.config.display())))?;
    let mut loaded = config::load(&source, stage_of(&cli.command), &sets, &env)?;
    let reporter = Reporter::auto(cli.json, loaded.redactor.clone());

    match &cli.command {
        Command::Deploy { .. } | Command::Rollback { .. } => {
            let run = if matches!(cli.command, Command::Rollback { .. }) {
                Run::Rollback
            } else {
                Run::Deploy
            };
            let plugins = load_plugins(&loaded.config)?;
            let mut lua_host = if plugins.is_empty() {
                None
            } else {
                Some(LuaHost::load(&loaded.config, &plugins).map_err(DcdError::Lua)?)
            };
            if let Some(host) = &lua_host {
                if host.has_hook("configure") {
                    let configure_host = ConfigureHost {
                        reporter: &reporter,
                        redactor: loaded.redactor.clone(),
                        deploy_root: loaded.config.deploy_root.clone(),
                        stage: loaded.config.stage.clone(),
                    };
                    host.fire(&configure_host, "configure").map_err(DcdError::Lua)?;
                    let overrides = host.config_overrides();
                    if !overrides.is_empty() {
                        loaded = config::reconfigure(&loaded.config, &overrides)?;
                        lua_host = Some(LuaHost::load(&loaded.config, &plugins).map_err(DcdError::Lua)?);
                    }
                }
            }
            execute(&loaded, &cli, &reporter, run, lua_host.as_ref())
        }
        Command::Status { .. } => status(&loaded, &reporter),
        Command::Tasks { .. } => tasks(&loaded, &reporter),
        Command::Check { .. } => {
            reporter.log(&format!("config ok ({} stage '{}')", loaded.config.project, loaded.config.stage));
            Ok(())
        }
        Command::Init { .. } => unreachable!("handled above"),
    }
}

enum Run {
    Deploy,
    Rollback,
}

fn execute(loaded: &Loaded, cli: &Cli, reporter: &Reporter, run: Run, lua: Option<&LuaHost>) -> Result<()> {
    let cfg = &loaded.config;
    host_guard(cfg)?;

    let clock = SystemClock;
    let holder = format!("pid {} since {}", std::process::id(), hhmmss(now_epoch()));
    let _lock = crate::lock::StageLock::acquire(&cfg.deploy_root, &cfg.stage, &holder)?;

    let state = load_state(&cfg.deploy_root)?;
    let interrupt = Interrupt::install();
    let opts = Options {
        dry_run: cli.dry_run,
        sleep_enabled: true,
        reason: cli.reason.clone(),
    };

    let runner: Box<dyn CommandRunner> = if cli.dry_run {
        Box::new(DryRunRunner::new(SystemRunner))
    } else {
        Box::new(SystemRunner)
    };
    let fs = SystemFs;

    let mut engine = Engine::new(cfg, runner.as_ref(), &fs, &clock, reporter, &loaded.redactor, &interrupt, state, opts);
    if let Some(host) = lua {
        engine = engine.with_plugins(host);
    }

    match run {
        Run::Deploy if cli.resume => engine.resume(),
        Run::Deploy => engine.deploy(),
        Run::Rollback => {
            if !confirm(&format!("Roll back {}?", cfg.stage), cli.yes)? {
                return Err(DcdError::PreCutover("rollback declined (pass --yes to confirm)".to_string()));
            }
            engine.rollback()
        }
    }
}

fn status(loaded: &Loaded, reporter: &Reporter) -> Result<()> {
    let state = load_state(&loaded.config.deploy_root)?;
    let stage = &loaded.config.stage;
    match state.stage(stage) {
        None => reporter.log(&format!("{stage}: no deploys recorded")),
        Some(s) => {
            reporter.log(&format!("{stage}: current = {}", s.current.as_deref().unwrap_or("none")));
            if let Some(pending) = s.cutover_pending() {
                reporter.warn(&format!(
                    "incomplete release {} — run `dcd deploy --resume {stage}` or `dcd rollback {stage}`",
                    pending.container
                ));
            }
            for release in s.releases.iter().rev() {
                let migrated = if release.ran_migrations { " (ran migrations)" } else { "" };
                reporter.log(&format!(
                    "  {} {:?} {}{}",
                    release.container,
                    release.status,
                    release.app_image().unwrap_or("?"),
                    migrated
                ));
            }
        }
    }
    Ok(())
}

fn tasks(loaded: &Loaded, reporter: &Reporter) -> Result<()> {
    reporter.log(&format!("plan for {} stage '{}':", loaded.config.project, loaded.config.stage));
    for step in DEPLOY_STEPS {
        let key = step.replace(':', "_");
        for hook in loaded.config.hooks.get(&format!("before_{key}")).into_iter().flatten() {
            reporter.plan(&format!("before {step}: {}", describe_hook(hook)));
        }
        reporter.log(&format!("- {step}"));
        for hook in loaded.config.hooks.get(&format!("after_{key}")).into_iter().flatten() {
            reporter.plan(&format!("after {step}: {}", describe_hook(hook)));
        }
    }
    Ok(())
}

fn describe_hook(hook: &config::HookAction) -> String {
    use config::HookAction::*;
    match hook {
        Run(cmd) => format!("run `{cmd}`"),
        ExecIn { exec_in } => format!("exec in {}: `{}`", exec_in.service, exec_in.cmd),
        ExecInRelease { exec_in_release } => format!("exec in release: `{exec_in_release}`"),
        Docker { docker } => format!("docker {}", docker.join(" ")),
        Compose { compose } => format!("compose {}", compose.join(" ")),
        CpFromRelease { cp_from_release } => format!("cp {} -> {}", cp_from_release.from, cp_from_release.to),
        CpToRelease { cp_to_release } => format!("cp {} -> {}", cp_to_release.from, cp_to_release.to),
    }
}

fn host_guard(cfg: &config::Config) -> Result<()> {
    let actual = gethostname::gethostname().to_string_lossy().to_string();
    host_check(cfg.host.as_deref(), &actual, &cfg.stage)
}

fn host_check(expected: Option<&str>, actual: &str, stage: &str) -> Result<()> {
    match expected {
        Some(host) if host != actual => Err(DcdError::HostMismatch {
            stage: stage.to_string(),
            expected: host.to_string(),
            actual: actual.to_string(),
        }),
        _ => Ok(()),
    }
}

fn load_state(deploy_root: &Path) -> Result<State> {
    let path = deploy_root.join("dcd-state.json");
    match std::fs::read(&path) {
        Ok(bytes) => State::from_json(&bytes).map_err(|e| DcdError::Config(format!("corrupt state {}: {e}", path.display()))),
        Err(_) => Ok(State::default()),
    }
}

fn confirm(prompt: &str, yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    print!("{prompt} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    Ok(matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

fn init(path: &Path, force: bool, with_plugin: bool) -> Result<()> {
    if path.exists() && !force {
        return Err(DcdError::Config(format!("{} already exists (use --force)", path.display())));
    }
    std::fs::write(path, SCAFFOLD).map_err(|e| DcdError::Config(format!("cannot write {}: {e}", path.display())))?;
    println!("created {}", path.display());
    if with_plugin {
        let dir = path.parent().unwrap_or(Path::new(".")).join("plugins");
        let _ = std::fs::create_dir_all(&dir);
        let plugin = dir.join("app.lua");
        std::fs::write(&plugin, PLUGIN_STUB).map_err(|e| DcdError::Config(format!("cannot write {}: {e}", plugin.display())))?;
        println!("created {}", plugin.display());
    }
    Ok(())
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn hhmmss(epoch: u64) -> String {
    let day = epoch % 86_400;
    format!("{:02}:{:02}:{:02}", day / 3600, (day % 3600) / 60, day % 60)
}

fn load_plugins(cfg: &config::Config) -> Result<Vec<(String, String)>> {
    let mut plugins = Vec::new();
    for path in &cfg.plugins {
        let resolved = if path.is_absolute() {
            path.clone()
        } else {
            cfg.deploy_root.join(path)
        };
        let content = std::fs::read_to_string(&resolved)
            .map_err(|e| DcdError::Config(format!("cannot read plugin {}: {e}", resolved.display())))?;
        plugins.push((path.display().to_string(), content));
    }
    Ok(plugins)
}

/// The host backing the `configure` hook — runs before the engine, so it offers
/// host-level effects (run/files/env) but no release-container operations.
struct ConfigureHost<'a> {
    reporter: &'a Reporter,
    redactor: Redactor,
    deploy_root: PathBuf,
    stage: String,
}

impl ConfigureHost<'_> {
    fn resolve(&self, path: &str) -> PathBuf {
        let path = Path::new(path);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.deploy_root.join(path)
        }
    }

    fn no_container(&self, what: &str) -> String {
        format!("ctx.{what} is unavailable in the configure hook (no release container yet)")
    }
}

impl HookHost for ConfigureHost<'_> {
    fn run_host(&self, cmd: &str) -> std::result::Result<String, String> {
        let full = format!("cd {} && {}", self.deploy_root.display(), cmd);
        SystemRunner
            .run(&Argv::of(["sh", "-c", &full]), Access::Mutate, &RunOpts::default())
            .map(|o| o.stdout)
            .map_err(|e| self.redactor.apply(&e.to_string()))
    }

    fn in_release(&self, _cmd: &str) -> std::result::Result<String, String> {
        Err(self.no_container("in_release"))
    }
    fn exec_in(&self, service: &str, cmd: &str) -> std::result::Result<String, String> {
        let argv = Argv::of(["docker", "exec", service, "sh", "-c", cmd]);
        SystemRunner
            .run(&argv, Access::Mutate, &RunOpts::default())
            .map(|o| o.stdout)
            .map_err(|e| self.redactor.apply(&e.to_string()))
    }
    fn docker(&self, args: Vec<String>) -> std::result::Result<String, String> {
        let mut argv = vec!["docker".to_string()];
        argv.extend(args);
        SystemRunner
            .run(&Argv(argv), Access::Mutate, &RunOpts::default())
            .map(|o| o.stdout)
            .map_err(|e| self.redactor.apply(&e.to_string()))
    }
    fn compose(&self, _args: Vec<String>) -> std::result::Result<String, String> {
        Err(self.no_container("compose"))
    }
    fn cp_from_release(&self, _from: &str, _to: &str) -> std::result::Result<(), String> {
        Err(self.no_container("cp_from_release"))
    }
    fn cp_to_release(&self, _from: &str, _to: &str) -> std::result::Result<(), String> {
        Err(self.no_container("cp_to_release"))
    }
    fn read_file(&self, path: &str) -> std::result::Result<String, String> {
        SystemFs
            .read(&self.resolve(path))
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .map_err(|e| e.to_string())
    }
    fn write_file(&self, path: &str, content: &str) -> std::result::Result<(), String> {
        SystemFs.write(&self.resolve(path), content.as_bytes(), None).map_err(|e| e.to_string())
    }
    fn file_exists(&self, path: &str) -> bool {
        SystemFs.exists(&self.resolve(path))
    }
    fn env(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
    fn log(&self, message: &str) {
        self.reporter.log(message);
    }
    fn warn(&self, message: &str) {
        self.reporter.warn(message);
    }
    fn container(&self) -> String {
        String::new()
    }
    fn stage(&self) -> String {
        self.stage.clone()
    }
    fn state_view(&self) -> StateView {
        let state = load_state(&self.deploy_root).unwrap_or_default();
        StateView::from_stage(state.stage(&self.stage))
    }
}

const SCAFFOLD: &str = r#"version: 1
project: myapp
network: myapp_net
registry: ${REGISTRY}

# Block style (not flow) is required for ${VAR} values.
images:
  app: ${APP_TAG}

compose:
  files: [docker-compose.prod.yml]
  env_file: compose.env
  env:
    REGISTRY: ${REGISTRY}
    APP_TAG: ${APP_TAG}

services:
  nginx:
    image: ~          # upstream image pinned in compose; dcd never recreates it
    container: myapp-nginx
    recreate: never
    wait: { exec_in: myapp-nginx, cmd: 'test -f /var/run/nginx.pid', retries: 30, interval: 1s }

release:
  image: app
  container_prefix: myapp-app
  run:
    network_alias: app
    env: { TZ: UTC }
  healthcheck:
    exec_in: myapp-nginx
    cmd: 'curl -sf http://{container}:8080/health'   # {container}, never the alias
    retries: 60
    interval: 2s
  # migrate: { before: '...', after: '...' }   # optional, expand-contract

cutover:
  upstream_file: nginx-upstream.conf
  backend_port: 8080
  reload: { exec_in: myapp-nginx, cmd: 'nginx -s reload' }

retention: { keep_releases: 3 }

stages:
  prod:
    host: prod.example.internal   # dcd refuses to run on the wrong host
"#;

const PLUGIN_STUB: &str = r#"-- Plugins (loaded at startup) register tasks and hooks.
--
-- Adjust the config before the deploy, based on runtime truths:
-- configure(function(ctx)
--   if ctx.env('CANARY') == '1' then ctx.set_config('retention.keep_releases', 5) end
-- end)
--
-- ctx available inside hooks:
--   effects: run, in_release, exec_in, docker, compose, cp_from_release, cp_to_release
--   files:   read_file, write_file, file_exists, env
--   data:    cfg (parsed config), state (current + history), vars (scratch, shared across hooks)
--   utils:   json_decode/encode, yaml_decode/encode, log, warn, dump, inspect
--
-- task('myapp:warmup', function(ctx)
--   ctx.log('warming up ' .. ctx.container)
--   ctx.in_release('php bin/console cache:warmup')
-- end)
-- after('healthcheck', 'myapp:warmup')
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_check_guards_mismatch_only() {
        assert!(host_check(None, "anybox", "prod").is_ok());
        assert!(host_check(Some("prod-host"), "prod-host", "prod").is_ok());
        let err = host_check(Some("prod-host"), "beta-box", "prod").unwrap_err();
        assert_eq!(err.exit_code(), 5);
        assert!(err.to_string().contains("prod-host"));
    }

    #[test]
    fn scaffold_parses_and_validates() {
        let env: std::collections::HashMap<String, String> =
            [("REGISTRY", "reg"), ("APP_TAG", "t")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let loaded = config::load(SCAFFOLD, Some("prod"), &[], &env).unwrap();
        assert_eq!(loaded.config.project, "myapp");
    }
}
