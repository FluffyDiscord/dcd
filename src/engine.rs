//! The red-black recipe: a fixed ordered sequence of steps with before/after hook
//! slots (spec §3, §7). Every side effect goes through the effects seam, so a full
//! deploy is asserted against the recorded argv with no Docker.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use indexmap::IndexMap;

use crate::config::{Config, HookAction, Recreate};
use crate::docker::Docker;
use crate::effects::{Access, Argv, Clock, CommandRunner, FileSystem, RunOpts};
use crate::error::{DcdError, Result};
use crate::redact::Redactor;
use crate::signal::Interrupt;
use crate::state::{FinalizeKind, Release, ReleaseStatus, State};
use crate::ui::{Reporter, Status};

pub const DEPLOY_STEPS: &[&str] = &[
    "preflight",
    "ensure_upstream",
    "pull",
    "infra",
    "migrate:before",
    "start:black",
    "healthcheck",
    "cutover",
    "drain:red",
    "migrate:after",
    "workers",
    "finalize",
];

const ROLLBACK_STEPS: &[&str] = &[
    "preflight",
    "ensure_upstream",
    "pull",
    "infra",
    "start:black",
    "healthcheck",
    "cutover",
    "drain:red",
    "workers",
    "finalize",
];

const RESUME_STEPS: &[&str] = &["drain:red", "migrate:after", "workers", "finalize"];

enum Outcome {
    Done(Option<String>),
    Skipped,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Deploy,
    Rollback,
    Resume,
}

pub struct Options {
    pub dry_run: bool,
    pub sleep_enabled: bool,
    pub reason: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            dry_run: false,
            sleep_enabled: true,
            reason: None,
        }
    }
}

pub struct Engine<'a> {
    cfg: &'a Config,
    runner: &'a dyn CommandRunner,
    fs: &'a dyn FileSystem,
    clock: &'a dyn Clock,
    reporter: &'a Reporter,
    redactor: &'a Redactor,
    interrupt: &'a Interrupt,
    docker: Docker<'a>,
    opts: Options,

    deploy_root: PathBuf,
    state_path: PathBuf,
    state: State,

    mode: Mode,
    release_id: u64,
    container: String,
    images: IndexMap<String, String>,
    serving_before: Option<String>,
    ran_migrations: bool,
    drained: bool,
    post_cutover: bool,
    black_started: bool,
}

