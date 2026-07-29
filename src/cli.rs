//! CLI surface (spec §8): parse, then orchestrate host guard, lock, state I/O,
//! and the engine. Command bodies are thin; the work lives in the typed modules.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

use crate::config::{self, Config};
use crate::effects::{
    Access, Argv, CommandRunner, DryRunRunner, FileSystem, RunOpts, SystemClock, SystemFs, SystemRunner,
};
use crate::engine::{Engine, Options, DEPLOY_STEPS};
use crate::error::{DcdError, Result};
use crate::lua::{HookHost, LuaHost};
use crate::signal::Interrupt;
use crate::state::State;
use crate::ui::Reporter;

#[derive(Parser)]
#[command(
    name = "dcd",
    version,
    disable_version_flag = true,
    propagate_version = true,
    about = "Zero-downtime red-black Docker deploys from a YAML config",
    long_about = "dcd runs a zero-downtime red-black Docker deploy on the server from a \
dcd.yaml: it builds the new container next to the live one, health-checks it, flips the \
nginx upstream over to it, then drains the old one.\n\n\
Env comes from a Symfony-style dotenv chain next to the config (.env, .env.local, \
.env.<stage>, .env.<stage>.local — later wins, real env wins over all): chain keys are \
delivered to containers via process-env passthrough, and dcd writes no env file. \
--env-file <path> rebases the whole chain onto another base name (e.g. .env.deploy, \
.env.deploy.local, .env.deploy.<stage>, .env.deploy.<stage>.local), so dcd's chain can \
live beside an application's own .env files without colliding.\n\n\
Author a config with `dcd init`, validate it with `dcd check <stage>`, and preview the \
exact plan with `dcd deploy <stage> --dry-run` before committing.",
    after_help = "Config help:\n  \
docs/examples/all_in_one/dcd.yaml  every field, described, with defaults\n  \
docs/examples/roadrunner_app/             a real, lean config\n  \
AGENTS.md                          a guide for authoring one from scratch"
)]
pub struct Cli {
    /// Print the dcd version and exit
    #[arg(short = 'v', long = "version", global = true, action = clap::ArgAction::Version)]
    version: Option<bool>,
    /// Path to the config file
    #[arg(short, long, global = true, default_value = "dcd.yaml")]
    config: std::path::PathBuf,
    /// Emit machine-readable JSON events instead of human output
    #[arg(long, global = true)]
    json: bool,
    /// Print every action without running it (read-only probes still execute)
    #[arg(long, global = true)]
    dry_run: bool,
    /// With `deploy`: finish an incomplete release that died after cutover
    #[arg(long, global = true)]
    resume: bool,
    /// Override a config value by dotted path, e.g. retention.keep_releases=5 (repeatable)
    #[arg(long = "set", global = true, value_name = "PATH=VALUE")]
    sets: Vec<String>,
    /// Override an image tag — sets docker.images.<logical> (repeatable)
    #[arg(long = "image", global = true, value_name = "LOGICAL=TAG")]
    images: Vec<String>,
    /// Directory of the dotenv chain (.env, .env.local, .env.<stage>[.local]);
    /// defaults to the config file's directory
    #[arg(long, global = true, value_name = "PATH")]
    env_dir: Option<PathBuf>,
    /// Chain base file (Symfony loadEnv semantics): loads <path>[.dist],
    /// <path>.local, <path>.<stage>, <path>.<stage>.local; the base must exist
    #[arg(long, global = true, value_name = "PATH", conflicts_with = "env_dir")]
    env_file: Option<PathBuf>,
    /// Read one extra dotenv document from stdin as the highest file layer
    /// (nothing lands on disk); requires piped input and -y for prompting commands
    #[arg(long, global = true)]
    env_stdin: bool,
    /// Skip confirmation prompts (e.g. for rollback)
    #[arg(short = 'y', long, global = true)]
    yes: bool,
    /// Record a reason on the release (kept in deploy state)
    #[arg(long, global = true)]
    reason: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the red-black deploy (use --resume to recover an incomplete release).
    Deploy {
        /// Stage to deploy (omit if the config defines exactly one)
        stage: Option<String>,
    },
    /// Roll back to the previous release (code only; no migrations).
    Rollback {
        /// Stage to roll back (omit if the config defines exactly one)
        stage: Option<String>,
    },
    /// Show the current release and history.
    Status {
        /// Stage to inspect (omit if the config defines exactly one)
        stage: Option<String>,
    },
    /// Print the resolved task plan without executing.
    Tasks {
        /// Stage to plan (omit if the config defines exactly one)
        stage: Option<String>,
    },
    /// Validate the config — stage merge, interpolation, overrides, and all rules.
    Check {
        /// Stage to validate (omit if the config defines exactly one)
        stage: Option<String>,
    },
    /// Scaffold a starter dcd.yaml (optionally with a Lua plugin stub).
    Init {
        /// Overwrite an existing dcd.yaml
        #[arg(long)]
        force: bool,
        /// Also write plugins/app.lua (a commented hook stub)
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

    let process_env: std::collections::HashMap<String, String> = std::env::vars().collect();
    let mut sets = cli.sets.clone();
    for image in &cli.images {
        let (logical, tag) = image
            .split_once('=')
            .ok_or_else(|| DcdError::Config(format!("--image `{image}` must be logical=tag")))?;
        sets.push(format!("docker.images.{logical}={tag}"));
    }

    let source = std::fs::read_to_string(&cli.config)
        .map_err(|e| DcdError::Config(format!("cannot read {}: {e}", cli.config.display())))?;
    let stage_name = config::peek_stage(&source, stage_of(&cli.command))?;
    let stdin_document = read_env_stdin(&cli)?;
    let chain_base = match (&cli.env_file, &cli.env_dir) {
        (Some(file), _) => file.clone(),
        (None, Some(dir)) => dir.join(".env"),
        (None, None) => config_dir(&cli.config).join(".env"),
    };
    let base_required = cli.env_file.is_some();
    let resolved = crate::dotenv::resolve(
        &chain_base,
        &stage_name,
        base_required,
        stdin_document.as_deref(),
        &process_env,
    )?;
    let mut cfg = config::load(&source, stage_of(&cli.command), &sets, &resolved.interpolation_env)?;
    let reporter = Reporter::auto(cli.json);

    match &cli.command {
        Command::Deploy { .. } | Command::Rollback { .. } => {
            let run = if matches!(cli.command, Command::Rollback { .. }) {
                Run::Rollback
            } else {
                Run::Deploy
            };
            let plugins = load_plugins(&cfg)?;
            let lua_host = if plugins.is_empty() {
                None
            } else {
                Some(LuaHost::load(&cfg, &plugins).map_err(DcdError::Lua)?)
            };
            if let Some(host) = &lua_host {
                if host.has_hook("configure") {
                    let state = load_state(&cfg.deploy_root)?;
                    let stage = state.stage(&cfg.stage).cloned().unwrap_or_default();
                    host.refresh(&cfg, &stage).map_err(DcdError::Lua)?;
                    let before = host.read_cfg().map_err(DcdError::Lua)?;
                    let configure_host = ConfigureHost {
                        reporter: &reporter,
                        runner: SystemRunner::with_context(resolved.container_env.clone(), cfg.deploy_root.clone()),
                        interpolation_env: resolved.interpolation_env.clone(),
                        deploy_root: cfg.deploy_root.clone(),
                        stage: cfg.stage.clone(),
                    };
                    host.fire(&configure_host, "configure").map_err(DcdError::Lua)?;
                    let after = host.read_cfg().map_err(DcdError::Lua)?;
                    if after != before {
                        cfg = config::from_lua_value(after, &cfg.stage)?;
                    }
                }
            }
            execute(&cfg, &cli, &reporter, run, lua_host.as_ref(), &resolved)
        }
        Command::Status { .. } => status(&cfg, &reporter),
        Command::Tasks { .. } => tasks(&cfg, &reporter),
        Command::Check { .. } => check_report(&cfg, &reporter, &resolved, &chain_base),
        Command::Init { .. } => unreachable!("handled above"),
    }
}

/// The §5.2.5 env observability report: which chain files loaded, what each
/// container class receives (key NAMES only — values are never printed).
fn check_report(
    cfg: &Config,
    reporter: &Reporter,
    resolved: &crate::dotenv::ResolvedEnv,
    chain_base: &Path,
) -> Result<()> {
    for layer in &resolved.layers {
        reporter.log(&format!("env: loaded {} ({} keys)", layer.label, layer.values.len()));
    }
    for path in &resolved.skipped {
        reporter.log(&format!("env: absent {}", path.display()));
    }
    if !resolved.shadowed_keys.is_empty() {
        reporter.log(&format!(
            "env: shadowed by process env: {}",
            resolved.shadowed_keys.join(", ")
        ));
    }

    let run = &cfg.release.run;
    let release_keys =
        crate::dotenv::delivered_keys(&resolved.container_env, &run.env_include, &run.env_exclude, run.env.keys())?;
    reporter.log(&format!("env: release/migrate containers receive: [{}]", release_keys.join(", ")));
    if let Some(workers) = &cfg.workers {
        let template = &workers.template;
        let worker_keys = crate::dotenv::delivered_keys(
            &resolved.container_env,
            &template.env_include,
            &template.env_exclude,
            template.env.keys(),
        )?;
        reporter.log(&format!("env: worker containers receive: [{}]", worker_keys.join(", ")));
    }

    let stray = cfg.deploy_root.join(".env");
    let is_same_file = stray.canonicalize().ok() == chain_base.canonicalize().ok();
    if stray.exists() && !is_same_file {
        reporter.warn(&format!(
            "{} exists but is not part of dcd's chain — compose never reads it (dcd pins compose's --env-file to /dev/null)",
            stray.display()
        ));
    }

    reporter.log(&format!("config ok ({} stage '{}')", cfg.project, cfg.stage));
    Ok(())
}

fn config_dir(config_path: &Path) -> PathBuf {
    match config_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// `--env-stdin` guards run eagerly at dispatch, before the lock (spec §5.2.1):
/// a TTY on stdin or a prompting command without `--yes` refuses immediately.
fn read_env_stdin(cli: &Cli) -> Result<Option<String>> {
    if !cli.env_stdin {
        return Ok(None);
    }
    if std::io::stdin().is_terminal() {
        return Err(DcdError::Config("--env-stdin requires piped input".to_string()));
    }
    let can_prompt = matches!(cli.command, Command::Rollback { .. });
    if can_prompt && !cli.yes {
        return Err(DcdError::Config("--env-stdin consumes stdin; pass -y/--yes".to_string()));
    }
    let document = std::io::read_to_string(std::io::stdin())
        .map_err(|e| DcdError::Config(format!("cannot read --env-stdin document: {e}")))?;
    Ok(Some(document))
}

enum Run {
    Deploy,
    Rollback,
}

fn execute(
    cfg: &Config,
    cli: &Cli,
    reporter: &Reporter,
    run: Run,
    lua: Option<&LuaHost>,
    resolved: &crate::dotenv::ResolvedEnv,
) -> Result<()> {
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
        container_env: resolved.container_env.clone(),
        interpolation_env: resolved.interpolation_env.clone(),
    };

    let system_runner =
        SystemRunner::with_context(resolved.container_env.clone(), cfg.deploy_root.clone());
    let runner: Box<dyn CommandRunner> = if cli.dry_run {
        Box::new(DryRunRunner::new(system_runner))
    } else {
        Box::new(system_runner)
    };
    let fs = SystemFs;

    let mut engine = Engine::new(cfg.clone(), runner.as_ref(), &fs, &clock, reporter, &interrupt, state, opts);
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

fn status(cfg: &Config, reporter: &Reporter) -> Result<()> {
    let state = load_state(&cfg.deploy_root)?;
    let stage = &cfg.stage;
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

fn tasks(cfg: &Config, reporter: &Reporter) -> Result<()> {
    reporter.log(&format!("plan for {} stage '{}':", cfg.project, cfg.stage));
    for step in DEPLOY_STEPS {
        let key = step.replace(':', "_");
        for hook in cfg.hooks.get(&format!("before_{key}")).into_iter().flatten() {
            reporter.plan(&format!("before {step}: {}", describe_hook(hook)));
        }
        reporter.log(&format!("- {step}"));
        for hook in cfg.hooks.get(&format!("after_{key}")).into_iter().flatten() {
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
/// host-level effects (run/files/env) but no release-container operations. Its
/// commands run under the same runner env and cwd as the engine's (spec §6.5).
struct ConfigureHost<'a> {
    reporter: &'a Reporter,
    runner: SystemRunner,
    interpolation_env: std::collections::HashMap<String, String>,
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
        self.runner
            .run(&Argv::of(["sh", "-c", &full]), Access::Mutate, &RunOpts::default())
            .map(|o| o.stdout)
            .map_err(|e| e.to_string())
    }

    fn in_release(&self, _cmd: &str) -> std::result::Result<String, String> {
        Err(self.no_container("in_release"))
    }
    fn exec_in(&self, service: &str, cmd: &str) -> std::result::Result<String, String> {
        let argv = Argv::of(["docker", "exec", service, "sh", "-c", cmd]);
        self.runner
            .run(&argv, Access::Mutate, &RunOpts::default())
            .map(|o| o.stdout)
            .map_err(|e| e.to_string())
    }
    fn docker(&self, args: Vec<String>) -> std::result::Result<String, String> {
        let mut argv = vec!["docker".to_string()];
        argv.extend(args);
        self.runner
            .run(&Argv(argv), Access::Mutate, &RunOpts::default())
            .map(|o| o.stdout)
            .map_err(|e| e.to_string())
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
        self.interpolation_env.get(name).cloned()
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
}

const SCAFFOLD: &str = r#"version: 1
project: myapp
network: myapp_net
# registry + deploy_root default from $REGISTRY / $CI_REGISTRY_IMAGE and $DEPLOY_ROOT.
# ${VAR} resolves from the process env layered over the dotenv chain next to this file
# (.env, .env.local, .env.<stage>, .env.<stage>.local); every chain-defined key is also
# delivered to the app/worker containers — dcd writes no env file on the server.

# Images dcd pulls/runs, plus long-lived side containers. Block style for ${VAR} values.
docker:
  images:
    app: ${APP_TAG}
  services:
    nginx:
      image: ~          # pinned in compose; dcd never recreates it
      container: myapp-nginx
      recreate: never
      wait: { exec_in: myapp-nginx, cmd: 'test -f /var/run/nginx.pid', retries: 30 }

compose:
  files: [docker-compose.prod.yml]
  env:
    REGISTRY: ${REGISTRY}
    APP_TAG: ${APP_TAG}

# The app — built, health-checked, then cut over to.
release:
  image: app
  container_prefix: myapp-app
  run:
    network_alias: app
    env: { TZ: UTC }
  healthcheck:
    exec_in: myapp-nginx
    cmd: 'curl -sf http://{container}:8080/health'   # {container}, never the alias
  # migrate: { before: '...', after: '...' }   # optional, expand-contract

cutover:
  backend_port: 8080
  reload: { exec_in: myapp-nginx, cmd: 'nginx -s reload' }

stages:
  prod:
    host: prod.example.internal   # dcd refuses to run on the wrong host
"#;

const PLUGIN_STUB: &str = r#"-- Plugins (loaded at startup) register tasks and hooks.
--
-- Adjust the config before the deploy, based on runtime truths. cfg is mutable: just
-- assign to it — the change flows back into the deploy (no helper function):
-- configure(function(ctx)
--   if ctx.env('CANARY') == '1' then ctx.cfg.retention.keep_releases = 5 end
-- end)
--
-- ctx available inside hooks:
--   effects: run, in_release, exec_in, docker, compose, cp_from_release, cp_to_release
--   files:   read_file, write_file, file_exists, env
--   data:    cfg (config) and state (current + history) are LIVE — assign to them and the
--            engine reads it back; vars is scratch shared across hooks
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
    fn env_stdin_on_a_prompting_command_without_yes_refuses_eagerly() {
        // TC-040: cargo's test stdin is piped (not a TTY), so this exercises the
        // prompting-command guard specifically.
        let cli = Cli::parse_from(["dcd", "rollback", "prod", "--env-stdin"]);
        let err = read_env_stdin(&cli).unwrap_err();
        assert!(err.to_string().contains("pass -y/--yes"), "got: {err}");
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn check_report_prints_key_names_and_never_values() {
        // TC-039: the env observability report leaks no value bytes.
        let config_env: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let cfg = config::load(SCAFFOLD_TEST_CONFIG, Some("prod"), &[], &config_env).unwrap();
        let documents = vec![(
            ".env".to_string(),
            "DATABASE_URL=postgres://app:sup3rs3cret@db/app\nAPP_SECRET=hunter2\n".to_string(),
        )];
        let resolved =
            crate::dotenv::resolve_documents(&documents, &config_env, Vec::new()).unwrap();
        let reporter = Reporter::capture(crate::ui::Mode::Plain);
        check_report(&cfg, &reporter, &resolved, Path::new(".")).unwrap();

        let output = reporter.lines().join("\n");
        assert!(output.contains("APP_SECRET"), "key names are printed: {output}");
        assert!(output.contains("DATABASE_URL"), "key names are printed: {output}");
        assert!(!output.contains("hunter2"), "value leaked: {output}");
        assert!(!output.contains("sup3rs3cret"), "value leaked: {output}");
    }

    const SCAFFOLD_TEST_CONFIG: &str = r#"
version: 1
project: demo
network: demo_net
docker:
  images: { app: app-1 }
  services:
    nginx: { container: demo-nginx, recreate: never }
compose:
  files: [base.yml]
release:
  image: app
  container_prefix: demo-app
  healthcheck: { exec_in: demo-nginx, cmd: 'curl {container}' }
cutover:
  backend_port: 8080
  reload: { exec_in: demo-nginx, cmd: 'nginx -s reload' }
stages:
  prod: {}
"#;

    #[test]
    fn scaffold_parses_and_validates() {
        let env: std::collections::HashMap<String, String> =
            [("REGISTRY", "reg"), ("APP_TAG", "t")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let cfg = config::load(SCAFFOLD, Some("prod"), &[], &env).unwrap();
        assert_eq!(cfg.project, "myapp");
    }
}
