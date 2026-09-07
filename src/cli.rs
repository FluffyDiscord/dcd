//! CLI surface (spec §8): parse, then orchestrate host guard, lock, state I/O,
//! and the engine. Command bodies are thin; the work lives in the typed modules.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::{Parser, Subcommand};

use crate::compose::{self, ComposeModel};
use crate::config::{self as config, Config};
use crate::effects::{
    Access, Argv, CommandRunner, DryRunRunner, FileSystem, RunOpts, SshFs, SshRunner, SystemClock, SystemFs,
    SystemRunner,
};
use crate::ssh::SshTarget;
use crate::engine::{Engine, Options, DEPLOY_STEPS};
use crate::error::{DcdError, Result};
use crate::lock::{LeasedLock, StageLock};
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
    long_about = "dcd runs a zero-downtime red-black Docker deploy from a dcd.yaml: it \
creates the new container next to the live one, health-checks it, flips the router's \
upstream over to it, then drains the old one.\n\n\
It runs HERE — on a CI runner or your workstation — and drives the target over ssh; \
nothing is installed there. Your compose file declares the containers, dcd.yaml the \
orchestration. Omit `ssh:` to drive a local Docker socket instead.\n\n\
Env comes from a Symfony-style dotenv chain next to the config (.env, .env.local, \
.env.<stage>, .env.<stage>.local — later wins, real env wins over all): chain keys reach \
containers as bare `-e KEY`, with the values riding a document on ssh stdin, so no value \
ever enters an argv on either machine and dcd writes no env file. \
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
    #[arg(short = 'V', long = "version", global = true, action = clap::ArgAction::Version)]
    version: Option<bool>,
    /// Trace every command dcd runs: its argv, exit code, elapsed, and output
    #[arg(short = 'v', long, global = true)]
    verbose: bool,
    /// Path to the config file
    #[arg(short, long, global = true, default_value = "dcd.yaml")]
    config: std::path::PathBuf,
    /// SSH target to deploy to, overriding `ssh:` (user@host, or an ~/.ssh/config Host alias)
    #[arg(long, global = true, value_name = "TARGET")]
    ssh: Option<String>,
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
    /// Pin a compose service's image for this run (repeatable)
    #[arg(long = "image", global = true, value_name = "SERVICE=REF")]
    images: Vec<String>,
    /// Directory of the dotenv chain (.env, .env.local, .env.<stage>[.local]);
    /// defaults to the config file's directory
    #[arg(long, global = true, value_name = "PATH")]
    env_dir: Option<PathBuf>,
    /// Chain base file (Symfony loadEnv semantics): loads <path>[.dist],
    /// <path>.local, <path>.<stage>, <path>.<stage>.local; the base must exist
    #[arg(long, global = true, value_name = "PATH", conflicts_with = "env_dir")]
    env_file: Option<PathBuf>,
    /// Read a dotenv document from stdin (nothing lands on disk). On its own it is
    /// the WHOLE chain — no .env is discovered next to the config; with --env-dir or
    /// --env-file it is the highest layer. Needs piped input, and -y when prompting
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
    /// Force a stuck deploy to done: accept the incomplete release and clear the lock.
    ///
    /// The last resort for when `deploy --resume` cannot finish. It marks the recorded
    /// cutover-pending release active and current, and removes the stage lock even while
    /// another dcd still holds it. It is a state repair only: no container is started,
    /// stopped, or removed, no migrations run, no workers are recreated, and no hooks
    /// fire — the next deploy runs fresh and cleans up what is left behind.
    Unlock {
        /// Stage to unlock (omit if the config defines exactly one)
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
    /// Reclaim disk: remove image versions past the retention counts.
    ///
    /// By default it only removes tags dcd itself recorded — the release history and
    /// the pull ledger — which is the same authority a deploy's own cleanup has.
    /// `--all` additionally asks Docker what sits in this config's repositories and
    /// offers tags no stage records: images pulled before dcd kept a ledger, left by
    /// a reset state file, or pulled by hand. That path infers ownership, so it lists
    /// every candidate and asks first. Pair either with --dry-run to change nothing.
    Gc {
        /// Stage to collect (omit if the config defines exactly one)
        stage: Option<String>,
        /// Also offer host tags no stage records (asks before removing)
        #[arg(long)]
        all: bool,
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
        /// Derive the config from an existing compose file instead of the generic scaffold
        #[arg(long = "from-compose", value_name = "FILE")]
        from_compose: Option<PathBuf>,
    },
    /// Print a JSON Schema for dcd.yaml, for editor completion and inline validation.
    Schema,
}

pub fn run() -> Result<()> {
    dispatch(Cli::parse())
}

/// `--image` is global for convenience, but it only chooses an image on the
/// commands that deploy or validate one. On `gc` a pin would change which tags
/// count as in-use and could make the live image prunable; on `rollback` the
/// recorded ref wins and the flag would be silently ignored. Refusing beats both.
fn reject_pins_where_they_do_nothing(cli: &Cli) -> Result<()> {
    if cli.images.is_empty() {
        return Ok(());
    }
    let applies = matches!(cli.command, Command::Deploy { .. } | Command::Check { .. });
    if applies {
        return Ok(());
    }
    let reason = match cli.command {
        Command::Rollback { .. } => " — rollback replays the image it recorded",
        _ => "",
    };
    Err(DcdError::Config(format!(
        "--image applies to `deploy` and `check` only{reason}"
    )))
}

fn stage_of(command: &Command) -> Option<&str> {
    match command {
        Command::Deploy { stage }
        | Command::Rollback { stage }
        | Command::Unlock { stage }
        | Command::Status { stage }
        | Command::Tasks { stage }
        | Command::Gc { stage, .. }
        | Command::Check { stage } => stage.as_deref(),
        Command::Init { .. } | Command::Schema => None,
    }
}

fn dispatch(cli: Cli) -> Result<()> {
    // Before the early returns, or `dcd schema --image bogus` succeeds while
    // `dcd check --image bogus` errors.
    image_pins(&cli.images)?;
    reject_pins_where_they_do_nothing(&cli)?;

    if let Command::Init { force, with_plugin, from_compose } = &cli.command {
        return init(&cli.config, *force, *with_plugin, from_compose.as_deref());
    }
    if matches!(cli.command, Command::Schema) {
        return schema();
    }

    let process_env: std::collections::HashMap<String, String> = std::env::vars().collect();
    let sets = cli.sets.clone();

    let source = std::fs::read_to_string(&cli.config)
        .map_err(|e| DcdError::Config(format!("cannot read {}: {e}", cli.config.display())))?;
    let config_dir = cli.config.parent().unwrap_or(Path::new(".")).to_path_buf();
    let stage_name = config::peek_stage(&source, stage_of(&cli.command))?;
    let stdin_document = read_env_stdin(&cli)?;
    let chain_base = chain_base(&cli);
    let base_required = cli.env_file.is_some();
    let resolved = crate::dotenv::resolve(
        chain_base.as_deref(),
        &stage_name,
        base_required,
        stdin_document.as_deref(),
        &process_env,
    )?;
    let mut cfg = config::load(&source, stage_of(&cli.command), &sets, &resolved.interpolation_env)?;
    materialise_release_run(&mut cfg)?;
    resolve_compose_sources(&mut cfg, &config_dir);
    let reporter = Reporter::auto(cli.json, cli.verbose);

    // Spec §2.7: over ~107 bytes ssh FAILS rather than degrading to an
    // unmultiplexed connection, so the length is checked before any connection is
    // attempted.
    if let Some(target) = ssh_target(&cfg, &cli) {
        require_option_free_destination(&target)?;
        require_control_path_fits(&target)?;
    }
    dispatch_command(&cfg, &cli, &reporter, &resolved, chain_base.as_deref(), config_dir)
}

