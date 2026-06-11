//! CLI surface (spec §8): parse, then orchestrate host guard, lock, state I/O,
//! and the engine. Command bodies are thin; the work lives in the typed modules.

use std::io::{IsTerminal, Write};
use std::path::Path;

use clap::{Parser, Subcommand};

use crate::config::{self, Loaded};
use crate::effects::{CommandRunner, DryRunRunner, SystemClock, SystemFs, SystemRunner};
use crate::engine::{Engine, Options, DEPLOY_STEPS};
use crate::error::{DcdError, Result};
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
    let loaded = config::load(&source, stage_of(&cli.command), &sets, &env)?;
    let reporter = Reporter::auto(cli.json, loaded.redactor.clone());

    match &cli.command {
        Command::Deploy { .. } => execute(&loaded, &cli, &reporter, Run::Deploy),
        Command::Rollback { .. } => execute(&loaded, &cli, &reporter, Run::Rollback),
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

fn execute(loaded: &Loaded, cli: &Cli, reporter: &Reporter, run: Run) -> Result<()> {
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

const PLUGIN_STUB: &str = r#"-- Custom tasks and hooks (loaded at startup).
-- task('myapp:warmup', function(ctx)
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