impl<'a> Engine<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: &'a Config,
        runner: &'a dyn CommandRunner,
        fs: &'a dyn FileSystem,
        clock: &'a dyn Clock,
        reporter: &'a Reporter,
        redactor: &'a Redactor,
        interrupt: &'a Interrupt,
        state: State,
        opts: Options,
    ) -> Self {
        let deploy_root = cfg.deploy_root.clone();
        let state_path = deploy_root.join("dcd-state.json");
        Engine {
            cfg,
            runner,
            fs,
            clock,
            reporter,
            redactor,
            interrupt,
            docker: Docker::new(cfg),
            opts,
            deploy_root,
            state_path,
            state,
            mode: Mode::Deploy,
            release_id: 0,
            container: String::new(),
            images: IndexMap::new(),
            serving_before: None,
            ran_migrations: false,
            drained: false,
            post_cutover: false,
            black_started: false,
        }
    }

    pub fn into_state(self) -> State {
        self.state
    }

    fn full_ref(&self, tag: &str) -> String {
        match &self.cfg.registry {
            Some(registry) => format!("{registry}:{tag}"),
            None => tag.to_string(),
        }
    }

    fn resolved_images(&self) -> IndexMap<String, String> {
        self.cfg
            .images
            .iter()
            .map(|(logical, tag)| (logical.clone(), self.full_ref(tag)))
            .collect()
    }

    fn begin(&mut self, mode: Mode) {
        self.mode = mode;
        self.release_id = self.clock.now_epoch();
        self.container = format!("{}-{}", self.cfg.release.container_prefix, self.release_id);
        self.serving_before = self
            .stage()
            .and_then(|s| s.serving())
            .map(|r| r.container.clone());
        self.ran_migrations = self.cfg.release.migrate.as_ref().is_some_and(|m| m.before.is_some());
    }

    pub fn deploy(&mut self) -> Result<()> {
        if let Some(stage) = self.stage() {
            if stage.pending_count() > 1 {
                return Err(DcdError::Config(
                    "state has more than one cutover_pending release — refusing".to_string(),
                ));
            }
            if stage.cutover_pending().is_some() {
                let pending = stage.cutover_pending().unwrap().container.clone();
                return Err(DcdError::PostCutover(format!(
                    "{} has an incomplete release {pending}; run `dcd deploy --resume {}` or `dcd rollback {}`",
                    self.cfg.stage, self.cfg.stage, self.cfg.stage
                )));
            }
        }
        self.begin(Mode::Deploy);
        self.images = self.resolved_images();
        self.drive(DEPLOY_STEPS)
    }

    pub fn rollback(&mut self) -> Result<()> {
        let target = self
            .stage()
            .and_then(|s| s.rollback_target())
            .ok_or_else(|| DcdError::PreCutover(format!("no previous release for {} to roll back to", self.cfg.stage)))?
            .clone();
        self.begin(Mode::Rollback);
        self.images = target.images.clone();
        let tags: Vec<String> = self.images.values().cloned().collect();
        for tag in &tags {
            if tag.is_empty() {
                return Err(DcdError::PreCutover("rollback target image is missing".to_string()));
            }
            let inspect = Argv::of(["docker", "image", "inspect", tag]);
            if !self.try_run(&inspect, Access::Read)?.success() {
                let pull = self.docker.pull(tag);
                if !self.try_run(&pull, Access::Mutate)?.success() {
                    return Err(DcdError::PreCutover(format!("target image {tag} not present and not pullable")));
                }
            }
        }
        self.ran_migrations = false;
        self.reporter.log(&format!(
            "rolling back {} -> release {} ({})",
            self.cfg.stage,
            target.id,
            target.app_image().unwrap_or("?")
        ));
        self.drive(ROLLBACK_STEPS)
    }

    pub fn resume(&mut self) -> Result<()> {
        if self.stage().map(|s| s.pending_count()).unwrap_or(0) > 1 {
            return Err(DcdError::Config(
                "state has more than one cutover_pending release — refusing".to_string(),
            ));
        }
        let pending = self
            .stage()
            .and_then(|s| s.cutover_pending())
            .ok_or_else(|| DcdError::PreCutover(format!("{} has no incomplete release to resume", self.cfg.stage)))?
            .clone();
        self.begin(Mode::Resume);
        self.container = pending.container.clone();
        self.images = pending.images.clone();
        self.post_cutover = true;
        self.drive(RESUME_STEPS)
    }

    fn drive(&mut self, steps: &[&str]) -> Result<()> {
        for step in steps {
            if self.interrupt.triggered() && !self.post_cutover {
                self.cleanup_black();
                return Err(DcdError::Interrupted);
            }
            if let Err(err) = self.run_step(step) {
                if !self.post_cutover {
                    self.cleanup_black();
                }
                return Err(err);
            }
        }
        Ok(())
    }

    fn run_step(&mut self, name: &str) -> Result<()> {
        let key = name.replace(':', "_");
        self.run_hooks(&format!("before_{key}"))?;
        let started = Instant::now();
        let outcome = self.dispatch(name)?;
        let ms = started.elapsed().as_millis() as u64;
        match outcome {
            Outcome::Done(detail) => self.reporter.task(name, Status::Ok, ms, detail.as_deref()),
            Outcome::Skipped => self.reporter.task(name, Status::Skip, ms, None),
        }
        self.run_hooks(&format!("after_{key}"))?;
        Ok(())
    }

    fn dispatch(&mut self, name: &str) -> Result<Outcome> {
        match name {
            "preflight" => self.preflight(),
            "ensure_upstream" => self.ensure_upstream(),
            "pull" => self.pull(),
            "infra" => self.infra(),
            "migrate:before" => self.migrate_before(),
            "start:black" => self.start_black(),
            "healthcheck" => self.healthcheck(),
            "cutover" => self.cutover(),
            "drain:red" => self.drain_red(),
            "migrate:after" => self.migrate_after(),
            "workers" => self.workers(),
            "finalize" => self.finalize(),
            other => Err(DcdError::Config(format!("unknown step {other}"))),
        }
    }


    fn preflight(&mut self) -> Result<Outcome> {
        let inspect = self.docker.network_inspect();
        if !self.read(&inspect)?.success() {
            let create = self.docker.network_create();
            self.exec(&create, Access::Mutate)?;
        }
        let dirs = self.cfg.preflight.directories.iter().map(|d| (d.path.clone(), d.owner.clone())).collect::<Vec<_>>();
        for (path, owner) in dirs {
            let resolved = self.resolve(&path);
            self.mkdir(&resolved)?;
            if let Some(owner) = owner {
                let mount = format!("{}:/wd", self.deploy_root.display());
                let target = format!("/wd/{}", path.display());
                let argv = Argv::of(["docker", "run", "--rm", "-v", &mount, "busybox", "chown", &owner, &target]);
                self.exec(&argv, Access::Mutate)?;
            }
        }
        self.reap_orphans()?;
        self.write_compose_env()?;
        Ok(Outcome::Done(None))
    }

    fn ensure_upstream(&mut self) -> Result<Outcome> {
        let current = self.stage().and_then(|s| s.current.clone());
        let dead = match &current {
            Some(container) => {
                let argv = self.docker.is_running(container);
                self.read(&argv)?.stdout.trim().is_empty()
            }
            None => true,
        };
        let upstream = self.resolve(&self.cfg.cutover.upstream_file);
        if dead || !self.fs.exists(&upstream) {
            let backend = self
                .cfg
                .cutover
                .template
                .replace("{backend}", &self.cfg.cutover.fallback_backend);
            self.fs_write(&upstream, backend.as_bytes(), None, "upstream fallback")?;
            return Ok(Outcome::Done(Some("fallback".to_string())));
        }
        Ok(Outcome::Skipped)
    }

    fn pull(&mut self) -> Result<Outcome> {
        let app = self.images.get("app").cloned().unwrap_or_default();
        let pull_app = self.docker.pull(&app);
        self.exec(&pull_app, Access::Mutate)?;
        let managed: Vec<String> = self
            .cfg
            .services
            .values()
            .filter_map(|s| s.image.as_ref())
            .filter_map(|logical| self.images.get(logical).cloned())
            .collect();
        for tag in managed {
            let argv = self.docker.pull(&tag);
            self.exec(&argv, Access::Mutate)?;
        }
        Ok(Outcome::Done(None))
    }

    fn infra(&mut self) -> Result<Outcome> {
        let services: Vec<String> = self.cfg.services.keys().cloned().collect();
        for name in &services {
            let service = &self.cfg.services[name];
            let container = service.container.clone();
            let recreate = service.recreate;
            let drain_first = service.on_recreate_drain_workers;
            let desired = service.image.as_ref().and_then(|l| self.images.get(l)).cloned();

            let needs_recreate = match recreate {
                Recreate::Always => true,
                Recreate::Never => false,
                Recreate::OnImageChange => {
                    let inspect = self.docker.inspect_image(&container);
                    let current = self.read(&inspect)?;
                    let current_image = current.stdout.trim();
                    desired.as_deref().map(|d| d != current_image).unwrap_or(false)
                }
            };

            if needs_recreate && drain_first && !self.drained {
                self.drain_workers()?;
            }
            let argv = if needs_recreate {
                self.docker.compose(&["up", "-d", name], false)
            } else {
                self.docker.compose(&["up", "-d", "--no-recreate", name], false)
            };
            self.exec(&argv, Access::Mutate)?;
        }

        for name in &services {
            let wait = match &self.cfg.services[name].wait {
                Some(wait) => wait,
                None => continue,
            };
            let exec_in = wait.exec_in.clone();
            let cmd = wait.cmd.clone();
            let retries = wait.retries;
            let interval = wait.interval;
            let argv = self.docker.exec_sh(&exec_in, &cmd);
            self.poll(&argv, Access::Read, retries, interval, &format!("{name} not ready"))?;
        }
        Ok(Outcome::Done(None))
    }

    fn migrate_before(&mut self) -> Result<Outcome> {
        let Some(command) = self.cfg.release.migrate.as_ref().and_then(|m| m.before.clone()) else {
            return Ok(Outcome::Skipped);
        };
        let name = format!("{}-migrate-{}", self.cfg.project, self.release_id);
        let app = self.images.get("app").cloned().unwrap_or_default();
        let args: Vec<String> = command.split_whitespace().map(String::from).collect();
        let argv = self.docker.run_throwaway(&name, &app, &args);
        self.exec(&argv, Access::Mutate)?;
        Ok(Outcome::Done(None))
    }

    fn start_black(&mut self) -> Result<Outcome> {
        let exists = self.docker.ps_names(&self.container, true);
        if !self.read(&exists)?.stdout.trim().is_empty() {
            return Err(DcdError::PreCutover(format!("container {} already exists", self.container)));
        }
        let app = self.images.get("app").cloned().unwrap_or_default();
        let argv = self.docker.run_black(&self.container, &app);
        self.exec(&argv, Access::Mutate)?;
        self.black_started = true;
        Ok(Outcome::Done(Some(self.container.clone())))
    }

    fn healthcheck(&mut self) -> Result<Outcome> {
        let hc = &self.cfg.release.healthcheck;
        let cmd = hc.cmd.replace("{container}", &self.container);
        let exec_in = hc.exec_in.clone();
        let retries = hc.retries;
        let interval = hc.interval;
        let argv = self.docker.exec_sh(&exec_in, &cmd);
        let mut last_stderr = String::new();
        for attempt in 1..=retries {
            if self.interrupt.triggered() {
                self.cleanup_black();
                return Err(DcdError::Interrupted);
            }
            let out = self.try_run(&argv, Access::Mutate)?;
            if out.success() {
                return Ok(Outcome::Done(Some(format!("{attempt}/{retries}"))));
            }
            last_stderr = out.stderr;
            if attempt < retries {
                self.sleep(interval);
            }
        }
        Err(DcdError::PreCutover(format!(
            "healthcheck failed after {retries} attempts: {}",
            self.redactor.apply(last_stderr.trim())
        )))
    }

    fn cutover(&mut self) -> Result<Outcome> {
        let upstream = self.resolve(&self.cfg.cutover.upstream_file);
        let previous = self.fs.read(&upstream).ok();
        let backend = format!("{}:{}", self.container, self.cfg.cutover.backend_port);
        let rendered = self.cfg.cutover.template.replace("{backend}", &backend);
        self.fs_write(&upstream, rendered.as_bytes(), None, "upstream cutover")?;

        if let Some(validate) = &self.cfg.cutover.validate {
            let argv = self.docker.exec_sh(&validate.exec_in, &validate.cmd);
            let out = self.try_run(&argv, Access::Mutate)?;
            if !out.success() {
                self.restore_upstream(&upstream, previous);
                return Err(DcdError::PreCutover(format!(
                    "cutover config validation failed: {}",
                    self.redactor.apply(out.stderr.trim())
                )));
            }
        }

        let reload = self.docker.exec_sh(&self.cfg.cutover.reload.exec_in, &self.cfg.cutover.reload.cmd);
        let out = self.try_run(&reload, Access::Mutate)?;
        if !out.success() {
            self.restore_upstream(&upstream, previous);
            return Err(DcdError::PreCutover(format!(
                "router reload failed; restored previous upstream: {}",
                self.redactor.apply(out.stderr.trim())
            )));
        }

        let release = Release {
            id: self.release_id,
            container: self.container.clone(),
            images: self.images.clone(),
            created_at: self.release_id,
            status: ReleaseStatus::CutoverPending,
            ran_migrations: self.ran_migrations,
            reason: self.opts.reason.clone(),
        };
        let stage = self.cfg.stage.clone();
        self.state.stage_mut(&stage).record_cutover(release);
        self.post_cutover = true;
        self.persist_state()?; // INV-3: the live black must be recoverable from disk
        Ok(Outcome::Done(Some("router reloaded".to_string())))
    }

    fn drain_red(&mut self) -> Result<Outcome> {
        let prefix = format!("{}-", self.cfg.release.container_prefix);
        let argv = self.docker.ps_names(&prefix, false);
        let running = self.read(&argv)?.stdout;
        let drain_cmd = self.cfg.release.drain.clone();
        let targets: Vec<String> = running
            .lines()
            .map(str::trim)
            .filter(|n| !n.is_empty() && *n != self.container)
            .map(String::from)
            .collect();
        for target in targets {
            if let Some(cmd) = &drain_cmd {
                let drain = self.docker.exec_sh(&target, cmd);
                let _ = self.try_run(&drain, Access::Mutate);
            }
            let rm = self.docker.rm_f(&target);
            let _ = self.try_run(&rm, Access::Mutate);
        }
        if !self.drained {
            self.drain_workers()?;
        }
        Ok(Outcome::Done(None))
    }

    fn migrate_after(&mut self) -> Result<Outcome> {
        let Some(command) = self.cfg.release.migrate.as_ref().and_then(|m| m.after.clone()) else {
            return Ok(Outcome::Skipped);
        };
        let args: Vec<String> = command.split_whitespace().map(String::from).collect();
        let argv = self.docker.exec_args(&self.container, &args);
        self.exec(&argv, Access::Mutate)?;
        Ok(Outcome::Done(None))
    }

    fn workers(&mut self) -> Result<Outcome> {
        let Some(workers) = &self.cfg.workers else {
            return Ok(Outcome::Skipped);
        };
        let names = self.worker_names(workers)?;
        if names.is_empty() {
            return Ok(Outcome::Skipped);
        }
        let compose_yaml = self.render_workers(workers, &names);
        let path = self.resolve(&workers.compose_file);
        self.fs_write(&path, compose_yaml.as_bytes(), None, "workers compose")?;
        let mut args: Vec<String> = vec!["up".into(), "-d".into()];
        for name in &names {
            args.push(format!("{}{}", workers.name_filter, name));
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let argv = self.docker.compose(&arg_refs, true);
        self.exec(&argv, Access::Mutate)?;
        Ok(Outcome::Done(Some(format!("{} worker(s)", names.len()))))
    }

    fn finalize(&mut self) -> Result<Outcome> {
        let kind = match self.mode {
            Mode::Rollback => FinalizeKind::Rollback,
            _ => FinalizeKind::Deploy,
        };
        let stage = self.cfg.stage.clone();
        let container = self.container.clone();
        let serving_before = self.serving_before.clone();
        let keep = self.cfg.retention.keep_releases;
        let keep_managed = self.cfg.retention.keep_managed_images;
        let managed: Vec<String> = self.cfg.services.values().filter_map(|s| s.image.clone()).collect();
        self.state.stage_mut(&stage).finalize(&container, kind, serving_before.as_deref());
        let (evictions, images) = self
            .state
            .stage(&stage)
            .map(|s| (s.evictions(keep), s.images_to_gc(keep, keep_managed, &managed)))
            .unwrap_or_default();
        self.persist_state()?;
        for container in &evictions {
            let rm = self.docker.rm_f(container);
            let _ = self.try_run(&rm, Access::Mutate);
        }
        for image in &images {
            let rm = self.docker.image_rm(image);
            let _ = self.try_run(&rm, Access::Mutate);
        }
        Ok(Outcome::Done(Some(format!("current = {}", self.container))))
    }


    fn run_hooks(&self, slot: &str) -> Result<()> {
        let Some(actions) = self.cfg.hooks.get(slot) else {
            return Ok(());
        };
        for action in actions {
            self.run_action(action)?;
        }
        Ok(())
    }

    fn run_action(&self, action: &HookAction) -> Result<()> {
        match action {
            HookAction::Run(cmd) => {
                let full = format!("cd {} && {}", self.deploy_root.display(), cmd);
                self.exec(&Argv::of(["sh", "-c", &full]), Access::Mutate)
            }
            HookAction::ExecIn { exec_in } => {
                self.exec(&self.docker.exec_sh(&exec_in.service, &exec_in.cmd), Access::Mutate)
            }
            HookAction::ExecInRelease { exec_in_release } => {
                self.exec(&self.docker.exec_sh(&self.container, exec_in_release), Access::Mutate)
            }
            HookAction::Docker { docker } => {
                let mut argv = vec!["docker".to_string()];
                argv.extend(docker.iter().cloned());
                self.exec(&Argv(argv), Access::Mutate)
            }
            HookAction::Compose { compose } => {
                let refs: Vec<&str> = compose.iter().map(String::as_str).collect();
                self.exec(&self.docker.compose(&refs, false), Access::Mutate)
            }
            HookAction::CpFromRelease { cp_from_release } => {
                let src = format!("{}:{}", self.container, cp_from_release.from);
                let dst = self.resolve(Path::new(&cp_from_release.to)).display().to_string();
                self.exec(&self.docker.cp(&src, &dst), Access::Mutate)
            }
            HookAction::CpToRelease { cp_to_release } => {
                let src = self.resolve(Path::new(&cp_to_release.from)).display().to_string();
                let dst = format!("{}:{}", self.container, cp_to_release.to);
                self.exec(&self.docker.cp(&src, &dst), Access::Mutate)
            }
        }
        .map(|_| ())
    }


    fn stage(&self) -> Option<&crate::state::StageState> {
        self.state.stage(&self.cfg.stage)
    }

    fn worker_names(&self, workers: &crate::config::Workers) -> Result<Vec<String>> {
        if let Some(list) = workers.provider.static_list() {
            return Ok(list.clone());
        }
        let Some(command) = &workers.provider.command_in_release else {
            return Ok(Vec::new());
        };
        if self.opts.dry_run {
            self.reporter
                .warn("worker set is dynamic (provider command); not resolvable in dry-run");
            return Ok(Vec::new());
        }
        let argv = self.docker.exec_sh(&self.container, command);
        let out = self.try_run(&argv, Access::Mutate)?;
        Ok(out
            .stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.contains('.') && !workers.provider.exclude.iter().any(|e| e == line))
            .map(String::from)
            .collect())
    }

    fn render_workers(&self, workers: &crate::config::Workers, names: &[String]) -> String {
        let template = &workers.template;
        let image = self.images.get(&template.image).cloned().unwrap_or_else(|| template.image.clone());
        let mut yaml = String::from("services:\n");
        for name in names {
            let service = format!("{}{}", workers.name_filter, name);
            yaml.push_str(&format!("    {service}:\n"));
            yaml.push_str(&format!("        image: {image}\n"));
            yaml.push_str(&format!("        entrypoint: {}\n", json_array(&template.entrypoint)));
            let command: Vec<String> = template.command.iter().map(|c| c.replace("{name}", name)).collect();
            yaml.push_str(&format!("        command: {}\n", json_array(&command)));
            yaml.push_str(&format!("        stop_signal: {}\n", template.stop_signal));
            yaml.push_str(&format!("        stop_grace_period: {}s\n", template.stop_grace_period));
            yaml.push_str(&format!("        restart: {}\n", template.restart));
            if !template.env.is_empty() {
                yaml.push_str("        environment:\n");
                for (key, value) in &template.env {
                    yaml.push_str(&format!("            {key}: {value}\n"));
                }
            }
            if !template.volumes.is_empty() {
                yaml.push_str("        volumes:\n");
                for volume in &template.volumes {
                    yaml.push_str(&format!("            - {volume}\n"));
                }
            }
        }
        yaml.push_str(&format!("networks:\n    default:\n        name: {}\n        external: true\n", self.cfg.network));
        yaml
    }

    fn drain_workers(&mut self) -> Result<()> {
        let Some(workers) = &self.cfg.workers else {
            return Ok(());
        };
        let name_filter = workers.name_filter.clone();
        let drain_cmd = workers.drain.clone();
        let timeout = workers.stop_timeout;
        let argv = self.docker.worker_ps_names(&name_filter);
        let names: Vec<String> = self
            .read(&argv)?
            .stdout
            .lines()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(String::from)
            .collect();
        if names.is_empty() {
            self.drained = true;
            return Ok(());
        }
        if let Some(cmd) = &drain_cmd {
            for name in &names {
                let drain = self.docker.exec_sh(name, cmd);
                let _ = self.try_run(&drain, Access::Mutate);
            }
        }
        let stop = self.docker.stop(&names, timeout);
        let _ = self.try_run(&stop, Access::Mutate);
        self.drained = true;
        Ok(())
    }

    fn reap_orphans(&mut self) -> Result<()> {
        let prefix = format!("{}-", self.cfg.release.container_prefix);
        let argv = self.docker.ps_names(&prefix, true);
        let out = self.read(&argv)?;
        let known: HashSet<String> = self
            .stage()
            .map(|s| s.releases.iter().map(|r| r.container.clone()).collect())
            .unwrap_or_default();
        let orphans: Vec<String> = out
            .stdout
            .lines()
            .map(str::trim)
            .filter(|n| !n.is_empty() && !known.contains(*n))
            .map(String::from)
            .collect();
        for orphan in orphans {
            let rm = self.docker.rm_f(&orphan);
            let _ = self.try_run(&rm, Access::Mutate);
        }
        Ok(())
    }

    fn write_compose_env(&self) -> Result<()> {
        let mut content = String::new();
        for (key, value) in &self.cfg.compose.env {
            content.push_str(&format!("{key}={value}\n"));
        }
        let path = self.resolve(&self.cfg.compose.env_file);
        self.fs_write(&path, content.as_bytes(), Some(0o600), "compose env")
    }

    fn persist_state(&self) -> Result<()> {
        let json = self.state.to_json();
        self.fs_write(&self.state_path.clone(), json.as_bytes(), Some(0o600), "state")
    }

    fn restore_upstream(&self, upstream: &Path, previous: Option<Vec<u8>>) {
        if let Some(previous) = previous {
            let _ = self.fs_write(upstream, &previous, None, "upstream restore");
        }
        self.cleanup_black();
    }

    fn cleanup_black(&self) {
        if self.black_started {
            let rm = self.docker.rm_f(&self.container);
            let _ = self.try_run(&rm, Access::Mutate);
        }
    }

    fn poll(&self, argv: &Argv, access: Access, retries: u32, interval: u64, fail: &str) -> Result<()> {
        if self.opts.dry_run {
            if !self.try_run(argv, access)?.success() {
                self.reporter.warn(&format!("{fail} (not satisfiable in dry-run)"));
            }
            return Ok(());
        }
        for attempt in 1..=retries {
            if self.try_run(argv, access)?.success() {
                return Ok(());
            }
            if attempt < retries {
                self.sleep(interval);
            }
        }
        Err(DcdError::PreCutover(fail.to_string()))
    }

    fn sleep(&self, secs: u64) {
        if self.opts.sleep_enabled && !self.opts.dry_run && secs > 0 {
            std::thread::sleep(std::time::Duration::from_secs(secs));
        }
    }

    fn resolve(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.deploy_root.join(path)
        }
    }

    fn mkdir(&self, path: &Path) -> Result<()> {
        if self.opts.dry_run {
            self.reporter.plan(&format!("mkdir -p {}", path.display()));
            return Ok(());
        }
        self.fs.create_dir_all(path).map_err(|e| self.classify(format!("mkdir {}: {e}", path.display())))
    }

    fn fs_write(&self, path: &Path, bytes: &[u8], mode: Option<u32>, what: &str) -> Result<()> {
        if self.opts.dry_run {
            self.reporter.plan(&format!("write {what}: {}", path.display()));
            return Ok(());
        }
        self.fs.write(path, bytes, mode).map_err(|e| self.classify(format!("write {}: {e}", path.display())))
    }

    fn run_argv(&self, argv: &Argv, access: Access, check: bool) -> Result<crate::effects::CmdOutput> {
        if self.opts.dry_run && access == Access::Mutate {
            self.reporter.plan(&self.redactor.apply(&argv.display()));
        }
        self.runner
            .run(argv, access, &RunOpts { check })
            .map_err(|e| self.classify(self.redactor.apply(&e.to_string())))
    }

    fn exec(&self, argv: &Argv, access: Access) -> Result<crate::effects::CmdOutput> {
        self.run_argv(argv, access, true)
    }

    fn try_run(&self, argv: &Argv, access: Access) -> Result<crate::effects::CmdOutput> {
        self.run_argv(argv, access, false)
    }

    fn read(&self, argv: &Argv) -> Result<crate::effects::CmdOutput> {
        self.run_argv(argv, Access::Read, false)
    }

    fn classify(&self, message: String) -> DcdError {
        if self.post_cutover {
            DcdError::PostCutover(message)
        } else {
            DcdError::PreCutover(message)
        }
    }
}

fn json_array(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|i| format!("'{i}'")).collect();
    format!("[{}]", quoted.join(", "))
}

#[cfg(test)]
mod tests;