/// `ssh:` is already refused by config validation; `--ssh` never passes through it,
/// and it lands in the same positional slot, where a leading `-` turns the target
/// into an ssh option that runs on the deploying machine.
fn require_option_free_destination(target: &SshTarget) -> Result<()> {
    if !SshTarget::is_option_like_destination(target.target()) {
        return Ok(());
    }
    Err(DcdError::Config(format!(
        "--ssh '{}' starts with '-': ssh reads it as an option, not a host, and an option like \
         -oProxyCommand= runs on the deploying machine",
        target.target()
    )))
}

fn require_control_path_fits(target: &SshTarget) -> Result<()> {
    prepare_private_directory()?;
    if target.control_path_fits() {
        return Ok(());
    }
    Err(DcdError::Config(format!(
        "the ssh ControlPath expands to {} bytes, over the {} the socket allows — set XDG_RUNTIME_DIR to a shorter directory",
        target.expanded_control_path_bytes(),
        SshTarget::max_control_path_bytes()
    )))
}

fn dispatch_command(
    cfg: &Config,
    cli: &Cli,
    reporter: &Reporter,
    resolved: &crate::dotenv::ResolvedEnv,
    chain_base: Option<&Path>,
    config_dir: PathBuf,
) -> Result<()> {
    let mut cfg = cfg.clone();
    match &cli.command {
        Command::Deploy { .. } | Command::Rollback { .. } => {
            let run = if matches!(cli.command, Command::Rollback { .. }) {
                Run::Rollback
            } else {
                Run::Deploy
            };
            let plugins = load_plugins(&cfg, &config_dir)?;
            let lua_host = if plugins.is_empty() {
                None
            } else {
                Some(LuaHost::load(&cfg, &plugins).map_err(DcdError::Lua)?)
            };
            if let Some(host) = &lua_host {
                if host.has_hook("configure") {
                    let state = load_state(engine_fs(&cfg, cli, resolved).as_ref(), &cfg.deploy_root)?;
                    let stage = state.stage(&cfg.stage).cloned().unwrap_or_default();
                    host.refresh(&cfg, &stage).map_err(DcdError::Lua)?;
                    let before = host.read_cfg().map_err(DcdError::Lua)?;
                    let configure_host = ConfigureHost {
                        reporter,
                        runner: engine_runner(&cfg, cli, resolved),
                        fs: engine_fs(&cfg, cli, resolved),
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
            execute(&cfg, cli, reporter, run, lua_host.as_ref(), resolved)
        }
        Command::Unlock { .. } => unlock(&cfg, cli, reporter, resolved),
        Command::Status { .. } => status(&cfg, cli, resolved, reporter),
        Command::Gc { all, .. } => gc(&cfg, cli, reporter, *all, resolved),
        Command::Tasks { .. } => tasks(&cfg, reporter),
        Command::Check { .. } => {
            check_report(&cfg, reporter, resolved, chain_base)?;
            // Every failure `check` exists to catch, before a deploy touches
            // anything: plugin sources load, the model resolves, every service
            // reference exists, every health gate is declared.
            let plugins = load_plugins(&cfg, &config_dir)?;
            if !plugins.is_empty() {
                LuaHost::load(&cfg, &plugins).map_err(DcdError::Lua)?;
                reporter.log(&format!("plugins: {} loaded", plugins.len()));
            }
            let model = resolve_compose_model(&cfg, cli, resolved)?;
            reporter.log("compose: every referenced service exists and declares a health gate");
            warn_compose_shape(&cfg, &model, reporter);
            Ok(())
        }
        Command::Init { .. } | Command::Schema => unreachable!("handled above"),
    }
}

/// The §5.2.5 env observability report: which chain files loaded, what each
/// container class receives (key NAMES only — values are never printed).
fn check_report(
    cfg: &Config,
    reporter: &Reporter,
    resolved: &crate::dotenv::ResolvedEnv,
    chain_base: Option<&Path>,
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

    let empty = indexmap::IndexMap::new();
    let (include, exclude, explicit) = match &cfg.release.run {
        Some(run) => (run.env_include.clone(), run.env_exclude.clone(), run.env.clone()),
        None => (Vec::new(), Vec::new(), empty),
    };
    let release_keys =
        crate::dotenv::delivered_keys(&resolved.container_env, &include, &exclude, explicit.keys())?;
    reporter.log(&format!(
        "env: release/migrate/worker containers receive: [{}]",
        release_keys.join(", ")
    ));

    // The v1 allow-list became a blanket pass, so the key names have to stay
    // visible: a typo'd ${...} in any compose file now interpolates a real value.
    let compose_keys: Vec<&str> = resolved.container_env.keys().map(String::as_str).collect();
    reporter.log(&format!("env: compose receives: [{}]", compose_keys.join(", ")));

    // Local only. `deploy_root` is on the TARGET under ssh, and probing it would
    // both address the wrong machine and make `check` — documented as opening no
    // connection — pay a ConnectTimeout in a CI lint job with no ssh access.
    let stray = cfg.deploy_root.join(".env");
    let base_real = chain_base.and_then(|base| base.canonicalize().ok());
    let is_same_file = base_real.is_some() && stray.canonicalize().ok() == base_real;
    if cfg.ssh.is_none() && stray.exists() && !is_same_file {
        reporter.warn(&format!(
            "{} exists but is not part of dcd's chain — compose never reads it (dcd pins compose's --env-file to /dev/null)",
            stray.display()
        ));
    }

    reporter.log(&format!("config ok ({} stage '{}')", cfg.project, cfg.stage));
    Ok(())
}

/// Where the dotenv chain hangs from, or `None` when no file source was named
/// (spec §5.2.1): `--env-stdin` alone must not absorb an application `.env` that
/// happens to sit beside `dcd.yaml`. `--env-dir`/`--env-file` combine both.
fn chain_base(cli: &Cli) -> Option<PathBuf> {
    match (&cli.env_file, &cli.env_dir) {
        (Some(file), _) => Some(file.clone()),
        (None, Some(dir)) => Some(dir.join(".env")),
        (None, None) if cli.env_stdin => None,
        (None, None) => Some(config_dir(&cli.config).join(".env")),
    }
}

fn config_dir(config_path: &Path) -> PathBuf {
    match config_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// `--env-stdin` guards run eagerly at dispatch, before the lock (spec §5.2.1):
/// a TTY on stdin or a prompting command without `--yes` refuses immediately.
/// The `--env-stdin` policy, as a pure function of the flags and whether stdin is
/// a terminal. Split out from the read so it is testable: asserting it through
/// `read_env_stdin` meant reading PROCESS-GLOBAL stdin, which made `cargo test`
/// fail from a terminal and hang behind an IDE runner that holds stdin open — and
/// when it did pass, the terminal branch short-circuited so the `-y` guard below
/// was never actually exercised.
fn env_stdin_refusal(cli: &Cli, stdin_is_terminal: bool) -> Option<DcdError> {
    if !cli.env_stdin {
        return None;
    }
    if stdin_is_terminal {
        return Some(DcdError::Config("--env-stdin requires piped input".to_string()));
    }
    let can_prompt = matches!(cli.command, Command::Rollback { .. } | Command::Unlock { .. });
    if can_prompt && !cli.yes {
        return Some(DcdError::Config("--env-stdin consumes stdin; pass -y/--yes".to_string()));
    }
    None
}

fn read_env_stdin(cli: &Cli) -> Result<Option<String>> {
    if !cli.env_stdin {
        return Ok(None);
    }
    if let Some(refusal) = env_stdin_refusal(cli, std::io::stdin().is_terminal()) {
        return Err(refusal);
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
    host_guard(cfg, cli)?;

    let clock = SystemClock;
    let holder = format!("{} pid {} since {}", hostname(), std::process::id(), hhmmss(now_epoch()));
    let _lock = acquire_lock(cfg, cli, resolved, &holder, reporter)?;

    let state = load_state(engine_fs(cfg, cli, resolved).as_ref(), &cfg.deploy_root)?;
    let interrupt = Interrupt::install();
    let opts = engine_options(cfg, cli, resolved);
    let runner = engine_runner(cfg, cli, resolved);
    let fs = engine_fs(cfg, cli, resolved);

    let model = resolve_compose_model(cfg, cli, resolved)?;
    let mut engine = Engine::new(cfg.clone(), runner.as_ref(), fs.as_ref(), &clock, reporter, &interrupt, state, opts, model);
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

/// Standalone retention (spec §7.13). Takes the stage lock like a deploy so it can
/// never race one, and prints the whole plan before touching anything.
fn gc(
    cfg: &Config,
    cli: &Cli,
    reporter: &Reporter,
    sweep_all: bool,
    resolved: &crate::dotenv::ResolvedEnv,
) -> Result<()> {
    host_guard(cfg, cli)?;

    let clock = SystemClock;
    let holder = format!("{} pid {} since {}", hostname(), std::process::id(), hhmmss(now_epoch()));
    let _lock = acquire_lock(cfg, cli, resolved, &holder, reporter)?;

    let state = load_state(engine_fs(cfg, cli, resolved).as_ref(), &cfg.deploy_root)?;
    let interrupt = Interrupt::install();
    let fs = engine_fs(cfg, cli, resolved);
    let runner = engine_runner(cfg, cli, resolved);
    let mut engine = Engine::new(
        cfg.clone(),
        runner.as_ref(),
        fs.as_ref(),
        &clock,
        reporter,
        &interrupt,
        state,
        engine_options(cfg, cli, resolved),
        resolve_compose_model(cfg, cli, resolved)?,
    );

    let plan = engine.gc_plan(sweep_all)?;
    for (tag, reason) in &plan.protected {
        reporter.log(&format!("keeping {tag} — {reason}"));
    }
    for tag in &plan.recorded {
        reporter.plan(&format!("remove {tag} (past its retention count)"));
    }
    for tag in &plan.orphans {
        reporter.plan(&format!("remove {tag} (on the host, recorded by no stage)"));
    }
    if plan.is_empty() {
        reporter.log(&format!("{}: nothing to reclaim", cfg.stage));
        return Ok(());
    }
    if cli.dry_run {
        return Ok(());
    }
    if !plan.orphans.is_empty() {
        let prompt = format!("Remove {} image(s) listed above from {}?", plan.removals().len(), cfg.stage);
        if !confirm(&prompt, cli.yes)? {
            return Err(DcdError::PreCutover("gc declined (pass --yes to confirm)".to_string()));
        }
    }
    let removed = engine.gc(&plan)?;
    reporter.log(&format!("{}: reclaimed {removed} image(s)", cfg.stage));
    Ok(())
}

fn engine_options(cfg: &Config, cli: &Cli, resolved: &crate::dotenv::ResolvedEnv) -> Options {
    Options {
        dry_run: cli.dry_run,
        sleep_enabled: true,
        reason: cli.reason.clone(),
        container_env: resolved.container_env.clone(),
        interpolation_env: resolved.interpolation_env.clone(),
        remote: ssh_target(cfg, cli).is_some(),
    }
}

/// The transport is a runner swap: with `ssh:` set every command is wrapped for
/// the target, without it nothing changes from v1 (ADR-014).
fn engine_runner(cfg: &Config, cli: &Cli, resolved: &crate::dotenv::ResolvedEnv) -> Box<dyn CommandRunner> {
    match ssh_target(cfg, cli) {
        Some(target) => {
            let runner = SshRunner::new(target, resolved.container_env.clone(), cfg.deploy_root.clone());
            if cli.dry_run {
                Box::new(DryRunRunner::new(runner))
            } else {
                Box::new(runner)
            }
        }
        None => {
            let runner = SystemRunner::with_context(resolved.container_env.clone(), cfg.deploy_root.clone());
            if cli.dry_run {
                Box::new(DryRunRunner::new(runner))
            } else {
                Box::new(runner)
            }
        }
    }
}

/// The filesystem follows the same swap: with `ssh:` the five operations dcd owns
/// run on the target, so `deploy_root` means the same thing to both.
fn engine_fs(cfg: &Config, cli: &Cli, resolved: &crate::dotenv::ResolvedEnv) -> Box<dyn FileSystem> {
    match ssh_target(cfg, cli) {
        Some(target) => Box::new(SshFs::new(SshRunner::new(
            target,
            resolved.container_env.clone(),
            cfg.deploy_root.clone(),
        ))),
        None => Box::new(SystemFs),
    }
}

/// `--ssh` wins over `ssh:`; neither means everything runs locally, exactly as v1
/// did — which is what the unit suite and `tests/e2e.rs` drive.
fn ssh_target(cfg: &Config, cli: &Cli) -> Option<SshTarget> {
    let target = cli.ssh.clone().or_else(|| cfg.ssh.clone())?;
    Some(SshTarget::new(target, control_path()))
}

/// dcd picks the multiplexing socket itself, and keeps it short: over ~107 bytes
/// ssh fails outright rather than degrading to an unmultiplexed connection.
fn control_path() -> PathBuf {
    private_directory().join("cm-%C")
}

/// Everything dcd generates for itself — the mux socket (spec §2.7) and the
/// rendered `release.run` document — lives here, and nowhere a second user can
/// reach. `$XDG_RUNTIME_DIR` is already per-user and `0700`; its usual absence in
/// CI must not land in a shared `/tmp`, where every name dcd writes is derivable
/// from the project and the stage. `prepare_private_directory` is what enforces that.
fn private_directory() -> PathBuf {
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime).join("dcd");
    }
    match std::env::var("HOME") {
        Ok(home) => PathBuf::from(home).join(".dcd"),
        Err(_) => std::env::temp_dir().join("dcd"),
    }
}

/// Created and locked to `0700` before anything is written into it: a mux socket in
/// a directory someone else can write is a socket dcd may be talked into using, and
/// a compose document someone else can write is a container they choose, started on
/// the target. The symlink check is what makes the `/tmp` fallback safe — without
/// it, a pre-planted `/tmp/dcd -> ~victim/.ssh` is followed by both the `chmod` and
/// every write that follows.
fn prepare_private_directory() -> Result<()> {
    let dir = private_directory();
    std::fs::create_dir_all(&dir)
        .map_err(|e| DcdError::Config(format!("cannot create {}: {e}", dir.display())))?;

    let entry = std::fs::symlink_metadata(&dir)
        .map_err(|e| DcdError::Config(format!("cannot inspect {}: {e}", dir.display())))?;
    if !entry.is_dir() {
        return Err(DcdError::Config(format!(
            "{} is a symlink, not a directory: dcd writes its own files there and will not follow it",
            dir.display()
        )));
    }

    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| DcdError::Config(format!("cannot make {} private: {e}", dir.display())))
}

/// The §5.1/§7.0 shape warnings: everything dcd can see in the resolved model that
/// will not fail until the deploy is already running. dcd uploads the compose
/// DOCUMENTS and nothing they reference, and Docker answers a missing bind source
/// by silently creating an empty directory — so a router whose config never
/// arrived starts cleanly and serves nothing.
fn warn_compose_shape(cfg: &Config, model: &ComposeModel, reporter: &Reporter) {
    let release = cfg.release.service_name(&cfg.project);

    // `compose config` absolutizes a relative bind source against the compose
    // file's own directory — so what identifies one is that it resolves INSIDE the
    // checkout. Those paths do not exist on the target, and Docker answers a
    // missing bind source by silently creating an empty directory.
    let checkout = std::env::current_dir().ok();
    if cfg.ssh.is_some() {
        if let Some(checkout) = &checkout {
            for (name, service) in &model.services {
                for source in service.bind_sources() {
                    if !Path::new(&source).starts_with(checkout) {
                        continue;
                    }
                    let relative = Path::new(&source).strip_prefix(checkout).unwrap_or(Path::new(&source));
                    // dcd writes the upstream file itself, and `directories:` is
                    // exactly the knob for "pre-create this on the target" — so
                    // neither is a path the operator has been left to place.
                    if relative == cfg.cutover.upstream_file {
                        continue;
                    }
                    if cfg.directories.iter().any(|directory| relative.starts_with(&directory.path)) {
                        continue;
                    }
                    reporter.warn(&format!(
                        "{name} bind-mounts {}, which dcd does not upload — it must already exist under {} on the target, or Docker will silently mount an empty directory",
                        relative.display(),
                        cfg.deploy_root.display()
                    ));
                }
            }
        }
    }

    if let Some(service) = model.service(&release) {
        if !service.ports.is_empty() {
            reporter.warn(&format!(
                "{release} declares ports: — the release is reached through {}, and a published port would collide between red and black",
                cfg.cutover.service
            ));
        }
        if !service.profiles.iter().any(|profile| cfg.compose.profiles.contains(profile)) {
            reporter.warn(&format!(
                "{release} declares no profile in {:?} — a hand-run `docker compose up` would start a second copy beside the release dcd deploys",
                cfg.compose.profiles
            ));
        }
    }

    let upstream = cfg.deploy_root.join(&cfg.cutover.upstream_file);
    let router = model.service(&cfg.cutover.service);
    let mounts_upstream = router
        .is_some_and(|service| upstream_is_mounted(&upstream, &cfg.cutover.upstream_file, &service.bind_sources()));
    if !mounts_upstream {
        reporter.warn(&format!(
            "{} does not bind-mount {} — dcd writes the upstream file on the target, but only your compose file can put it inside the router",
            cfg.cutover.service,
            cfg.cutover.upstream_file.display()
        ));
    }
}

/// Whether the router mounts a path the upstream file lands inside. Three shapes count,
/// and the third is the one to reach for: dcd writes the upstream file staged-then-renamed
/// (`ssh.rs::write_file_script`), so mounting the FILE pins the original inode and the
/// router reloads the config it already had — mounting the directory above it is what
/// survives. Split out from `warn_compose_shape` so the matching is covered without Docker.
fn upstream_is_mounted(upstream: &Path, upstream_file: &Path, bind_sources: &[String]) -> bool {
    bind_sources.iter().any(|source| {
        upstream.ends_with(source.trim_start_matches("./"))
            || source.ends_with(&upstream_file.display().to_string())
            || (Path::new(source).is_absolute() && upstream.starts_with(source))
    })
}

/// The escape hatch (spec §4.4): promote the stuck release and drop the stage lock —
/// deliberately overriding a live flock, since the whole point is to unstick a deploy that
/// will never release it. State and lock files only; loads no plugins, runs no Docker.
fn unlock(cfg: &Config, cli: &Cli, reporter: &Reporter, resolved: &crate::dotenv::ResolvedEnv) -> Result<()> {
    host_guard(cfg, cli)?;

    let stage = cfg.stage.clone();
    let state = load_state(engine_fs(cfg, cli, resolved).as_ref(), &cfg.deploy_root)?;
    let pending = state
        .stage(&stage)
        .and_then(|s| s.newest_cutover_pending())
        .map(|release| release.container.clone());

    // The lock lives wherever the deploy runs. Probing the local filesystem for a
    // remote stage sees nothing, reports success, and can delete a same-named path
    // on the workstation.
    let held = match ssh_target(cfg, cli) {
        Some(target) => LeasedLock::is_held(&target, &cfg.deploy_root, &stage)
            .then(|| LeasedLock::holder(&target, &cfg.deploy_root, &stage)),
        None => StageLock::is_held(&cfg.deploy_root, &stage)
            .then(|| StageLock::holder(&cfg.deploy_root, &stage).unwrap_or_else(|| "unknown holder".to_string())),
    };
    if let Some(holder) = held {
        reporter.warn(&format!(
            "{stage} is locked by a RUNNING dcd ({holder}) — clearing it lets a second deploy start alongside that one, and its next state write would overwrite this unlock"
        ));
    }

    let prompt = match &pending {
        Some(container) => format!("Mark {container} as the successful release on {stage} and clear the lock?"),
        None => format!("No incomplete release on {stage} — clear the stage lock anyway?"),
    };
    if !confirm(&prompt, cli.yes)? {
        return Err(DcdError::PreCutover("unlock declined (pass --yes to confirm)".to_string()));
    }

    let clock = SystemClock;
    let fs = engine_fs(cfg, cli, resolved);
    let interrupt = Interrupt::install();
    let runner = engine_runner(cfg, cli, resolved);
    let mut engine = Engine::new(
        cfg.clone(),
        runner.as_ref(),
        fs.as_ref(),
        &clock,
        reporter,
        &interrupt,
        state,
        engine_options(cfg, cli, resolved),
        // Deliberately empty: spec §4.4 makes `unlock` a pure state repair that
        // runs no Docker command, and `Engine::unlock` reads no container fact.
        // Resolving the model here would run `docker compose config` and full
        // model validation — locking the operator out of the escape hatch for
        // exactly the compose drift that stranded the deploy.
        ComposeModel::default(),
    );
    match engine.unlock()? {
        Some(container) => reporter.log(&format!("{stage}: {container} is now active and current")),
        None => reporter.log(&format!("{stage}: no incomplete release to promote — state untouched")),
    }

    clear_lock(cfg, cli, reporter, &stage)
}

fn clear_lock(cfg: &Config, cli: &Cli, reporter: &Reporter, stage: &str) -> Result<()> {
    if cli.dry_run {
        for path in StageLock::paths(&cfg.deploy_root, stage) {
            reporter.plan(&format!("remove lock {}", path.display()));
        }
        return Ok(());
    }
    let removed = match ssh_target(cfg, cli) {
        Some(target) => LeasedLock::force_release(&target, &cfg.deploy_root, stage)?,
        None => StageLock::force_release(&cfg.deploy_root, stage)?,
    };
    if removed.is_empty() {
        reporter.log(&format!("{stage}: no lock file was present"));
    } else {
        reporter.log(&format!("{stage}: lock cleared"));
    }
    Ok(())
}

fn status(cfg: &Config, cli: &Cli, resolved: &crate::dotenv::ResolvedEnv, reporter: &Reporter) -> Result<()> {
    let state = load_state(engine_fs(cfg, cli, resolved).as_ref(), &cfg.deploy_root)?;
    let stage = &cfg.stage;
    match state.stage(stage) {
        None => reporter.log(&format!("{stage}: no deploys recorded")),
        Some(s) => {
            reporter.log(&format!("{stage}: current = {}", s.current.as_deref().unwrap_or("none")));
            if let Some(pending) = s.cutover_pending() {
                reporter.warn(&format!(
                    "incomplete release {} — run `dcd deploy --resume {stage}`, `dcd rollback {stage}`, or `dcd unlock {stage}` to accept it as-is",
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

/// INV-8, evaluated where the deploy will actually land. SSH makes this guard
/// worth more, not less: an ssh alias can be repointed and a stage copy-pasted.
/// `deploy_root` must exist before the lock: the lock's `flock` cannot create its
/// file in a missing directory, and the ENOENT would otherwise be reported as
/// "another deploy holds this stage" on a virgin host (spec §2.5).
/// `--dry-run` must touch nothing: spec §2.4 and IT-013 say it takes no lock and
/// leaves `deploy_root` empty, and both README and AGENTS.md tell operators to run
/// it against production as a validation step. It reports a held lock instead, so
/// the plan is still labelled when it may be stale.
fn acquire_lock(
    cfg: &Config,
    cli: &Cli,
    resolved: &crate::dotenv::ResolvedEnv,
    holder: &str,
    reporter: &Reporter,
) -> Result<Option<StageLock>> {
    if cli.dry_run {
        let held = match ssh_target(cfg, cli) {
            Some(target) => LeasedLock::is_held(&target, &cfg.deploy_root, &cfg.stage),
            None => StageLock::is_held(&cfg.deploy_root, &cfg.stage),
        };
        if held {
            reporter.warn(&format!("a deploy holds {}; this plan may be stale", cfg.stage));
        }
        return Ok(None);
    }

    let fs = engine_fs(cfg, cli, resolved);
    fs.create_dir_all(&cfg.deploy_root).map_err(|e| {
        DcdError::Config(format!("deploy_root {} is not usable: {e}", cfg.deploy_root.display()))
    })?;
    let lock = match ssh_target(cfg, cli) {
        Some(target) => crate::lock::LeasedLock::acquire(&target, &cfg.deploy_root, &cfg.stage, holder)?,
        None => StageLock::acquire(&cfg.deploy_root, &cfg.stage, holder)?,
    };
    Ok(Some(lock))
}

fn hostname() -> String {
    gethostname::gethostname().to_string_lossy().to_string()
}

fn host_guard(cfg: &config::Config, cli: &Cli) -> Result<()> {
    if cfg.host.is_none() {
        return Ok(());
    }
    let actual = match ssh_target(cfg, cli) {
        Some(target) => remote_hostname(&target)?,
        None => gethostname::gethostname().to_string_lossy().to_string(),
    };
    host_check(cfg.host.as_deref(), &actual, &cfg.stage)
}

/// `hostname -f` where it exists, `hostname` where it does not — most Linux hosts
/// report a short name, while every example config names an FQDN.
fn remote_hostname(target: &SshTarget) -> Result<String> {
    let runner = SshRunner::new(target.clone(), Default::default(), PathBuf::from("/"));
    let argv = Argv::of(["sh", "-c", "hostname -f 2>/dev/null || hostname"]);
    let out = runner
        .run(&argv, Access::Read, &RunOpts::default())
        .map_err(|e| DcdError::Transport(format!("cannot reach {}: {e}", target.target())))?;
    Ok(out.stdout.trim().to_string())
}

fn host_check(expected: Option<&str>, actual: &str, stage: &str) -> Result<()> {
    match expected {
        // A target reporting a short name still satisfies the FQDN every example
        // config names; anything else would fail on correctly-configured boxes.
        Some(host) if host != actual && !host.starts_with(&format!("{actual}.")) => Err(DcdError::HostMismatch {
            stage: stage.to_string(),
            expected: host.to_string(),
            actual: actual.to_string(),
        }),
        _ => Ok(()),
    }
}

/// Reads the deploy state through the effects seam so a remote `deploy_root` is
/// read from the target, not from the deploying machine. A **missing** file is a
/// fresh stage; an **unreadable** one is an error — mapping both to a default
/// would silently discard the release history and let the next finalize
/// overwrite it (spec §2.3).
/// Resolves the compose model on the DEPLOYING machine, against the checkout.
/// Resolving it on the target would make `check` validate nothing on a fresh
/// host and make `--dry-run` plan from stale remote files (spec §7 preamble).
///
/// Its stdout is parsed and never traced: `compose config` inlines resolved env
/// values, so printing it would dump the whole secret set (spec §8.2).
/// `release.run` is the fallback for a project with no compose service for its app
/// (spec §5.3). Rendering it into a real compose document and appending it to
/// `compose.files` is what keeps ONE creation path: from here on the run-based
/// release is indistinguishable from a declared service — it resolves in the
/// model, uploads with `sync`, pulls, and is created by `compose run`.
///
/// It lands in dcd's own private directory, not the checkout. `docker compose config`
/// must be able to read it, so it has to exist before `check` and `--dry-run` resolve
/// the model — but those two are documented as having no side effects, and writing
/// a generated, untracked file into the operator's repository is one.
///
/// Not a shared `/tmp`: the name is `{project}-dcd-release-run.{stage}.yml`, which
/// anyone sharing the machine can derive and pre-create — as a symlink dcd's write
/// would follow, or as a file they keep owning and rewrite before `sync` uploads it
/// to the target and `compose` starts what it describes. `prepare_private_directory`
/// is the whole defence: inside a `0700` directory dcd owns, nobody else can plant
/// the name in the first place.
fn materialise_release_run(cfg: &mut Config) -> Result<()> {
    let Some(run) = cfg.release.run.clone() else {
        return Ok(());
    };
    let service = cfg.release.service_name(&cfg.project);
    let document = compose::render_run_document(&service, &run);
    let name = format!("dcd-release-run.{}.yml", cfg.stage);
    prepare_private_directory()?;
    let path = private_directory().join(format!("{}-{name}", cfg.project));
    std::fs::write(&path, document)
        .map_err(|e| DcdError::Config(format!("cannot write {}: {e}", path.display())))?;

    // Read from temp, addressed under `deploy_root` by its plain name.
    cfg.compose.files.push(PathBuf::from(name));
    cfg.compose.generated_source = Some(path);
    Ok(())
}

/// Splits each `compose.files` entry into the path dcd READS (against the config's
/// own directory, on this machine) and the path `-f` ADDRESSES (against
/// `deploy_root`, where every command runs). They are the same string in the common
/// layout and differ the moment `-c` points elsewhere or `deploy_root` is not dcd's
/// cwd — where, before this, the model resolved from one directory and the deploy
/// ran in another, and an absolute entry was uploaded outside `deploy_root`.
fn resolve_compose_sources(cfg: &mut Config, config_dir: &Path) {
    let generated = cfg.compose.generated_source.clone();
    let last = cfg.compose.files.len().saturating_sub(1);
    let mut sources = Vec::with_capacity(cfg.compose.files.len());
    let mut addressed = Vec::with_capacity(cfg.compose.files.len());
    for (index, file) in cfg.compose.files.iter().enumerate() {
        let generated_here = generated.as_ref().filter(|_| index == last);
        match (generated_here, file.is_absolute()) {
            (Some(path), _) => {
                sources.push(path.clone());
                addressed.push(file.clone());
            }
            (None, true) => {
                sources.push(file.clone());
                addressed.push(file.file_name().map(PathBuf::from).unwrap_or_else(|| file.clone()));
            }
            (None, false) => {
                sources.push(config_dir.join(file));
                addressed.push(file.clone());
            }
        }
    }
    cfg.compose.sources = sources;
    cfg.compose.files = addressed;
}

fn resolve_compose_model(cfg: &Config, cli: &Cli, resolved: &crate::dotenv::ResolvedEnv) -> Result<ComposeModel> {
    // Resolved from the SOURCE paths: this runs on the deploying machine, in dcd's
    // own cwd, against the checkout.
    let argv = compose::config_argv(&cfg.project, &cfg.compose.sources, &cfg.compose.profiles);
    // The chain has to reach compose, or every `image: ${REGISTRY}:${APP_TAG}`
    // resolves to `:` — and that empty ref is then written into the override file,
    // which is passed LAST and therefore wins over the operator's own compose file.
    let runner = SystemRunner::with_env(resolved.container_env.clone());
    let opts = RunOpts {
        env: Some(cfg.compose.env.clone().into_iter().collect()),
        ..RunOpts::default()
    };
    let out = runner
        .run(&argv, Access::Read, &opts)
        .map_err(|e| DcdError::Config(format!("cannot resolve the compose model: {e}")))?;
    let mut model = ComposeModel::from_json(out.stdout.as_bytes())?;
    apply_image_pins(&mut model, &cli.images)?;
    config::validate_with_model(cfg, &model)?;
    Ok(model)
}

/// Parses `--image` and pins each service on the resolved model. Split out from
/// `resolve_compose_model` so the wiring is covered without a daemon — the flag
/// broke once already, and the only tests that saw it needed real Docker.
fn apply_image_pins(model: &mut ComposeModel, specs: &[String]) -> Result<()> {
    for (service, image) in image_pins(specs)? {
        model.pin_image(&service, &image)?;
    }
    Ok(())
}

/// Parses `--image <service>=<ref>` before anything reaches Docker, so a typo
/// fails on the spot rather than after the compose model resolves.
fn image_pins(specs: &[String]) -> Result<Vec<(String, String)>> {
    specs
        .iter()
        .map(|spec| {
            let split = spec.split_once('=');
            let (service, image) = split
                .ok_or_else(|| DcdError::Config(format!("--image `{spec}` must be service=reference")))?;
            if service.is_empty() || image.is_empty() {
                return Err(DcdError::Config(format!("--image `{spec}` must be service=reference")));
            }
            Ok((service.to_string(), image.to_string()))
        })
        .collect()
}

fn load_state(fs: &dyn FileSystem, deploy_root: &Path) -> Result<State> {
    let path = deploy_root.join("dcd-state.json");
    let present = fs
        .exists(&path)
        .map_err(|e| DcdError::Config(format!("cannot read state {}: {e}", path.display())))?;
    if !present {
        return Ok(State::default());
    }
    let bytes = fs
        .read(&path)
        .map_err(|e| DcdError::Config(format!("cannot read state {}: {e}", path.display())))?;
    State::from_json(&bytes).map_err(|e| DcdError::Config(format!("corrupt state {}: {e}", path.display())))
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

/// Derived from the typed `Config`, so it cannot drift from the loader the way a
/// hand-written schema would (spec §8.4).
fn schema() -> Result<()> {
    let json = serde_json::to_string_pretty(&config::authoring_schema()?)
        .map_err(|e| DcdError::Config(format!("cannot render the schema: {e}")))?;
    println!("{json}");
    Ok(())
}

fn init(path: &Path, force: bool, with_plugin: bool, from_compose: Option<&Path>) -> Result<()> {
    if path.exists() && !force {
        return Err(DcdError::Config(format!("{} already exists (use --force)", path.display())));
    }
    let document = match from_compose {
        Some(compose) => {
            let scaffold = crate::scaffold::Scaffold::from_compose(compose, path)?;
            let rendered = scaffold.render();
            for note in scaffold.notes() {
                println!("{note}");
            }
            rendered
        }
        None => SCAFFOLD.to_string(),
    };
    std::fs::write(path, &document).map_err(|e| DcdError::Config(format!("cannot write {}: {e}", path.display())))?;
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

/// Plugin sources are read on the deploying machine and executed in dcd's own
/// process — nothing on a target ever reads a `.lua` file — so relative paths
/// resolve against the **config file's** directory, never `deploy_root` (spec §2.1).
fn load_plugins(cfg: &config::Config, config_dir: &Path) -> Result<Vec<(String, String)>> {
    let mut plugins = Vec::new();
    for path in &cfg.plugins {
        let resolved = if path.is_absolute() {
            path.clone()
        } else {
            config_dir.join(path)
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
    /// The SAME runner and filesystem the deploy will use. Hard-coding the local
    /// ones made `configure` address the deploying machine while every later hook
    /// addressed the target — spec §6.5 says the two see one environment.
    runner: Box<dyn CommandRunner + 'a>,
    fs: Box<dyn FileSystem + 'a>,
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

    /// The configure hook runs before the engine exists, so it traces its own
    /// commands (`-v`) the way `Engine::run_argv` traces the recipe's.
    fn run_traced(&self, argv: Argv) -> std::result::Result<String, String> {
        self.reporter.command(&argv.display());
        let started = Instant::now();
        let outcome = self.runner.run(&argv, Access::Mutate, &RunOpts::default());
        let ms = started.elapsed().as_millis() as u64;
        match &outcome {
            Ok(out) => self.reporter.command_output(out.code, ms, &out.stdout, &out.stderr),
            Err(err) => self.reporter.command_error(ms, &err.to_string()),
        }
        outcome.map(|out| out.stdout).map_err(|e| e.to_string())
    }
}

impl HookHost for ConfigureHost<'_> {
    fn run_host(&self, cmd: &str) -> std::result::Result<String, String> {
        let full = format!("cd {} && {}", crate::ssh::quote(&self.deploy_root.display().to_string()), cmd);
        self.run_traced(Argv::of(["sh", "-c", &full]))
    }

    fn in_release(&self, _cmd: &str) -> std::result::Result<String, String> {
        Err(self.no_container("in_release"))
    }
    fn exec_in(&self, service: &str, cmd: &str) -> std::result::Result<String, String> {
        self.run_traced(Argv::of(["docker", "exec", service, "sh", "-c", cmd]))
    }
    fn docker(&self, args: Vec<String>) -> std::result::Result<String, String> {
        let mut argv = vec!["docker".to_string()];
        argv.extend(args);
        self.run_traced(Argv(argv))
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
        self.fs
            .read(&self.resolve(path))
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .map_err(|e| e.to_string())
    }
    fn write_file(&self, path: &str, content: &str) -> std::result::Result<(), String> {
        self.fs.write(&self.resolve(path), content.as_bytes(), None).map_err(|e| e.to_string())
    }
    fn file_exists(&self, path: &str) -> bool {
        match self.fs.exists(&self.resolve(path)) {
            Ok(present) => present,
            Err(e) => {
                self.reporter
                    .warn(&format!("ctx.file_exists({path}) could not be answered, reporting false: {e}"));
                false
            }
        }
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

const SCAFFOLD: &str = r#"version: 2
project: myapp

# dcd runs HERE (your laptop or CI runner) and reaches the server over SSH.
# Omit `ssh:` to drive a local Docker socket instead.
ssh: ${DEPLOY_SSH}
deploy_root: ${DEPLOY_ROOT}     # where dcd works ON THE SERVER

# Your compose file declares the containers; dcd declares the orchestration.
# It is uploaded to deploy_root at the start of every deploy.
compose:
  files: [docker-compose.prod.yml]

# The app: a compose service, started alongside the live one and cut over to.
# Mark it `profiles: ["dcd-release"]` in the compose file so a hand-run
# `docker compose up` does not start a second copy beside the release.
release:
  service: app
  # The gate is the service's own `healthcheck:` in the compose file. Use this
  # escape hatch instead when the image cannot probe itself:
  # healthcheck: { exec_in: nginx, cmd: 'curl -sf http://{container}:8080/health' }
  # migrate: { before: 'app migrate --phase before', after: 'app migrate --phase after' }
  # drain: 'app graceful-stop'

# Point the router at the new container, then reload it.
cutover:
  service: nginx
  backend_port: 8080
  reload: { exec_in: nginx, cmd: 'nginx -s reload' }

# Policy only — names, images and readiness come from the compose file.
# services:
#   postgres: { on_recreate_drain_workers: true }
#   valkey: { recreate: never }

# workers:
#   service: worker
#   provider: { static: [default] }

stages:
  prod:
    host: prod.example.internal   # dcd refuses to run if the TARGET reports another name
"#;

const PLUGIN_STUB: &str = r#"-- Plugins register tasks and hooks. They are read and run on the
-- deploying machine, resolved against this config file's directory, and never uploaded.
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

    /// The v1 bug this guards: `load_state` mapped ANY read error to
    /// `State::default()`, so a `deploy_root` dcd could not read looked like a
    /// brand-new stage — losing the release history, and letting the next
    /// finalize overwrite it. Missing and unreadable must not be the same thing.
    #[test]
    fn a_missing_state_file_is_fresh_but_an_unreadable_one_is_an_error() {
        use crate::effects::MemoryFs;

        let fs = MemoryFs::new();
        let root = Path::new("/srv/acme");

        let fresh = load_state(&fs, root).expect("a missing state file is a fresh stage");
        assert!(fresh.stage("prod").is_none());

        fs.write(&root.join("dcd-state.json"), b"{ not json", None).unwrap();
        let err = load_state(&fs, root).expect_err("unreadable state must not silently default");
        assert!(err.to_string().contains("corrupt state"), "got: {err}");
    }
    use super::*;

    /// The generated compose document is read back by `docker compose config`, then
    /// uploaded and started on the target — so wherever it lands, a second user on
    /// the deploying machine must not be able to pre-create that name as a symlink
    /// to follow or as a file of their own to rewrite. A `0700` directory dcd owns
    /// is the answer; a shared `/tmp` is not.
    #[test]
    fn the_generated_release_run_document_lands_in_a_private_directory() {
        let source = r#"
version: 2
project: demo
deploy_root: /srv/demo
compose:
  files: [docker-compose.yml]
release:
  run: { image: demo/app:1 }
  healthcheck: { exec_in: router, cmd: 'curl -sf http://{container}:80/up' }
cutover: { service: router, backend_port: 80, reload: { exec_in: router, cmd: 'nginx -s reload' } }
"#;
        let empty_env = std::collections::HashMap::new();
        let mut cfg = config::load(source, None, &[], &empty_env).expect("a run-based config");
        materialise_release_run(&mut cfg).expect("the document is rendered");

        let path = cfg.compose.generated_source.expect("run: renders a document");
        let directory = path.parent().expect("the document has a home");
        assert_eq!(directory, private_directory(), "the document must not land in a shared directory");
        assert_ne!(directory, std::env::temp_dir(), "a bare temp dir is world-writable");

        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(directory).expect("the directory exists").permissions().mode();
        assert_eq!(mode & 0o077, 0, "{} is reachable by another user: mode {mode:o}", directory.display());

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn host_check_guards_mismatch_only() {
        assert!(host_check(None, "anybox", "prod").is_ok());
        assert!(host_check(Some("prod-host"), "prod-host", "prod").is_ok());
        let err = host_check(Some("prod-host"), "beta-box", "prod").unwrap_err();
        assert_eq!(err.exit_code(), 5);
        assert!(err.to_string().contains("prod-host"));
    }

    #[test]
    /// TC-040. Asserted against the pure policy with the TTY state passed IN:
    /// going through `read_env_stdin` read process-global stdin, so the result
    /// depended on how the suite was launched — it failed from a terminal, and when
    /// it passed, the terminal branch short-circuited before reaching this guard.
    fn env_stdin_on_a_prompting_command_without_yes_refuses_eagerly() {
        let cli = Cli::parse_from(["dcd", "rollback", "prod", "--env-stdin"]);
        let err = env_stdin_refusal(&cli, false).expect("a prompting command must refuse");
        assert!(err.to_string().contains("pass -y/--yes"), "got: {err}");
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn env_stdin_from_a_terminal_is_refused_whatever_the_command() {
        for command in [["dcd", "deploy", "prod", "--env-stdin"], ["dcd", "check", "prod", "--env-stdin"]] {
            let cli = Cli::parse_from(command);
            let err = env_stdin_refusal(&cli, true).expect("a TTY carries no document");
            assert!(err.to_string().contains("requires piped input"), "got: {err}");
        }
    }

    #[test]
    fn unlock_prompts_so_env_stdin_needs_yes_there_too() {
        let cli = Cli::parse_from(["dcd", "unlock", "prod", "--env-stdin"]);
        let err = env_stdin_refusal(&cli, false).expect("unlock prompts, so it must refuse");
        assert!(err.to_string().contains("pass -y/--yes"), "got: {err}");

        let confirmed = Cli::parse_from(["dcd", "unlock", "prod", "--env-stdin", "-y"]);
        assert!(env_stdin_refusal(&confirmed, false).is_none());

        // A non-prompting command needs no -y.
        let deploy = Cli::parse_from(["dcd", "deploy", "prod", "--env-stdin"]);
        assert!(env_stdin_refusal(&deploy, false).is_none());
    }

    #[test]
    fn unlock_takes_the_positional_stage_like_every_other_command() {
        let cli = Cli::parse_from(["dcd", "unlock", "beta"]);
        assert_eq!(stage_of(&cli.command), Some("beta"));

        let stageless = Cli::parse_from(["dcd", "unlock"]);
        assert_eq!(stage_of(&stageless.command), None);
    }

    #[test]
    fn env_stdin_alone_discovers_no_dotenv_next_to_the_config() {
        // The regression: an application .env beside dcd.yaml used to be absorbed
        // into the chain — and into every container — behind a stdin-only deploy.
        let cli = Cli::parse_from(["dcd", "check", "prod", "--config", "/srv/app/dcd.yaml", "--env-stdin"]);
        assert_eq!(chain_base(&cli), None);

        let empty = std::collections::HashMap::new();
        let resolved = crate::dotenv::resolve(None, "prod", false, Some("APP_SECRET=s\n"), &empty).unwrap();
        let labels: Vec<&str> = resolved.layers.iter().map(|layer| layer.label.as_str()).collect();
        assert_eq!(labels, ["<stdin>"]);
        assert!(resolved.skipped.is_empty(), "nothing is probed on disk: {:?}", resolved.skipped);
    }

    #[test]
    fn an_explicit_file_source_still_pairs_with_a_stdin_layer() {
        let with_dir = Cli::parse_from([
            "dcd", "check", "prod", "--config", "/srv/app/dcd.yaml", "--env-dir", "/srv/env", "--env-stdin",
        ]);
        assert_eq!(chain_base(&with_dir), Some(PathBuf::from("/srv/env/.env")));

        let with_file = Cli::parse_from([
            "dcd", "check", "prod", "--config", "/srv/app/dcd.yaml", "--env-file", "/srv/env/.env.deploy", "--env-stdin",
        ]);
        assert_eq!(chain_base(&with_file), Some(PathBuf::from("/srv/env/.env.deploy")));
    }

    #[test]
    fn without_env_stdin_the_config_directory_dotenv_is_the_base() {
        let cli = Cli::parse_from(["dcd", "check", "prod", "--config", "/srv/app/dcd.yaml"]);
        assert_eq!(chain_base(&cli), Some(PathBuf::from("/srv/app/.env")));
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
        check_report(&cfg, &reporter, &resolved, Some(Path::new("."))).unwrap();

        let output = reporter.lines().join("\n");
        assert!(output.contains("APP_SECRET"), "key names are printed: {output}");
        assert!(output.contains("DATABASE_URL"), "key names are printed: {output}");
        assert!(!output.contains("hunter2"), "value leaked: {output}");
        assert!(!output.contains("sup3rs3cret"), "value leaked: {output}");
    }

    const SCAFFOLD_TEST_CONFIG: &str = r#"
version: 2
project: demo
compose:
  files: [base.yml]
release:
  service: app
  healthcheck: { exec_in: nginx, cmd: 'curl {container}' }
cutover:
  service: nginx
  backend_port: 8080
  reload: { exec_in: nginx, cmd: 'nginx -s reload' }
services:
  nginx: { recreate: never }
stages:
  prod: {}
"#;

    #[test]
    fn scaffold_parses_and_validates() {
        let env: std::collections::HashMap<String, String> = [
            ("DEPLOY_SSH", "deploy@prod.example.internal"),
            ("DEPLOY_ROOT", "/srv/myapp"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let cfg = config::load(SCAFFOLD, Some("prod"), &[], &env).unwrap();
        assert_eq!(cfg.project, "myapp");
        assert_eq!(cfg.release.service.as_deref(), Some("app"));
        assert_eq!(cfg.cutover.service, "nginx");
        assert_eq!(cfg.ssh.as_deref(), Some("deploy@prod.example.internal"));
    }

    /// `--image` names a COMPOSE SERVICE and a full image reference; a reference
    /// containing '=' (a digest pin does not, but a tag with one would) must keep
    /// everything after the first separator.
    /// The wiring, not just the parser: this is the path that broke, and until now
    /// only a Docker-gated test covered it.
    #[test]
    fn the_cli_flag_pins_the_resolved_model() {
        let mut model = crate::compose::ComposeModel::from_json(
            br#"{"services":{"app":{"image":"reg:app-1"},"worker":{"image":"reg:app-1"}}}"#,
        )
        .unwrap();

        apply_image_pins(&mut model, &["app=reg:pinned".to_string()]).unwrap();

        assert_eq!(model.service("app").unwrap().image.as_deref(), Some("reg:pinned"));
        assert_eq!(
            model.service("worker").unwrap().image.as_deref(),
            Some("reg:app-1"),
            "a pin is per-service"
        );
    }

    #[test]
    fn the_cli_flag_refuses_a_service_the_compose_files_do_not_declare() {
        let mut model = crate::compose::ComposeModel::from_json(br#"{"services":{"app":{"image":"reg:app-1"}}}"#).unwrap();
        let error = apply_image_pins(&mut model, &["ap=reg:pinned".to_string()])
            .unwrap_err()
            .to_string();
        assert!(error.contains("--image 'ap' is not a service"), "{error}");
        assert_eq!(model.service("app").unwrap().image.as_deref(), Some("reg:app-1"));
    }

    #[test]
    fn an_image_pin_splits_on_the_first_separator_only() {
        let pins = image_pins(&["app=registry.example.com/team/app:v1".to_string()]).unwrap();
        assert_eq!(pins, vec![("app".to_string(), "registry.example.com/team/app:v1".to_string())]);

        let digest = image_pins(&["app=reg/app@sha256:abc=def".to_string()]).unwrap();
        assert_eq!(digest[0].1, "reg/app@sha256:abc=def");
    }

    #[test]
    fn an_image_pin_without_both_halves_is_rejected() {
        for spec in ["app", "app=", "=reg:tag", ""] {
            let error = image_pins(&[spec.to_string()]).unwrap_err().to_string();
            assert!(error.contains("must be service=reference"), "{spec}: {error}");
        }
    }

    /// Mounting the directory is the shape the docs steer operators to — a single-file
    /// mount pins the inode dcd renames away from — so warning about it was telling
    /// them the correct config was wrong.
    #[test]
    fn a_directory_mount_above_the_upstream_file_counts_as_mounting_it() {
        let upstream_file = Path::new("nginx-conf/upstream-block.conf");
        let upstream = Path::new("/srv/app").join(upstream_file);

        for source in ["/srv/app/nginx-conf", "/srv/app", "/srv/app/nginx-conf/upstream-block.conf"] {
            assert!(
                upstream_is_mounted(&upstream, upstream_file, &[source.to_string()]),
                "{source} places the upstream file inside the router"
            );
        }

        assert!(
            upstream_is_mounted(&upstream, upstream_file, &["./nginx-conf/upstream-block.conf".to_string()]),
            "the relative form compose absolutizes from is still recognised"
        );
    }

    /// The warning has to keep firing for a router that mounts something else entirely,
    /// including a sibling whose path merely shares a prefix STRING with the upstream
    /// directory — `starts_with` compares components, which is what makes that safe.
    #[test]
    fn an_unrelated_mount_does_not_count_as_mounting_the_upstream_file() {
        let upstream_file = Path::new("nginx-conf/upstream-block.conf");
        let upstream = Path::new("/srv/app").join(upstream_file);

        for source in ["/srv/app/certs", "/srv/other/nginx-conf", "/srv/app/nginx-conf-backup"] {
            assert!(
                !upstream_is_mounted(&upstream, upstream_file, &[source.to_string()]),
                "{source} does not place the upstream file inside the router"
            );
        }
    }
}
