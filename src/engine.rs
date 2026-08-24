//! The red-black recipe: a fixed ordered sequence of steps with before/after hook
//! slots (spec §3, §7). Every side effect goes through the effects seam, so a full
//! deploy is asserted against the recorded argv with no Docker.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use indexmap::IndexMap;

use crate::config::{Config, HookAction, Recreate};
use crate::docker::Docker;
use crate::effects::{Access, Argv, Clock, CommandRunner, FileSystem, RunOpts};
use crate::error::{DcdError, Result};
use crate::lua::{HookHost, LuaHost};
use crate::signal::Interrupt;
use crate::state::{FinalizeKind, KeepPolicy, Release, ReleaseStatus, State};
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
    /// Chain-derived keys+values delivered to containers (spec §5.2.2); values ride
    /// the runner env, the engine only emits their key names.
    pub container_env: BTreeMap<String, String>,
    /// Process env over the chain — what `ctx.env()` reads (spec §5.2.2).
    pub interpolation_env: HashMap<String, String>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            dry_run: false,
            sleep_enabled: true,
            reason: None,
            container_env: BTreeMap::new(),
            interpolation_env: HashMap::new(),
        }
    }
}

/// What a `dcd gc` run intends to do, so the operator can see it (and, for the
/// host-scoped sweep, approve it) before anything is removed.
#[derive(Debug)]
pub struct GcPlan {
    /// Tags this stage's release history and pull ledger say are past their keep count.
    pub recorded: Vec<String>,
    /// Tags found on the host in an owned repository that no stage records (`--all`).
    pub orphans: Vec<String>,
    /// Host tags left alone, each with the rule that spared it.
    pub protected: Vec<(String, String)>,
}

impl GcPlan {
    pub fn removals(&self) -> Vec<String> {
        let mut all = self.recorded.clone();
        all.extend(self.orphans.iter().cloned());
        all
    }

    pub fn is_empty(&self) -> bool {
        self.recorded.is_empty() && self.orphans.is_empty()
    }
}

/// Whether a repository names a registry host, per Docker's own rule: the first path
/// segment is a host only if it carries a dot, a port, or is `localhost`. Everything
/// else — `postgres`, `bitnami/postgresql`, `library/redis` — is a Docker Hub name,
/// which on a shared host belongs to whoever pulled it.
fn names_a_registry_host(repository: &str) -> bool {
    let first_segment = repository.split('/').next().unwrap_or(repository);
    first_segment.contains('.') || first_segment.contains(':') || first_segment == "localhost"
}

/// The repository half of an image reference: everything before the tag, with any
/// `@sha256:…` digest dropped. A `:` only separates a tag when nothing after it is a
/// path separator — otherwise it is a registry port (`localhost:5000/app`).
fn repository_of(reference: &str) -> String {
    let without_digest = reference.split('@').next().unwrap_or(reference);
    match without_digest.rfind(':') {
        Some(colon) if !without_digest[colon + 1..].contains('/') => without_digest[..colon].to_string(),
        _ => without_digest.to_string(),
    }
}

