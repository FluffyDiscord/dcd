//! The red-black recipe: a fixed ordered sequence of steps with before/after hook
//! slots (spec §3, §7). Every side effect goes through the effects seam, so a full
//! deploy is asserted against the recorded argv with no Docker.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use indexmap::IndexMap;

use crate::compose::ComposeModel;
use crate::config::{Config, HookAction, Recreate};
use crate::docker::Docker;
use crate::effects::{Access, Argv, Clock, CommandRunner, FileSystem, RunOpts};
use crate::error::{DcdError, Result};
use crate::lua::{HookHost, LuaHost};
use crate::signal::Interrupt;
use crate::state::{FinalizeKind, KeepPolicy, Release, ReleaseStatus, State};
use crate::ui::{Reporter, Status};

pub const DEPLOY_STEPS: &[&str] = &[
    "sync",
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
    "sync",
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

// `workers` runs `compose run` against the uploaded files, so resume syncs first.
const RESUME_STEPS: &[&str] = &["sync", "drain:red", "migrate:after", "workers", "finalize"];

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
    /// Whether the target is reached over ssh. Only `sync` cares: locally the
    /// compose files are already where they need to be.
    pub remote: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            dry_run: false,
            sleep_enabled: true,
            reason: None,
            container_env: BTreeMap::new(),
            interpolation_env: HashMap::new(),
            remote: false,
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
    /// The operator's compose file, resolved. Under ADR-013 this is where every
    /// container fact lives: names, images, health gates, restart policies.
    model: ComposeModel,

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
        model: ComposeModel,
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
            model,
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

    /// Services dcd itself pulls, and therefore bounds versions for: the release
    /// service, every policy-managed service, and the worker service. Anything
    /// else in the compose file is nobody's to reclaim.
    fn gc_services(&self) -> Vec<String> {
        let mut services = vec![self.cfg.release.service_name(&self.cfg.project)];
        let managed = self.cfg.services.keys().cloned();
        let worker = self.cfg.workers.as_ref().map(|w| w.service.clone());
        for service in managed.chain(worker) {
            if !services.contains(&service) {
                services.push(service);
            }
        }
        services
    }

    fn keep_policy(&self) -> KeepPolicy {
        let retention = &self.cfg.retention;
        KeepPolicy {
            releases: retention.keep_releases,
            managed_images: retention.keep_managed_images,
            per_logical: retention.keep_images.clone(),
        }
    }

    /// Images come from the resolved compose model (ADR-013), keyed by service
    /// name — the identity the pull ledger and retention are built on (spec §5.4).
    fn resolved_images(&self) -> IndexMap<String, String> {
        self.gc_services()
            .into_iter()
            .filter_map(|service| {
                let image = self.model.service(&service)?.image.clone()?;
                Some((service, image))
            })
            .collect()
    }

    /// The container name compose gave a service. `compose ps` deliberately is not
    /// consulted for the release container — compose hides one-off containers
    /// from it (INV-13).
    fn container_of(&self, service: &str) -> Result<String> {
        let declared = self.model.require_service(service, "exec_in")?;
        Ok(match &declared.container_name {
            Some(name) => name.clone(),
            None => format!("{}-{service}-1", self.cfg.project),
        })
    }

    fn begin(&mut self, mode: Mode) {
        self.mode = mode;
        self.release_id = self.clock.now_epoch();
        self.container = format!("{}-{}", self.cfg.release.container_prefix(&self.cfg.project), self.release_id);
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
        if self.stage().map(|s| s.pending_count()).unwrap_or(0) > 1 {
            return Err(DcdError::Config(
                "state has more than one cutover_pending release — refusing".to_string(),
            ));
        }
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
        let logicals = self.gc_services();
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
        let prefix = format!("{}-", self.cfg.release.container_prefix(&self.cfg.project));
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
            None => self.resolved_images().values().cloned().collect(),
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
        let listing = self.list(&argv)?;
        let images = listing
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
            "sync" => self.sync(),
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


    /// Puts the operator's compose documents and the generated image override on
    /// the target (spec §7.0). Every write goes through the dry-run-gated
    /// `fs_write`, so `--dry-run` prints the uploads and touches nothing.
    ///
    /// dcd uploads the compose DOCUMENTS and nothing they reference: a relative
    /// bind-mount source, an `env_file:` or a build context must already be there.
    fn sync(&mut self) -> Result<Outcome> {
        // Always written, local or remote: `compose(...)` passes `-f` for it
        // unconditionally, so a missing file would break every compose call.
        let override_path = self.resolve(Path::new(&self.docker().image_override_file()));
        let document = self.render_image_override();
        self.fs_write(&override_path, document.as_bytes(), None, "image override")?;

        if !self.opts.remote {
            return Ok(Outcome::Done(Some("local: nothing to upload".to_string())));
        }

        let files = self.cfg.compose.files.clone();
        let mut uploaded = 0;
        for file in &files {
            let bytes = std::fs::read(file)
                .map_err(|e| DcdError::Config(format!("cannot read {}: {e}", file.display())))?;
            let target = self.resolve(file);
            // `infra/docker-compose.yml` uploads to {deploy_root}/infra/…, and the
            // write is a redirect — it fails on a fresh target unless the parent
            // is there first.
            if let Some(parent) = target.parent() {
                if parent != self.deploy_root {
                    self.mkdir(parent)?;
                }
            }
            self.fs_write(&target, &bytes, None, "compose file")?;
            uploaded += 1;
        }
        Ok(Outcome::Done(Some(format!("{uploaded} compose file(s) uploaded"))))
    }

    /// Pins each service's image to the exact ref this release resolved, so
    /// `--image` works whatever the compose file names its variables — and so a
    /// rollback replays the recorded image rather than whatever the environment
    /// says today (spec §5.5). Image references only: `compose config` inlines
    /// resolved env values, so anything richer would write secrets to disk.
    fn render_image_override(&self) -> String {
        let mut document = String::from("services:\n");
        for (service, image) in &self.images {
            document.push_str(&format!("  {service}:\n    image: {image}\n"));
        }
        if self.images.is_empty() {
            document.push_str("  {}\n");
        }
        document
    }

    fn preflight(&mut self) -> Result<Outcome> {
        for network in self.model.network_names() {
            let inspect = self.docker().network_inspect(&network);
            if !self.read(&inspect)?.success() {
                self.reporter
                    .warn(&format!("network {network} does not exist yet; compose will create it"));
            }
        }
        self.require_target_tools()?;
        let dirs = self
            .cfg
            .directories
            .iter()
            .map(|d| (d.path.clone(), d.owner.clone(), d.mode.clone()))
            .collect::<Vec<_>>();
        for (path, owner, mode) in dirs {
            let resolved = self.resolve(&path);
            self.mkdir(&resolved)?;
            let mount = format!("{}:/wd", self.deploy_root.display());
            let target = format!("/wd/{}", path.display());
            if let Some(owner) = owner {
                let argv = Argv::of(["docker", "run", "--rm", "-v", &mount, "busybox", "chown", &owner, &target]);
                self.exec(&argv, Access::Mutate)?;
            }
            // `mode` was accepted and documented but never applied, so a bind
            // target declared `0750` was created with whatever umask the target had.
            if let Some(mode) = mode {
                let argv = Argv::of(["docker", "run", "--rm", "-v", &mount, "busybox", "chmod", &mode, &target]);
                self.exec(&argv, Access::Mutate)?;
            }
        }
        self.reap_v1_workers()?;
        self.reap_orphans()?;
        Ok(Outcome::Done(None))
    }

    /// The two things dcd assumes the target has. Named here rather than
    /// discovered as a confusing failure twenty steps in: without `flock` the
    /// stage lock silently does not exist, and without `base64` every file dcd
    /// writes lands empty.
    fn require_target_tools(&self) -> Result<()> {
        if !self.opts.remote {
            return Ok(());
        }
        for tool in ["flock", "base64"] {
            // `command` is a shell builtin, so it has to be asked of a shell —
            // as a bare argv there is no binary to exec.
            let probe = format!("command -v {tool} >/dev/null 2>&1");
            let argv = Argv::of(["sh", "-c", &probe]);
            if !self.read(&argv)?.success() {
                return Err(DcdError::PreCutover(format!(
                    "{tool} is required on the target and was not found on PATH"
                )));
            }
        }
        Ok(())
    }

    fn ensure_upstream(&mut self) -> Result<Outcome> {
        let current = self.stage().and_then(|s| s.current.clone());
        // The same rule as the `exists` probe below, on the other input to the same
        // decision: a `docker ps` that could not RUN is not a container that is
        // gone. `read` does not check the exit code, so an empty stdout has to be
        // read together with a zero exit before calling the serving release dead.
        let dead = match &current {
            Some(container) => {
                let argv = self.docker().is_running(container);
                let out = self.read(&argv)?;
                if !out.success() {
                    return Err(DcdError::PreCutover(format!(
                        "cannot tell whether {container} is running (exit {}): {}",
                        out.code,
                        out.stderr.trim()
                    )));
                }
                out.stdout.trim().is_empty()
            }
            None => true,
        };
        let upstream = self.resolve(&self.cfg.cutover.upstream_file);
        // A probe that could not run is not a missing file: writing the fallback
        // over a live upstream would point the router away from the serving release.
        let present = self
            .fs
            .exists(&upstream)
            .map_err(|e| DcdError::PreCutover(format!("cannot check {}: {e}", upstream.display())))?;
        if dead || !present {
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
        // Two services routinely share one image (an app and its workers), and the
        // ledger is a census of TAGS on the host (INV-11) — so each distinct tag is
        // recorded and pulled once, under the first service that resolves it.
        let mut pulls: Vec<(String, String)> = Vec::new();
        for service in self.gc_services() {
            let Some(image) = self.images.get(&service).cloned() else {
                continue;
            };
            if pulls.iter().any(|(_, tag)| tag == &image) {
                continue;
            }
            pulls.push((service, image));
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

    /// `compose up --wait` blocks forever on a service whose healthcheck never goes
    /// healthy. Spec §7.4 bounds it, so a bad side container fails the deploy
    /// instead of hanging CI.
    fn wait_timeout_seconds() -> u64 {
        120
    }

    fn infra(&mut self) -> Result<Outcome> {
        let wait_timeout = Engine::wait_timeout_seconds().to_string();
        let services: Vec<String> = self.cfg.services.keys().cloned().collect();
        for name in &services {
            let policy = self.cfg.services[name].clone();
            let container = self.container_of(name)?;
            let desired = self.images.get(name).cloned();

            let needs_recreate = match policy.recreate {
                Recreate::Always => true,
                Recreate::Never => false,
                Recreate::OnImageChange => {
                    let inspect = self.docker().inspect_image(&container);
                    let current = self.read(&inspect)?;
                    let current_image = current.stdout.trim();
                    desired.as_deref().map(|d| d != current_image).unwrap_or(false)
                }
            };

            if needs_recreate && policy.on_recreate_drain_workers && !self.drained {
                self.drain_workers()?;
            }
            let argv = if needs_recreate {
                self.docker()
                    .compose(&["up", "-d", "--wait", "--wait-timeout", &wait_timeout, name])
            } else {
                self.docker()
                    .compose(&["up", "-d", "--no-recreate", "--wait", "--wait-timeout", &wait_timeout, name])
            };
            self.exec_env(&argv, Access::Mutate, self.compose_overlay())?;
        }

        // `compose up --wait` above already gated on each service's own compose
        // healthcheck; an explicit `wait:` is the override for services that
        // declare none (spec §7.4).
        for name in &services {
            let Some(wait) = self.cfg.services[name].wait.clone() else {
                continue;
            };
            let exec_in = wait.exec_in.clone().unwrap_or_else(|| name.clone());
            let container = self.container_of(&exec_in)?;
            let argv = self.docker().exec_sh(&container, &wait.cmd);
            self.poll(&argv, Access::Read, wait.retries, wait.interval, &format!("{name} not ready"))?;
        }
        Ok(Outcome::Done(None))
    }

    fn migrate_before(&mut self) -> Result<Outcome> {
        let Some(command) = self.cfg.release.migrate.as_ref().and_then(|m| m.before.clone()) else {
            return Ok(Outcome::Skipped);
        };
        let args: Vec<String> = command.split_whitespace().map(String::from).collect();
        let service = self.cfg.release.service_name(&self.cfg.project);
        let env_keys = self.release_env_keys()?;
        let argv = self.docker().run_throwaway(&service, &args, &env_keys);
        self.exec_env(&argv, Access::Mutate, self.run_overlay())?;
        Ok(Outcome::Done(None))
    }

    fn start_black(&mut self) -> Result<Outcome> {
        let exists = self.docker().ps_names(&self.container, true);
        if !self.read(&exists)?.stdout.trim().is_empty() {
            return Err(DcdError::PreCutover(format!("container {} already exists", self.container)));
        }
        let service = self.cfg.release.service_name(&self.cfg.project);
        let env_keys = self.release_env_keys()?;
        let argv = self.docker().run_black(&self.container, &service, &env_keys);
        self.exec_env(&argv, Access::Mutate, self.run_overlay())?;
        self.black_started = true;
        self.apply_restart_policy(&service, &self.container.clone())?;
        Ok(Outcome::Done(Some(self.container.clone())))
    }

    /// Compose forces `restart=no` on one-off containers, so whatever the operator
    /// declared has to be re-applied or the release will not survive a reboot.
    fn apply_restart_policy(&mut self, service: &str, container: &str) -> Result<()> {
        let Some(policy) = self.model.service(service).and_then(|s| s.restart_policy()) else {
            self.reporter.warn(&format!(
                "compose service {service} declares no restart policy; {container} will not restart after a reboot"
            ));
            return Ok(());
        };
        let argv = self.docker().update_restart(container, &policy);
        self.exec(&argv, Access::Mutate)?;
        Ok(())
    }

    /// Default gate: the compose service's own `healthcheck:`, polled through
    /// `docker inspect`. `compose run` has no `--health-cmd`, so dcd cannot inject
    /// a probe — the escape hatch covers images that cannot self-probe (spec §7.7).
    fn healthcheck(&mut self) -> Result<Outcome> {
        match self.cfg.release.healthcheck.clone() {
            Some(probe) => {
                let container = self.container_of(&probe.exec_in)?;
                let cmd = probe.cmd.replace("{container}", &self.container);
                let argv = self.docker().exec_sh(&container, &cmd);
                self.poll(&argv, Access::Mutate, probe.retries, probe.interval, "healthcheck never passed")?;
            }
            None => {
                let argv = self.docker().inspect_health(&self.container);
                self.poll_health(&argv, Self::get_health_retries(), Self::get_health_interval_seconds())?;
            }
        }
        Ok(Outcome::Done(None))
    }

    fn get_health_retries() -> u32 {
        60
    }

    fn get_health_interval_seconds() -> u64 {
        2
    }

    fn cutover(&mut self) -> Result<Outcome> {
        let upstream = self.resolve(&self.cfg.cutover.upstream_file);
        // Captured before the switch and required: this is what points the router
        // back at red if validate/reload fails. Silently proceeding without it
        // would leave the upstream naming a container the rollback just removed —
        // an outage that fires on the NEXT reload, long after dcd exited 1.
        let previous = match self.fs.read(&upstream) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                return Err(DcdError::PreCutover(format!(
                    "cannot read {} before the cutover: {e}",
                    upstream.display()
                )))
            }
        };
        let backend = format!("{}:{}", self.container, self.cfg.cutover.backend_port);
        let rendered = self.cfg.cutover.template.replace("{backend}", &backend);
        self.fs_write(&upstream, rendered.as_bytes(), None, "upstream cutover")?;

        if let Some(validate) = &self.cfg.cutover.validate {
            let container = self.container_of(&validate.exec_in)?;
            let argv = self.docker().exec_sh(&container, &validate.cmd);
            let out = self.try_run(&argv, Access::Mutate)?;
            if !out.success() {
                self.restore_upstream(&upstream, &previous);
                return Err(DcdError::PreCutover(format!(
                    "cutover config validation failed: {}",
                    out.stderr.trim()
                )));
            }
        }

        let router = self.container_of(&self.cfg.cutover.reload.exec_in)?;
        let reload = self.docker().exec_sh(&router, &self.cfg.cutover.reload.cmd);
        let out = self.try_run(&reload, Access::Mutate)?;
        if !out.success() {
            self.restore_upstream(&upstream, &previous);
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
        let prefix = format!("{}-", self.cfg.release.container_prefix(&self.cfg.project));
        let argv = self.docker().ps_names(&prefix, false);
        let running = self.list(&argv)?;
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

    /// N containers from ONE compose service (spec §7.12). The v1 renderer and its
    /// generated compose file are gone: per-worker arguments ride the `compose run`
    /// argv, overriding the service's `command` while keeping its entrypoint.
    fn workers(&mut self) -> Result<Outcome> {
        let Some(workers) = self.cfg.workers.clone() else {
            return Ok(Outcome::Skipped);
        };
        let names = self.worker_names(&workers)?;
        if names.is_empty() {
            return Ok(Outcome::Skipped);
        }
        let env_keys = self.release_env_keys()?;
        for name in &names {
            let container = format!("{}{}", workers.name_prefix, name);
            let argv = self
                .docker()
                .run_worker(&container, &workers.service, name, &workers.args, &env_keys);
            self.exec_env(&argv, Access::Mutate, self.compose_overlay())?;
            self.apply_restart_policy(&workers.service.clone(), &container)?;
        }
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
        let logicals = self.gc_services();
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
                let full = format!("cd {} && {}", crate::ssh::quote(&self.deploy_root.display().to_string()), cmd);
                self.exec(&Argv::of(["sh", "-c", &full]), Access::Mutate)
            }
            HookAction::ExecIn { exec_in } => {
                let container = self.container_of(&exec_in.service)?;
                self.exec(&self.docker().exec_sh(&container, &exec_in.cmd), Access::Mutate)
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
                self.exec_env(&self.docker().compose(&refs), Access::Mutate, self.compose_overlay())
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

    /// The chain keys delivered to the release, its migrate throwaway and its
    /// workers. With a compose service the filters have no dcd-side home, so the
    /// whole delivered chain rides bare `-e KEY`; the `run` fallback keeps them.
    fn release_env_keys(&self) -> Result<Vec<String>> {
        let empty = IndexMap::new();
        match &self.cfg.release.run {
            Some(run) => self.delivered_env_keys(&run.env_include, &run.env_exclude, &run.env),
            None => self.delivered_env_keys(&[], &[], &empty),
        }
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
        let mut overlay = self.compose_overlay().unwrap_or_default();
        if let Some(run) = &self.cfg.release.run {
            overlay.extend(run.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
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
            .filter(|line| !line.is_empty() && !line.contains('.') && !workers.exclude.iter().any(|e| e == line))
            .map(String::from)
            .collect())
    }

    /// The generated file carries env key NAMES only (compose bare-key passthrough);
    /// values ride the workers-`up` command env — no secret bytes on disk (spec §5.2.4).
    fn drain_workers(&mut self) -> Result<()> {
        let Some(workers) = &self.cfg.workers else {
            return Ok(());
        };
        let worker_service = workers.service.clone();
        let drain_cmd = workers.drain.clone();
        let timeout = workers.stop_timeout;
        let signal = workers.stop_signal.clone();
        let argv = self.docker().worker_ps_names(&worker_service);
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
        let stop = self.docker().stop(&names, timeout, &signal);
        let _ = self.try_run(&stop, Access::Mutate);
        // Removal, not just stopping, is the v2 contract: `compose run --name`
        // fails against a stopped container that still holds the name (spec §7.9).
        for name in &names {
            let rm = self.docker().rm_f(name);
            let _ = self.try_run(&rm, Access::Mutate);
        }
        self.drained = true;
        Ok(())
    }

    /// v1 rendered one compose SERVICE per worker (`worker-async`, `worker-sched`),
    /// so those containers carry `com.docker.compose.service=worker-async` — which
    /// v2's discovery filter (`service={workers.service}`) can never match. Left
    /// alone they are never drained or removed, and the first v2 deploy then
    /// collides on the container name post-cutover. Runs once, while the stage has
    /// no v2 release recorded.
    fn reap_v1_workers(&mut self) -> Result<()> {
        let Some(workers) = self.cfg.workers.clone() else {
            return Ok(());
        };
        if self.stage().is_some_and(|stage| !stage.releases.is_empty()) {
            return Ok(());
        }
        let argv = self.docker().labelled_ps_names(&workers.name_prefix);
        let listing = self.list(&argv)?;
        let stale: Vec<String> = listing
            .lines()
            .filter_map(|line| line.trim().split_once(' '))
            .filter(|(_, service)| service.trim() != workers.service)
            .map(|(name, _)| name.to_string())
            .collect();
        for name in &stale {
            self.reporter
                .warn(&format!("removing {name}, a v1 worker v2 discovery cannot see"));
            let rm = self.docker().rm_f(name);
            let _ = self.try_run(&rm, Access::Mutate);
        }
        Ok(())
    }

    fn reap_orphans(&mut self) -> Result<()> {
        let prefix = format!("{}-", self.cfg.release.container_prefix(&self.cfg.project));
        let argv = self.docker().ps_names(&prefix, true);
        let listing = self.list(&argv)?;
        let recorded = self.stage().map(|s| {
            s.releases
                .iter()
                .map(|r| r.container.clone())
                .collect::<HashSet<String>>()
        });
        let found: Vec<String> = listing
            .lines()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(String::from)
            .collect();

        // No stage row at all means the state file is missing, not that every
        // container is an orphan — and this runs at step 2 of 13, five steps
        // before a black exists. Reaping here would `docker rm -f` the container
        // currently serving traffic.
        let Some(known) = recorded else {
            if !found.is_empty() {
                return Err(DcdError::PreCutover(format!(
                    "{} has no recorded releases but {} container(s) match {prefix}: {} —                      refusing to reap what may be serving traffic; check dcd-state.json",
                    self.cfg.stage,
                    found.len(),
                    found.join(", ")
                )));
            }
            return Ok(());
        };

        for orphan in found.into_iter().filter(|name| !known.contains(name)) {
            self.reporter
                .warn(&format!("removing {orphan}, which no release in state accounts for"));
            let rm = self.docker().rm_f(&orphan);
            let _ = self.try_run(&rm, Access::Mutate);
        }
        Ok(())
    }

    /// One `dcd-state.json` holds every stage, but the stage lock only serialises
    /// runs of the SAME stage — so `dcd deploy prod` and `dcd deploy beta` against
    /// one deploy_root would each write the whole document and lose the other's
    /// rows, INV-3's `cutover_pending` record included. Re-reading and replacing
    /// only this stage's row keeps a concurrent stage's history intact.
    fn persist_state(&self) -> Result<()> {
        let path = self.state_path.clone();
        let mut document = self.state.clone();
        if let Ok(bytes) = self.fs.read(&path) {
            if let Ok(on_disk) = State::from_json(&bytes) {
                document = on_disk;
                if let Some(mine) = self.state.stage(&self.cfg.stage) {
                    document.stages.insert(self.cfg.stage.clone(), mine.clone());
                }
            }
        }
        self.fs_write(&path, document.to_json().as_bytes(), Some(0o600), "state")
    }

    /// §11's guarantee on the failed-cutover path: red serving, on-disk AND
    /// in-memory. A restore that fails leaves the router pointed at a container
    /// about to be removed, so it is reported rather than discarded.
    fn restore_upstream(&self, upstream: &Path, previous: &[u8]) {
        if !previous.is_empty() {
            if let Err(e) = self.fs_write(upstream, previous, None, "upstream restore") {
                self.reporter.warn(&format!(
                    "could not restore {} — the router still names the failed release: {e}",
                    upstream.display()
                ));
            }
        }
        self.cleanup_black();
    }

    fn cleanup_black(&self) {
        if self.black_started {
            let rm = self.docker().rm_f(&self.container);
            let _ = self.try_run(&rm, Access::Mutate);
        }
    }

    /// Spec §2.5: the executor checks the interrupt between tasks AND inside retry
    /// loops. A health gate is 60 attempts 2 s apart, so without this a Ctrl-C is
    /// noticed up to two minutes later — and reported as a failed deploy, not 130.
    fn abort_if_interrupted(&self) -> Result<()> {
        if self.interrupt.triggered() && !self.post_cutover {
            return Err(DcdError::Interrupted);
        }
        Ok(())
    }

    fn poll(&self, argv: &Argv, access: Access, retries: u32, interval: u64, fail: &str) -> Result<()> {
        if self.opts.dry_run {
            if !self.try_run(argv, access)?.success() {
                self.reporter.warn(&format!("{fail} (not satisfiable in dry-run)"));
            }
            return Ok(());
        }
        for attempt in 1..=retries {
            self.abort_if_interrupted()?;
            if self.try_run(argv, access)?.success() {
                return Ok(());
            }
            if attempt < retries {
                self.sleep(interval);
            }
        }
        Err(DcdError::PreCutover(fail.to_string()))
    }

    /// Docker reports health as a word, not an exit code: `starting` must keep
    /// polling, `unhealthy` must keep polling (the probe may still recover within
    /// its retries), and only `healthy` ends the wait.
    fn poll_health(&self, argv: &Argv, retries: u32, interval: u64) -> Result<()> {
        if self.opts.dry_run {
            self.reporter.warn("health status is not observable in dry-run");
            return Ok(());
        }
        for attempt in 1..=retries {
            self.abort_if_interrupted()?;
            let status = self.read(argv)?.stdout.trim().to_string();
            match status.as_str() {
                "healthy" => return Ok(()),
                "none" => {
                    return Err(DcdError::PreCutover(format!(
                        "container {} declares no healthcheck: give the compose service a `healthcheck:` \
                         or set release.healthcheck",
                        self.container
                    )))
                }
                _ => {}
            }
            if attempt < retries {
                self.sleep(interval);
            }
        }
        Err(DcdError::PreCutover("healthcheck never passed".to_string()))
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
                .run(argv, access, &RunOpts { check, env, stdin: None })
                .map_err(|e| self.classify_run_error(e));
        }

        self.reporter.command(&argv.display());
        let started = Instant::now();
        let outcome = self.runner.run(argv, access, &RunOpts { check, env, stdin: None });
        let ms = started.elapsed().as_millis() as u64;
        match &outcome {
            Ok(out) => self.reporter.command_output(out.code, ms, &out.stdout, &out.stderr),
            Err(err) => self.reporter.command_error(ms, &err.to_string()),
        }
        outcome.map_err(|e| self.classify_run_error(e))
    }

    fn classify_run_error(&self, error: crate::effects::RunError) -> DcdError {
        match error {
            crate::effects::RunError::Transport { .. } => self.classify_transport(error.to_string()),
            other => self.classify(other.to_string()),
        }
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

    /// A listing whose command FAILED is not an empty listing. Read through
    /// `read`, a broken `docker ps` yields empty stdout, which every caller then
    /// treats as "nothing to do" — so drain silently leaves red running, worker
    /// drain reports success, and `gc --all` sees no in-use images at all.
    fn list(&self, argv: &Argv) -> Result<String> {
        let out = self.read(argv)?;
        if !out.success() {
            return Err(self.classify(format!(
                "`{}` failed (exit {}): {}",
                argv.display(),
                out.code,
                out.stderr.trim()
            )));
        }
        Ok(out.stdout)
    }

    /// ssh's own failures keep their identity through the recipe: pre-cutover they
    /// exit 6 rather than 1, so CI can tell "the network broke" from "the deploy
    /// was rejected". Post-cutover, §11's rule wins — anything after the point of
    /// no return is exit 4, whatever caused it.
    fn classify_transport(&self, message: String) -> DcdError {
        if self.post_cutover {
            return DcdError::PostCutover(message);
        }
        DcdError::Transport(message)
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
        let full = format!("cd {} && {}", crate::ssh::quote(&self.deploy_root.display().to_string()), cmd);
        self.exec(&Argv::of(["sh", "-c", &full]), Access::Mutate).map(|o| o.stdout).map_err(|e| e.to_string())
    }

    fn in_release(&self, cmd: &str) -> std::result::Result<String, String> {
        self.exec(&self.docker().exec_sh(&self.container, cmd), Access::Mutate).map(|o| o.stdout).map_err(|e| e.to_string())
    }

    fn exec_in(&self, service: &str, cmd: &str) -> std::result::Result<String, String> {
        let container = self.container_of(service).map_err(|e| e.to_string())?;
        self.exec(&self.docker().exec_sh(&container, cmd), Access::Mutate).map(|o| o.stdout).map_err(|e| e.to_string())
    }

    fn docker(&self, args: Vec<String>) -> std::result::Result<String, String> {
        let mut argv = vec!["docker".to_string()];
        argv.extend(args);
        self.exec(&Argv(argv), Access::Mutate).map(|o| o.stdout).map_err(|e| e.to_string())
    }

    fn compose(&self, args: Vec<String>) -> std::result::Result<String, String> {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.exec_env(&self.docker().compose(&refs), Access::Mutate, self.compose_overlay())
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
        self.fs.exists(&self.resolve(Path::new(path))).unwrap_or(false)
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


#[cfg(test)]
mod tests;