pub struct Engine<'a> {
    cfg: Config,
    runner: &'a dyn CommandRunner,
    fs: &'a dyn FileSystem,
    clock: &'a dyn Clock,
    reporter: &'a Reporter,
    interrupt: &'a Interrupt,
    lua: Option<&'a LuaHost>,
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
        cfg: Config,
        runner: &'a dyn CommandRunner,
        fs: &'a dyn FileSystem,
        clock: &'a dyn Clock,
        reporter: &'a Reporter,
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
            interrupt,
            lua: None,
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

    pub fn with_plugins(mut self, lua: &'a LuaHost) -> Self {
        self.lua = Some(lua);
        self
    }

    pub fn into_state(self) -> State {
        self.state
    }

    /// Built on demand (not stored) so it always reflects the current `cfg`, which a
    /// plugin can mutate between steps.
    fn docker(&self) -> Docker<'_> {
        Docker::new(&self.cfg)
    }

    fn full_ref(&self, tag: &str) -> String {
        match &self.cfg.registry {
            Some(registry) => format!("{registry}:{tag}"),
            None => tag.to_string(),
        }
    }

    /// Every logical dcd itself puts on the host, and therefore bounds versions for:
    /// the release image, each managed service image, and the worker template's.
    /// Untouched entries in `docker.images` are nobody's to reclaim.
    fn gc_logicals(&self) -> Vec<String> {
        let mut logicals = vec!["app".to_string()];
        let service_images = self.cfg.docker.services.values().filter_map(|s| s.image.clone());
        let worker_image = self.cfg.workers.as_ref().map(|w| w.template.image.clone());
        for logical in service_images.chain(worker_image) {
            if !logicals.contains(&logical) {
                logicals.push(logical);
            }
        }
        logicals
    }

    fn keep_policy(&self) -> KeepPolicy {
        let retention = &self.cfg.retention;
        KeepPolicy {
            releases: retention.keep_releases,
            managed_images: retention.keep_managed_images,
            per_logical: retention.keep_images.clone(),
        }
    }

    fn resolved_images(&self) -> IndexMap<String, String> {
        self.cfg
            .docker
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
                    "{} has an incomplete release {pending}; run `dcd deploy --resume {}`, `dcd rollback {}`, or `dcd unlock {}` to accept it as-is",
                    self.cfg.stage, self.cfg.stage, self.cfg.stage, self.cfg.stage
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
                let pull = self.docker().pull(tag);
                if !self.try_run(&pull, Access::Mutate)?.success() {
                    return Err(DcdError::PreCutover(format!("target image {tag} not present and not pullable")));
                }
            }
        }
        self.ran_migrations = false;
        self.warn_env_drift(&target.env_keys, target.id);
        self.reporter.log(&format!(
            "rolling back {} -> release {} ({})",
            self.cfg.stage,
            target.id,
            target.app_image().unwrap_or("?")
        ));
        self.drive(ROLLBACK_STEPS)
    }

    /// What `dcd gc` would remove. `sweep_all` additionally asks Docker what sits in
    /// the repositories this config resolves to and proposes tags no stage records —
    /// the only path that reclaims images pulled before dcd kept a ledger, and the
    /// only one that infers ownership, which is why it never runs unattended.
    pub fn gc_plan(&self, sweep_all: bool) -> Result<GcPlan> {
        let keep = self.keep_policy();
        let logicals = self.gc_logicals();
        let recorded = self.state.images_to_gc(&self.cfg.stage, &keep, &logicals);
        let mut plan = GcPlan {
            recorded,
            orphans: Vec::new(),
            protected: Vec::new(),
        };
        if !sweep_all {
            return Ok(plan);
        }

        let repositories = self.owned_repositories()?;
        let recorded_anywhere = self.state.all_recorded_tags();
        let in_use = self.container_images()?;
        for repository in &repositories {
            for tag in self.host_tags(repository)? {
                if plan.recorded.contains(&tag) {
                    continue;
                }
                if recorded_anywhere.contains(&tag) {
                    plan.protected.push((tag, "a release or the pull ledger still records it".to_string()));
                } else if in_use.contains(&tag) {
                    plan.protected.push((tag, "a container references it".to_string()));
                } else {
                    plan.orphans.push(tag);
                }
            }
        }
        Ok(plan)
    }

    /// Remove a plan's tags, best-effort: Docker refusing a tag that is still
    /// referenced is the correct outcome and never fails the command.
    pub fn gc(&mut self, plan: &GcPlan) -> Result<usize> {
        let proposed = plan.removals();
        let removed = self.remove_images(&proposed);
        let stage = self.cfg.stage.clone();
        let keep = self.keep_policy();
        // gc removes images only; a release whose container is still around stays
        // evictable, so only rows whose container is already gone are settled here
        let evicted = self.state.stage(&stage).map(|s| s.evictions(keep.releases)).unwrap_or_default();
        let gone = self.containers_absent(&evicted);
        self.state.stage_mut(&stage).forget_pulled(&removed);
        self.state.stage_mut(&stage).mark_reaped(&gone, &proposed, &removed);
        self.persist_state()?;
        Ok(removed.len())
    }

    /// Of `containers`, those Docker no longer has. `dcd gc` never removes containers, so
    /// it may only settle rows whose container a previous run already tore down. One
    /// listing, not one probe per container; a listing that fails settles nothing.
    fn containers_absent(&self, containers: &[String]) -> Vec<String> {
        let prefix = format!("{}-", self.cfg.release.container_prefix);
        let argv = self.docker().ps_names(&prefix, true);
        let Ok(out) = self.try_run(&argv, Access::Read) else {
            return Vec::new();
        };
        if !out.success() {
            return Vec::new();
        }
        let present: Vec<&str> = out.stdout.lines().map(str::trim).filter(|n| !n.is_empty()).collect();
        containers
            .iter()
            .filter(|container| !present.contains(&container.as_str()))
            .cloned()
            .collect()
    }

    /// `docker image rm` each tag, returning those Docker confirmed gone. A refusal
    /// (still referenced) is reported and the tag stays known.
    fn remove_images(&self, tags: &[String]) -> Vec<String> {
        let mut removed: Vec<String> = Vec::new();
        for tag in tags {
            let rm = self.docker().image_rm(tag);
            let Ok(out) = self.try_run(&rm, Access::Mutate) else {
                self.reporter.warn(&format!("kept {tag}: docker could not be run"));
                continue;
            };
            // an image that is already gone is the outcome we wanted: counting it as
            // removed stops the same tag being re-proposed, and warned about, forever
            let already_gone = out.stderr.contains("No such image");
            if out.success() || already_gone {
                if out.success() {
                    self.reporter.log(&format!("removed {tag}"));
                }
                removed.push(tag.clone());
            } else {
                self.reporter.warn(&format!("kept {tag}: {}", out.stderr.trim()));
            }
        }
        removed
    }

    /// The repositories this project+stage publishes to, skipping any that cannot be
    /// shown to belong to it. An explicit `registry:` is the operator naming their own
    /// prefix, so it is trusted unless it is a bare Docker Hub name; a repository
    /// merely inferred from an image reference must name a registry host, or
    /// `bitnami/postgresql` would look as much "ours" as `ghcr.io/us/app`. Only
    /// logicals dcd itself puts on the host are considered. Skipping is per
    /// repository — one public image must not disable the sweep for our own.
    fn owned_repositories(&self) -> Result<Vec<String>> {
        let declared = self.cfg.registry.is_some();
        let references: Vec<String> = match &self.cfg.registry {
            Some(registry) => vec![registry.clone()],
            None => self
                .gc_logicals()
                .iter()
                .filter_map(|logical| self.cfg.docker.images.get(logical).cloned())
                .collect(),
        };
        let mut repositories: Vec<String> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        for reference in &references {
            let repository = repository_of(reference);
            let has_host = names_a_registry_host(&repository);
            let is_ours = if declared { has_host || repository.contains('/') } else { has_host };
            if !is_ours {
                if !skipped.contains(&repository) {
                    self.reporter.warn(&format!(
                        "not sweeping '{repository}': a Docker Hub name cannot be shown to belong to {}",
                        self.cfg.project
                    ));
                    skipped.push(repository);
                }
                continue;
            }
            if !repositories.contains(&repository) {
                repositories.push(repository);
            }
        }
        if repositories.is_empty() {
            return Err(DcdError::Config(format!(
                "refusing to sweep: no repository of {} can be shown to belong to it (set `registry:` to one you own)",
                self.cfg.project
            )));
        }
        Ok(repositories)
    }

    fn host_tags(&self, repository: &str) -> Result<Vec<String>> {
        let argv = self.docker().images_in(repository);
        let out = self.read(&argv)?;
        let tags = out
            .stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.ends_with(":<none>"))
            .map(String::from)
            .collect();
        Ok(tags)
    }

    fn container_images(&self) -> Result<HashSet<String>> {
        let argv = self.docker().container_images();
        let out = self.read(&argv)?;
        let images = out
            .stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(String::from)
            .collect();
        Ok(images)
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
        self.warn_env_drift(&pending.env_keys, pending.id);
        self.post_cutover = true;
        self.drive(RESUME_STEPS)
    }

    /// The `unlock` escape hatch (spec §4.4): accept the recorded `cutover_pending`
    /// release as the outcome of the deploy, so the stage is deployable again. It is a
    /// pure state repair — no container, image, or hook is touched, so nothing that
    /// already failed can block it. The leftovers are the next deploy's job: it reaps
    /// every stale app container in `drain:red` and applies retention in `finalize`.
    /// Returns the promoted container, or `None` when the stage has no incomplete release.
    pub fn unlock(&mut self) -> Result<Option<String>> {
        let pending = self.stage().and_then(|s| s.newest_cutover_pending()).cloned();
        let Some(pending) = pending else {
            return Ok(None);
        };

        let stage = self.cfg.stage.clone();
        let serving_before = self.stage().and_then(|s| s.current.clone());
        let demoted = self.state.stage_mut(&stage).demote_other_pending_releases(&pending.container);
        if !demoted.is_empty() {
            self.reporter
                .warn(&format!("unlock: demoted stale incomplete release(s) {}", demoted.join(", ")));
        }
        if let Some(reason) = self.opts.reason.clone() {
            self.state.stage_mut(&stage).set_reason(&pending.container, reason);
        }

        self.reporter
            .log(&format!("unlock: accepting {} as the release of record", pending.container));
        self.state
            .stage_mut(&stage)
            .finalize(&pending.container, FinalizeKind::Deploy, serving_before.as_deref());
        self.persist_state()?;
        self.warn_unlock_left_behind(serving_before.as_deref());

        Ok(Some(pending.container))
    }

    /// What `unlock` deliberately left alone — the promoted release is live without the
    /// post-cutover work the recipe would have done, and the next deploy is what cleans up.
    fn warn_unlock_left_behind(&self, serving_before: Option<&str>) {
        let runs_after_migration = self.cfg.release.migrate.as_ref().is_some_and(|m| m.after.is_some());
        if runs_after_migration {
            self.reporter
                .warn("unlock: `migrate:after` was NOT run — run it yourself if the release needs it");
        }
        if self.cfg.workers.is_some() {
            self.reporter
                .warn("unlock: workers were NOT recreated — they still run the previous release");
        }
        if let Some(previous) = serving_before {
            self.reporter
                .warn(&format!("unlock: {previous} was left running — the next deploy drains it"));
        }
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
        self.fire_hooks(&format!("before_{key}"))?;
        let started = Instant::now();
        let outcome = self.dispatch(name)?;
        let ms = started.elapsed().as_millis() as u64;
        match outcome {
            Outcome::Done(detail) => self.reporter.task(name, Status::Ok, ms, detail.as_deref()),
            Outcome::Skipped => self.reporter.task(name, Status::Skip, ms, None),
        }
        self.fire_hooks(&format!("after_{key}"))?;
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
        let inspect = self.docker().network_inspect();
        if !self.read(&inspect)?.success() {
            let create = self.docker().network_create();
            self.exec(&create, Access::Mutate)?;
        }
        let dirs = self.cfg.directories.iter().map(|d| (d.path.clone(), d.owner.clone())).collect::<Vec<_>>();
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
        Ok(Outcome::Done(None))
    }

    fn ensure_upstream(&mut self) -> Result<Outcome> {
        let current = self.stage().and_then(|s| s.current.clone());
        let dead = match &current {
            Some(container) => {
                let argv = self.docker().is_running(container);
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
        let mut pulls: Vec<(String, String)> = vec![("app".to_string(), app)];
        for logical in self.gc_logicals().into_iter().skip(1) {
            if let Some(tag) = self.images.get(&logical).cloned() {
                pulls.push((logical, tag));
            }
        }
        self.record_pulls(&pulls)?;
        for (_, tag) in &pulls {
            let argv = self.docker().pull(tag);
            self.exec(&argv, Access::Mutate)?;
        }
        Ok(Outcome::Done(None))
    }

    /// Write the pull ledger before pulling, so a deploy that dies before `finalize`
    /// still leaves every tag it dropped on this host reclaimable (spec §7.13). Purely
    /// additive: `releases` and `current` are untouched, so recovery reads the same
    /// state it would have without this write (INV-3).
    fn record_pulls(&mut self, pulls: &[(String, String)]) -> Result<()> {
        let stage = self.cfg.stage.clone();
        let at = self.clock.now_epoch();
        let ledger = self.state.stage_mut(&stage);
        for (logical, tag) in pulls {
            if tag.is_empty() {
                continue;
            }
            ledger.record_pull(logical, tag, at);
        }
        self.persist_state()
    }

    fn infra(&mut self) -> Result<Outcome> {
        let services: Vec<String> = self.cfg.docker.services.keys().cloned().collect();
        for name in &services {
            let service = &self.cfg.docker.services[name];
            let container = service.container.clone();
            let recreate = service.recreate;
            let drain_first = service.on_recreate_drain_workers;
            let desired = service.image.as_ref().and_then(|l| self.images.get(l)).cloned();

            let needs_recreate = match recreate {
                Recreate::Always => true,
                Recreate::Never => false,
                Recreate::OnImageChange => {
                    let inspect = self.docker().inspect_image(&container);
                    let current = self.read(&inspect)?;
                    let current_image = current.stdout.trim();
                    desired.as_deref().map(|d| d != current_image).unwrap_or(false)
                }
            };

            if needs_recreate && drain_first && !self.drained {
                self.drain_workers()?;
            }
            let argv = if needs_recreate {
                self.docker().compose(&["up", "-d", name], false)
            } else {
                self.docker().compose(&["up", "-d", "--no-recreate", name], false)
            };
            self.exec_env(&argv, Access::Mutate, self.compose_overlay())?;
        }

        for name in &services {
            let wait = match &self.cfg.docker.services[name].wait {
                Some(wait) => wait,
                None => continue,
            };
            let exec_in = wait.exec_in.clone();
            let cmd = wait.cmd.clone();
            let retries = wait.retries;
            let interval = wait.interval;
            let argv = self.docker().exec_sh(&exec_in, &cmd);
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
        let env_keys = self.release_env_keys()?;
        let argv = self.docker().run_throwaway(&name, &app, &args, &env_keys);
        self.exec_env(&argv, Access::Mutate, self.run_overlay())?;
        Ok(Outcome::Done(None))
    }

    fn start_black(&mut self) -> Result<Outcome> {
        let exists = self.docker().ps_names(&self.container, true);
        if !self.read(&exists)?.stdout.trim().is_empty() {
            return Err(DcdError::PreCutover(format!("container {} already exists", self.container)));
        }
        let app = self.images.get("app").cloned().unwrap_or_default();
        let env_keys = self.release_env_keys()?;
        let argv = self.docker().run_black(&self.container, &app, &env_keys);
        self.exec_env(&argv, Access::Mutate, self.run_overlay())?;
        self.black_started = true;
        Ok(Outcome::Done(Some(self.container.clone())))
    }

    fn healthcheck(&mut self) -> Result<Outcome> {
        let hc = &self.cfg.release.healthcheck;
        let cmd = hc.cmd.replace("{container}", &self.container);
        let exec_in = hc.exec_in.clone();
        let retries = hc.retries;
        let interval = hc.interval;
        let argv = self.docker().exec_sh(&exec_in, &cmd);
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
            last_stderr.trim()
        )))
    }

    fn cutover(&mut self) -> Result<Outcome> {
        let upstream = self.resolve(&self.cfg.cutover.upstream_file);
        let previous = self.fs.read(&upstream).ok();
        let backend = format!("{}:{}", self.container, self.cfg.cutover.backend_port);
        let rendered = self.cfg.cutover.template.replace("{backend}", &backend);
        self.fs_write(&upstream, rendered.as_bytes(), None, "upstream cutover")?;

        if let Some(validate) = &self.cfg.cutover.validate {
            let argv = self.docker().exec_sh(&validate.exec_in, &validate.cmd);
            let out = self.try_run(&argv, Access::Mutate)?;
            if !out.success() {
                self.restore_upstream(&upstream, previous);
                return Err(DcdError::PreCutover(format!(
                    "cutover config validation failed: {}",
                    out.stderr.trim()
                )));
            }
        }

        let reload = self.docker().exec_sh(&self.cfg.cutover.reload.exec_in, &self.cfg.cutover.reload.cmd);
        let out = self.try_run(&reload, Access::Mutate)?;
        if !out.success() {
            self.restore_upstream(&upstream, previous);
            return Err(DcdError::PreCutover(format!(
                "router reload failed; restored previous upstream: {}",
                out.stderr.trim()
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
            env_keys: self.release_env_keys()?,
            reaped: false,
        };
        let stage = self.cfg.stage.clone();
        self.state.stage_mut(&stage).record_cutover(release);
        self.post_cutover = true;
        self.persist_state()?; // INV-3: the live black must be recoverable from disk
        Ok(Outcome::Done(Some("router reloaded".to_string())))
    }

    fn drain_red(&mut self) -> Result<Outcome> {
        let prefix = format!("{}-", self.cfg.release.container_prefix);
        let argv = self.docker().ps_names(&prefix, false);
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
                let drain = self.docker().exec_sh(&target, cmd);
                let _ = self.try_run(&drain, Access::Mutate);
            }
            let rm = self.docker().rm_f(&target);
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
        let argv = self.docker().exec_args(&self.container, &args);
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
        let template = &workers.template;
        let env_keys =
            self.delivered_env_keys(&template.env_include, &template.env_exclude, &template.env)?;
        let compose_yaml = self.render_workers(workers, &names, &env_keys);
        let path = self.resolve(&workers.compose_file);
        self.fs_write(&path, compose_yaml.as_bytes(), None, "workers compose")?;
        let mut args: Vec<String> = vec!["up".into(), "-d".into()];
        for name in &names {
            args.push(format!("{}{}", workers.name_filter, name));
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let argv = self.docker().compose(&arg_refs, true);
        self.exec_env(&argv, Access::Mutate, self.workers_up_overlay(template))?;
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
        let keep = self.keep_policy();
        let logicals = self.gc_logicals();
        self.state.stage_mut(&stage).finalize(&container, kind, serving_before.as_deref());
        let evictions = self.state.stage(&stage).map(|s| s.evictions(keep.releases)).unwrap_or_default();
        let images = self.state.images_to_gc(&stage, &keep, &logicals);
        self.persist_state()?;
        // only containers Docker confirmed gone are reaped below: a removal that really
        // failed must stay evictable, or a stuck container would never be retried
        let mut torn_down: Vec<String> = Vec::new();
        for container in &evictions {
            let rm = self.docker().rm_f(container);
            if let Ok(out) = self.try_run(&rm, Access::Mutate) {
                if out.success() {
                    torn_down.push(container.clone());
                }
            }
        }
        let removed = self.remove_images(&images);
        // only tags Docker confirmed gone leave the ledger: one it refused is still on
        // the host, and forgetting it would make it invisible again (INV-11)
        self.state.stage_mut(&stage).forget_pulled(&removed);
        // mark the evicted releases settled, or `evictions`/`gc_candidates` re-derive the
        // same dead containers and tags from `releases` on every subsequent deploy
        self.state.stage_mut(&stage).mark_reaped(&torn_down, &images, &removed);
        // the release is already recorded and live; failing the deploy over unpruned
        // bookkeeping would report a successful cutover as a failure with nothing to resume
        if self.persist_state().is_err() {
            self.reporter.warn("pull ledger not pruned; the next deploy or `dcd gc` retries it");
        }
        Ok(Outcome::Done(Some(format!("current = {}", self.container))))
    }


    /// Fire a slot's YAML actions, then its Lua hooks. Lua hooks see a `ctx.cfg`/`ctx.state`
    /// refreshed from the engine's current state; any direct mutation is read back into the
    /// typed config/state so subsequent steps honor it (plugins get full, transparent power).
    fn fire_hooks(&mut self, slot: &str) -> Result<()> {
        for action in self.cfg.hooks.get(slot).into_iter().flatten() {
            self.run_action(action)?;
        }
        let Some(lua) = self.lua else { return Ok(()) };
        if !lua.has_hook(slot) {
            return Ok(());
        }
        let stage = self.state.stage(&self.cfg.stage).cloned().unwrap_or_default();
        lua.refresh(&self.cfg, &stage).map_err(|e| self.classify(e))?;
        let cfg_before = lua.read_cfg().map_err(|e| self.classify(e))?;
        let state_before = lua.read_state().map_err(|e| self.classify(e))?;
        lua.fire(self, slot).map_err(|e| self.classify(e))?;
        let cfg_after = lua.read_cfg().map_err(|e| self.classify(e))?;
        let state_after = lua.read_state().map_err(|e| self.classify(e))?;
        if cfg_after != cfg_before {
            self.apply_synced_cfg(cfg_after)?;
        }
        if state_after != state_before {
            self.apply_synced_state(state_after)?;
        }
        Ok(())
    }

    /// Adopt a `ctx.cfg` a plugin mutated: re-derive and re-validate the typed config from
    /// the live Lua table. A validation failure aborts the deploy.
    fn apply_synced_cfg(&mut self, value: serde_yaml::Value) -> Result<()> {
        self.cfg = crate::config::from_lua_value(value, &self.cfg.stage).map_err(|e| self.classify(e.to_string()))?;
        Ok(())
    }

    /// Adopt a `ctx.state` a plugin mutated into the current stage. Persisted immediately
    /// once past cutover so the change survives a crash (matching INV-3).
    fn apply_synced_state(&mut self, value: serde_yaml::Value) -> Result<()> {
        let mut stage: crate::state::StageState =
            serde_yaml::from_value(value).map_err(|e| self.classify(format!("ctx.state: {e}")))?;
        let key = self.cfg.stage.clone();
        // the pull ledger is dcd's record of what it put on this host, not a plugin's
        // to edit: a round-trip that dropped it would make those tags unreclaimable
        stage.pulled = self.state.stage_mut(&key).pulled.clone();
        *self.state.stage_mut(&key) = stage;
        if self.post_cutover {
            self.persist_state()?;
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
                self.exec(&self.docker().exec_sh(&exec_in.service, &exec_in.cmd), Access::Mutate)
            }
            HookAction::ExecInRelease { exec_in_release } => {
                self.exec(&self.docker().exec_sh(&self.container, exec_in_release), Access::Mutate)
            }
            HookAction::Docker { docker } => {
                let mut argv = vec!["docker".to_string()];
                argv.extend(docker.iter().cloned());
                self.exec(&Argv(argv), Access::Mutate)
            }
            HookAction::Compose { compose } => {
                let refs: Vec<&str> = compose.iter().map(String::as_str).collect();
                self.exec_env(&self.docker().compose(&refs, false), Access::Mutate, self.compose_overlay())
            }
            HookAction::CpFromRelease { cp_from_release } => {
                let src = format!("{}:{}", self.container, cp_from_release.from);
                let dst = self.resolve(Path::new(&cp_from_release.to)).display().to_string();
                self.exec(&self.docker().cp(&src, &dst), Access::Mutate)
            }
            HookAction::CpToRelease { cp_to_release } => {
                let src = self.resolve(Path::new(&cp_to_release.from)).display().to_string();
                let dst = format!("{}:{}", self.container, cp_to_release.to);
                self.exec(&self.docker().cp(&src, &dst), Access::Mutate)
            }
        }
        .map(|_| ())
    }


    fn stage(&self) -> Option<&crate::state::StageState> {
        self.state.stage(&self.cfg.stage)
    }

    fn delivered_env_keys(
        &self,
        include: &[String],
        exclude: &[String],
        explicit: &IndexMap<String, String>,
    ) -> Result<Vec<String>> {
        crate::dotenv::delivered_keys(&self.opts.container_env, include, exclude, explicit.keys())
            .map_err(|e| self.classify(e.to_string()))
    }

    fn release_env_keys(&self) -> Result<Vec<String>> {
        let run = &self.cfg.release.run;
        self.delivered_env_keys(&run.env_include, &run.env_exclude, &run.env)
    }

    fn overlay(env: &IndexMap<String, String>) -> Option<BTreeMap<String, String>> {
        if env.is_empty() {
            return None;
        }
        Some(env.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
    }

    fn compose_overlay(&self) -> Option<BTreeMap<String, String>> {
        Self::overlay(&self.cfg.compose.env)
    }

    fn run_overlay(&self) -> Option<BTreeMap<String, String>> {
        Self::overlay(&self.cfg.release.run.env)
    }

    /// The workers `compose up` carries compose.env (for `${VAR}` substitution in
    /// compose files) with `template.env` layered over it — spec §5.2.4 precedence.
    fn workers_up_overlay(
        &self,
        template: &crate::config::WorkerTemplate,
    ) -> Option<BTreeMap<String, String>> {
        let mut overlay = self.compose_overlay().unwrap_or_default();
        overlay.extend(template.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        if overlay.is_empty() {
            return None;
        }
        Some(overlay)
    }

    /// Warn loudly when a recorded release's delivered key set differs from what the
    /// current chain would deliver — env is not versioned (spec §5.2.5).
    fn warn_env_drift(&self, recorded: &[String], release_id: u64) {
        if recorded.is_empty() {
            return; // pre-rework release: nothing was recorded
        }
        let Ok(current) = self.release_env_keys() else { return };
        let added: Vec<&String> = current.iter().filter(|k| !recorded.contains(k)).collect();
        let removed: Vec<&String> = recorded.iter().filter(|k| !current.contains(k)).collect();
        if added.is_empty() && removed.is_empty() {
            return;
        }
        let describe = |keys: &[&String], sign: char| -> String {
            keys.iter().map(|k| format!("{sign}{k}")).collect::<Vec<_>>().join(", ")
        };
        self.reporter.warn(&format!(
            "release {release_id} ran with different env keys than the current chain delivers ({})",
            [describe(&added, '+'), describe(&removed, '-')]
                .iter()
                .filter(|s| !s.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
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
        let argv = self.docker().exec_sh(&self.container, command);
        let out = self.try_run(&argv, Access::Mutate)?;
        Ok(out
            .stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.contains('.') && !workers.provider.exclude.iter().any(|e| e == line))
            .map(String::from)
            .collect())
    }

    /// The generated file carries env key NAMES only (compose bare-key passthrough);
    /// values ride the workers-`up` command env — no secret bytes on disk (spec §5.2.4).
    fn render_workers(&self, workers: &crate::config::Workers, names: &[String], env_keys: &[String]) -> String {
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
            if !env_keys.is_empty() {
                yaml.push_str("        environment:\n");
                for key in env_keys {
                    yaml.push_str(&format!("            - {key}\n"));
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
        let argv = self.docker().worker_ps_names(&name_filter);
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
                let drain = self.docker().exec_sh(name, cmd);
                let _ = self.try_run(&drain, Access::Mutate);
            }
        }
        let stop = self.docker().stop(&names, timeout);
        let _ = self.try_run(&stop, Access::Mutate);
        self.drained = true;
        Ok(())
    }

    fn reap_orphans(&mut self) -> Result<()> {
        let prefix = format!("{}-", self.cfg.release.container_prefix);
        let argv = self.docker().ps_names(&prefix, true);
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
            let rm = self.docker().rm_f(&orphan);
            let _ = self.try_run(&rm, Access::Mutate);
        }
        Ok(())
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
            let rm = self.docker().rm_f(&self.container);
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

    fn run_argv(
        &self,
        argv: &Argv,
        access: Access,
        check: bool,
        env: Option<BTreeMap<String, String>>,
    ) -> Result<crate::effects::CmdOutput> {
        let stubbed = self.opts.dry_run && access == Access::Mutate;
        if stubbed {
            self.reporter.plan(&argv.display());
            return self
                .runner
                .run(argv, access, &RunOpts { check, env })
                .map_err(|e| self.classify(e.to_string()));
        }

        self.reporter.command(&argv.display());
        let started = Instant::now();
        let outcome = self.runner.run(argv, access, &RunOpts { check, env });
        let ms = started.elapsed().as_millis() as u64;
        match &outcome {
            Ok(out) => self.reporter.command_output(out.code, ms, &out.stdout, &out.stderr),
            Err(err) => self.reporter.command_error(ms, &err.to_string()),
        }
        outcome.map_err(|e| self.classify(e.to_string()))
    }

    fn exec(&self, argv: &Argv, access: Access) -> Result<crate::effects::CmdOutput> {
        self.run_argv(argv, access, true, None)
    }

    fn exec_env(
        &self,
        argv: &Argv,
        access: Access,
        env: Option<BTreeMap<String, String>>,
    ) -> Result<crate::effects::CmdOutput> {
        self.run_argv(argv, access, true, env)
    }

    fn try_run(&self, argv: &Argv, access: Access) -> Result<crate::effects::CmdOutput> {
        self.run_argv(argv, access, false, None)
    }

    fn read(&self, argv: &Argv) -> Result<crate::effects::CmdOutput> {
        self.run_argv(argv, Access::Read, false, None)
    }

    fn classify(&self, message: String) -> DcdError {
        if self.post_cutover {
            DcdError::PostCutover(message)
        } else {
            DcdError::PreCutover(message)
        }
    }
}

impl HookHost for Engine<'_> {
    fn run_host(&self, cmd: &str) -> std::result::Result<String, String> {
        let full = format!("cd {} && {}", self.deploy_root.display(), cmd);
        self.exec(&Argv::of(["sh", "-c", &full]), Access::Mutate).map(|o| o.stdout).map_err(|e| e.to_string())
    }

    fn in_release(&self, cmd: &str) -> std::result::Result<String, String> {
        self.exec(&self.docker().exec_sh(&self.container, cmd), Access::Mutate).map(|o| o.stdout).map_err(|e| e.to_string())
    }

    fn exec_in(&self, service: &str, cmd: &str) -> std::result::Result<String, String> {
        self.exec(&self.docker().exec_sh(service, cmd), Access::Mutate).map(|o| o.stdout).map_err(|e| e.to_string())
    }

    fn docker(&self, args: Vec<String>) -> std::result::Result<String, String> {
        let mut argv = vec!["docker".to_string()];
        argv.extend(args);
        self.exec(&Argv(argv), Access::Mutate).map(|o| o.stdout).map_err(|e| e.to_string())
    }

    fn compose(&self, args: Vec<String>) -> std::result::Result<String, String> {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.exec_env(&self.docker().compose(&refs, false), Access::Mutate, self.compose_overlay())
            .map(|o| o.stdout)
            .map_err(|e| e.to_string())
    }

    fn cp_from_release(&self, from: &str, to: &str) -> std::result::Result<(), String> {
        let src = format!("{}:{}", self.container, from);
        let dst = self.resolve(Path::new(to)).display().to_string();
        self.exec(&self.docker().cp(&src, &dst), Access::Mutate).map(|_| ()).map_err(|e| e.to_string())
    }

    fn cp_to_release(&self, from: &str, to: &str) -> std::result::Result<(), String> {
        let src = self.resolve(Path::new(from)).display().to_string();
        let dst = format!("{}:{}", self.container, to);
        self.exec(&self.docker().cp(&src, &dst), Access::Mutate).map(|_| ()).map_err(|e| e.to_string())
    }

    fn read_file(&self, path: &str) -> std::result::Result<String, String> {
        let resolved = self.resolve(Path::new(path));
        self.fs.read(&resolved).map(|b| String::from_utf8_lossy(&b).into_owned()).map_err(|e| e.to_string())
    }

    fn write_file(&self, path: &str, content: &str) -> std::result::Result<(), String> {
        let resolved = self.resolve(Path::new(path));
        self.fs_write(&resolved, content.as_bytes(), None, "plugin write").map_err(|e| e.to_string())
    }

    fn file_exists(&self, path: &str) -> bool {
        self.fs.exists(&self.resolve(Path::new(path)))
    }

    fn env(&self, name: &str) -> Option<String> {
        self.opts.interpolation_env.get(name).cloned()
    }

    fn log(&self, message: &str) {
        self.reporter.log(message);
    }

    fn warn(&self, message: &str) {
        self.reporter.warn(message);
    }

    fn container(&self) -> String {
        self.container.clone()
    }

    fn stage(&self) -> String {
        self.cfg.stage.clone()
    }
}

fn json_array(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|i| format!("'{i}'")).collect();
    format!("[{}]", quoted.join(", "))
}

#[cfg(test)]
mod tests;
