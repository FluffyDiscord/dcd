use std::collections::HashMap;

use super::*;
use crate::config;
use crate::effects::{CmdOutput, FixedClock, MemoryFs, RecordingRunner};
use crate::signal::Interrupt;
use crate::state::{Release, ReleaseStatus, State};
use crate::ui::{Mode, Reporter};
use indexmap::IndexMap;

fn release(id: u64, container: &str, app: &str, status: ReleaseStatus) -> Release {
    let mut images = IndexMap::new();
    images.insert("app".to_string(), app.to_string());
    Release {
        id,
        container: container.to_string(),
        images,
        created_at: id,
        status,
        ran_migrations: false,
        reason: None,
        env_keys: Vec::new(),
        reaped: false,
    }
}

fn cfg() -> config::Config {
    config::load(cfg_src(), Some("prod"), &[], &HashMap::new()).unwrap()
}

fn cfg_src() -> &'static str {
    r#"
version: 2
project: demo
registry: reg
compose:
  files: [base.yml]
  env:
    REGISTRY: reg
directories:
  - { path: .docker/logs }
release:
  service: app
  container_prefix: demo-app
  healthcheck: { exec_in: nginx, cmd: 'curl -sf http://{container}:2114/health', retries: 3, interval: 1s }
  migrate: { before: 'migrate before', after: 'migrate after' }
  drain: 'graceful-stop'
cutover:
  service: nginx
  backend_port: 8080
  reload: { exec_in: nginx, cmd: 'nginx -s reload' }
services:
  postgres:
    on_recreate_drain_workers: true
    wait: { cmd: 'pg_isready', retries: 3, interval: 1s }
  nginx:
    recreate: never
workers:
  service: worker
  provider: { command_in_release: 'list-transports' }
stages:
  prod: {}
"#
}

/// Stands in for `docker compose config --format json` — under ADR-013 every
/// container fact the engine uses comes from here, not from `dcd.yaml`.
fn model() -> crate::compose::ComposeModel {
    crate::compose::ComposeModel::from_json(
        br#"{"services":{
            "app":{"image":"reg:app-1","restart":"unless-stopped",
                   "networks":{"default":{"aliases":["app-rr"]}}},
            "worker":{"image":"reg:app-1","restart":"unless-stopped"},
            "postgres":{"image":"reg:db-1","container_name":"demo-postgres"},
            "nginx":{"container_name":"demo-nginx"}
        }}"#,
    )
    .unwrap()
}

fn opts() -> Options {
    Options {
        sleep_enabled: false,
        ..Options::default()
    }
}

#[test]
fn full_deploy_records_the_pipeline_and_advances_state() {
    let cfg = cfg();
    let runner = RecordingRunner::new()
        .with_stdout("inspect demo-postgres", "reg:db-1") // matches desired -> no recreate
        .with_stdout("list-transports", "async\nscheduler");
    let fs = MemoryFs::new();
    let clock = FixedClock(1000);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();

    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, State::default(), opts(), model());
    engine.deploy().unwrap();

    let calls = runner.display_calls();
    let has = |needle: &str| calls.iter().any(|c| c == needle);

    assert!(has("docker pull reg:app-1"));
    assert!(has("docker pull reg:db-1"));
    assert!(has("docker inspect demo-postgres --format '{{.Config.Image}}'"));
    assert!(has("docker compose -p demo --env-file /dev/null -f base.yml -f dcd-image-override.prod.yml up -d --no-recreate --wait --wait-timeout 120 postgres"));
    assert!(has("docker exec demo-postgres sh -c pg_isready"));
    assert!(has("docker compose -p demo --env-file /dev/null -f base.yml -f dcd-image-override.prod.yml run --rm -T --no-deps --entrypoint migrate app before"));
    assert!(has("docker compose -p demo --env-file /dev/null -f base.yml -f dcd-image-override.prod.yml run -d --name demo-app-1000 --use-aliases --no-deps app"));
    assert!(has("docker update --restart unless-stopped demo-app-1000"));
    assert!(has("docker exec demo-nginx sh -c 'curl -sf http://demo-app-1000:2114/health'"));
    assert!(has("docker exec demo-nginx sh -c 'nginx -s reload'"));
    assert!(has("docker exec demo-app-1000 migrate after"));
    // N containers from ONE compose service — no rendered workers file (spec §7.12)
    assert!(has("docker compose -p demo --env-file /dev/null -f base.yml -f dcd-image-override.prod.yml run -d --name worker-async --no-deps worker async"));
    assert!(has("docker compose -p demo --env-file /dev/null -f base.yml -f dcd-image-override.prod.yml run -d --name worker-scheduler --no-deps worker scheduler"));

    // healthcheck targets the container NAME, never a shared alias (spec §7.7)
    assert!(!calls.iter().any(|c| c.contains("http://app-rr:")));

    let state = engine.into_state();
    let prod = state.stage("prod").unwrap();
    assert_eq!(prod.current.as_deref(), Some("demo-app-1000"));
    assert_eq!(prod.find("demo-app-1000").unwrap().status, crate::state::ReleaseStatus::Active);

    // no env file is ever rendered (spec §5.2.4), and v2 renders no workers file
    assert!(!fs.exists(std::path::Path::new("./compose.env")).unwrap());
    assert!(!fs.exists(std::path::Path::new("./workers.yml")).unwrap());
    assert!(fs.exists(std::path::Path::new("./dcd-state.json")).unwrap());
    // The one permission dcd sets deliberately: state records what is deployed and
    // to where, and is the file INV-3 recovery reads.
    assert_eq!(fs.mode_of(std::path::Path::new("./dcd-state.json")), Some(0o600));
}

#[test]
fn verbose_traces_every_command_the_recipe_runs() {
    let cfg = cfg();
    let runner = RecordingRunner::new()
        .with_stdout("inspect demo-postgres", "reg:db-1")
        .with_stdout("list-transports", "async\nscheduler");
    let fs = MemoryFs::new();
    let clock = FixedClock(1000);
    let reporter = Reporter::capture_verbose(Mode::Plain);
    let interrupt = Interrupt::inert();

    let mut traced_opts = opts();
    traced_opts.container_env = [("APP_SECRET".to_string(), "hunter2".to_string())].into_iter().collect();
    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, State::default(), traced_opts, model());
    engine.deploy().unwrap();

    let lines = reporter.lines();
    let traced: Vec<&String> = lines.iter().filter(|line| line.starts_with("$ ")).collect();
    assert_eq!(traced.len(), runner.display_calls().len());
    assert!(traced.iter().any(|line| *line == "$ docker pull reg:app-1"));
    assert!(lines.iter().any(|line| line.starts_with("  exit 0 in ")));

    // captured stdout is surfaced instead of being dropped
    assert!(lines.iter().any(|line| line == "  | async"));

    // env values never reach the trace — passthrough is a bare `-e KEY` (§5.2.4).
    // Asserted against a value the run actually carries, or it proves nothing.
    let trace = lines.join("\n");
    assert!(trace.contains("-e APP_SECRET"), "the key name is what travels: {trace}");
    assert!(!trace.contains("hunter2"), "a value reached the trace: {trace}");
}

#[test]
fn quiet_by_default_leaves_the_recipe_output_untouched() {
    let cfg = cfg();
    let runner = RecordingRunner::new()
        .with_stdout("inspect demo-postgres", "reg:db-1")
        .with_stdout("list-transports", "async\nscheduler");
    let fs = MemoryFs::new();
    let clock = FixedClock(1000);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();

    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, State::default(), opts(), model());
    engine.deploy().unwrap();

    assert!(!reporter.lines().iter().any(|line| line.starts_with("$ ")));
}

#[test]
fn failed_healthcheck_aborts_pre_cutover_and_removes_black() {
    let cfg = cfg();
    let runner = RecordingRunner::new()
        .with_stdout("inspect demo-postgres", "reg:db-1")
        .with_response(
            "2114/health",
            CmdOutput {
                code: 1,
                stdout: String::new(),
                stderr: "unhealthy".into(),
            },
        );
    let fs = MemoryFs::new();
    let clock = FixedClock(2000);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();

    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, State::default(), opts(), model());
    let err = engine.deploy().unwrap_err();

    assert_eq!(err.exit_code(), 1); // pre-cutover, red would still be serving
    let calls = runner.display_calls();
    assert!(calls.iter().any(|c| c == "docker rm -f demo-app-2000")); // black cleaned up
    assert!(!calls.iter().any(|c| c.contains("nginx -s reload"))); // never cut over

    let state = engine.into_state();
    let prod = state.stage("prod").unwrap();
    assert!(prod.releases.is_empty()); // no release recorded pre-cutover (INV-3)
    assert_eq!(prod.current, None);
    // but the tags this deploy pulled are on the host, so they are on the ledger
    let pulled: Vec<&str> = prod.pulled.iter().map(|p| p.tag.as_str()).collect();
    assert_eq!(pulled, vec!["reg:app-1", "reg:db-1"]);
}

#[test]
fn dry_run_executes_no_mutations() {
    use crate::effects::DryRunRunner;
    let cfg = cfg();
    let inner = RecordingRunner::new()
        .with_stdout("inspect demo-postgres", "reg:db-1")
        .with_stdout("list-transports", "async\nscheduler");
    let runner = DryRunRunner::new(inner);
    let fs = MemoryFs::new();
    let clock = FixedClock(3000);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();

    let mut engine = Engine::new(
        cfg.clone(),
        &runner,
        &fs,
        &clock,
        &reporter,
        &interrupt,
        State::default(),
        Options {
            dry_run: true,
            sleep_enabled: false,
            ..Options::default()
        },
        model(),
    );
    engine.deploy().unwrap();

    // no files written in dry-run
    assert!(!fs.exists(std::path::Path::new("./compose.env")).unwrap());
    assert!(!fs.exists(std::path::Path::new("./dcd-state.json")).unwrap());
    // every mutating docker command was stubbed, not executed
    let stubbed = runner.stubbed();
    assert!(stubbed.iter().any(|a| a.display().starts_with("docker compose -p demo --env-file /dev/null -f base.yml -f dcd-image-override.prod.yml run -d --name demo-app-3000")));
}

#[test]
fn rollback_deploys_previous_image_without_migrations() {
    let cfg = cfg();
    let runner = RecordingRunner::new()
        .with_stdout("inspect demo-postgres", "reg:db-1")
        .with_stdout("list-transports", "async");
    let fs = MemoryFs::new();
    let clock = FixedClock(5000);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();

    let mut state = State::default();
    {
        let st = state.stage_mut("prod");
        st.releases.push(release(1, "demo-app-1", "reg:app-old", ReleaseStatus::Superseded));
        st.releases.push(release(2, "demo-app-2", "reg:app-new", ReleaseStatus::Active));
        st.current = Some("demo-app-2".into());
    }

    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, state, opts(), model());
    engine.rollback().unwrap();

    let calls = runner.display_calls();
    assert!(calls.iter().any(|c| c == "docker pull reg:app-old"));
    assert!(calls.iter().any(|c| c == "docker compose -p demo --env-file /dev/null -f base.yml -f dcd-image-override.prod.yml run -d --name demo-app-5000 --use-aliases --no-deps app"));
    assert!(!calls.iter().any(|c| c.contains("migrate"))); // INV-5: no migrations on rollback

    let state = engine.into_state();
    let prod = state.stage("prod").unwrap();
    assert_eq!(prod.current.as_deref(), Some("demo-app-5000"));
    assert_eq!(prod.find("demo-app-5000").unwrap().status, ReleaseStatus::Active);
    assert_eq!(prod.find("demo-app-2").unwrap().status, ReleaseStatus::RolledBack);
    assert_eq!(prod.find("demo-app-1").unwrap().status, ReleaseStatus::Superseded);
}

#[test]
fn resume_runs_only_post_cutover_steps() {
    let cfg = cfg();
    let runner = RecordingRunner::new().with_stdout("list-transports", "async");
    let fs = MemoryFs::new();
    let clock = FixedClock(6000);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();

    let mut state = State::default();
    {
        let st = state.stage_mut("prod");
        st.releases.push(release(1, "demo-app-prev", "reg:app-1", ReleaseStatus::Active));
        st.releases.push(release(2, "demo-app-9", "reg:app-2", ReleaseStatus::CutoverPending));
        st.current = Some("demo-app-prev".into());
    }

    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, state, opts(), model());
    engine.resume().unwrap();

    let calls = runner.display_calls();
    assert!(!calls.iter().any(|c| c.starts_with("docker run -d"))); // no new black started
    assert!(!calls.iter().any(|c| c.contains("nginx -s reload"))); // no cutover
    assert!(calls.iter().any(|c| c == "docker exec demo-app-9 migrate after"));

    let state = engine.into_state();
    let prod = state.stage("prod").unwrap();
    assert_eq!(prod.current.as_deref(), Some("demo-app-9"));
    assert_eq!(prod.find("demo-app-9").unwrap().status, ReleaseStatus::Active);
    assert_eq!(prod.find("demo-app-prev").unwrap().status, ReleaseStatus::Superseded);
}

#[test]
fn cutover_persists_pending_to_disk_enabling_resume_after_exit4() {
    let cfg = cfg();
    let fs = MemoryFs::new();
    let clock = FixedClock(7000);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();

    // Deploy that fails AFTER cutover (migrate:after errors) -> exit 4.
    let runner1 = RecordingRunner::new()
        .with_stdout("inspect demo-postgres", "reg:db-1")
        .with_response(
            "demo-app-7000 migrate after",
            CmdOutput { code: 1, stdout: String::new(), stderr: "boom".into() },
        );
    let mut e1 = Engine::new(cfg.clone(), &runner1, &fs, &clock, &reporter, &interrupt, State::default(), opts(), model());
    let err = e1.deploy().unwrap_err();
    assert_eq!(err.exit_code(), 4); // post-cutover failure, black is live

    // The cutover_pending release MUST be on disk (INV-3) so resume can find it.
    let bytes = fs.read(std::path::Path::new("./dcd-state.json")).unwrap();
    let persisted = State::from_json(&bytes).unwrap();
    let pending = persisted.stage("prod").unwrap().cutover_pending().unwrap();
    assert_eq!(pending.container, "demo-app-7000");

    // A fresh engine resumes from the persisted state and finalizes.
    let runner2 = RecordingRunner::new().with_stdout("list-transports", "async");
    let mut e2 = Engine::new(cfg.clone(), &runner2, &fs, &clock, &reporter, &interrupt, persisted, opts(), model());
    e2.resume().unwrap();
    let calls = runner2.display_calls();
    assert!(calls.iter().any(|c| c == "docker exec demo-app-7000 migrate after"));
    let prod = e2.into_state();
    assert_eq!(prod.stage("prod").unwrap().current.as_deref(), Some("demo-app-7000"));
}

#[test]
fn deploy_refuses_when_a_cutover_pending_exists() {
    let cfg = cfg();
    let runner = RecordingRunner::new();
    let fs = MemoryFs::new();
    let clock = FixedClock(8000);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();
    let mut state = State::default();
    state.stage_mut("prod").releases.push(release(1, "demo-app-1", "reg:app-1", ReleaseStatus::CutoverPending));

    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, state, opts(), model());
    let err = engine.deploy().unwrap_err();
    assert_eq!(err.exit_code(), 4);
    assert!(runner.display_calls().is_empty()); // nothing ran
}

#[test]
fn unlock_promotes_the_stuck_release_and_runs_no_docker_at_all() {
    let cfg = cfg();
    let runner = RecordingRunner::new();
    let fs = MemoryFs::new();
    let clock = FixedClock(8200);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();

    let mut state = State::default();
    {
        let st = state.stage_mut("prod");
        st.releases.push(release(1, "demo-app-prev", "reg:app-1", ReleaseStatus::Active));
        st.releases.push(release(2, "demo-app-9", "reg:app-2", ReleaseStatus::CutoverPending));
        st.current = Some("demo-app-prev".into());
    }

    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, state, opts(), model());
    let promoted = engine.unlock().unwrap();
    assert_eq!(promoted.as_deref(), Some("demo-app-9"));

    // Nothing is drained, recreated, or garbage-collected — the next deploy does that.
    assert!(runner.display_calls().is_empty(), "unlock spawned: {:?}", runner.display_calls());

    let persisted = State::from_json(&fs.read(std::path::Path::new("./dcd-state.json")).unwrap()).unwrap();
    let prod = persisted.stage("prod").unwrap();
    assert_eq!(prod.current.as_deref(), Some("demo-app-9"));
    assert_eq!(prod.find("demo-app-9").unwrap().status, ReleaseStatus::Active);
    assert_eq!(prod.find("demo-app-prev").unwrap().status, ReleaseStatus::Superseded);
    assert_eq!(prod.pending_count(), 0);

    let warnings = reporter.lines().join("\n");
    assert!(warnings.contains("migrate:after"), "operator is told what was skipped: {warnings}");
    assert!(warnings.contains("workers"), "operator is told what was skipped: {warnings}");
    assert!(
        warnings.contains("demo-app-prev was left running"),
        "operator is told the old container survives: {warnings}"
    );
}

#[test]
fn unlock_without_a_pending_release_changes_nothing() {
    let cfg = cfg();
    let runner = RecordingRunner::new();
    let fs = MemoryFs::new();
    let clock = FixedClock(8300);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();

    let mut state = State::default();
    {
        let st = state.stage_mut("prod");
        st.releases.push(release(1, "demo-app-prev", "reg:app-1", ReleaseStatus::Active));
        st.current = Some("demo-app-prev".into());
    }

    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, state, opts(), model());
    assert_eq!(engine.unlock().unwrap(), None);
    assert!(runner.display_calls().is_empty());
    assert!(!fs.exists(std::path::Path::new("./dcd-state.json")).unwrap());
    assert_eq!(engine.into_state().stage("prod").unwrap().current.as_deref(), Some("demo-app-prev"));
}

#[test]
fn unlock_promotes_a_stuck_first_ever_release() {
    // No previous `current` to supersede — the escape hatch must not need one.
    let cfg = cfg();
    let runner = RecordingRunner::new();
    let fs = MemoryFs::new();
    let clock = FixedClock(8400);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();

    let mut state = State::default();
    state.stage_mut("prod").releases.push(release(9, "demo-app-9", "reg:app-2", ReleaseStatus::CutoverPending));

    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, state, opts(), model());
    assert_eq!(engine.unlock().unwrap().as_deref(), Some("demo-app-9"));
    let prod = engine.into_state();
    assert_eq!(prod.stage("prod").unwrap().current.as_deref(), Some("demo-app-9"));
    assert!(!reporter.lines().join("\n").contains("was left running"));
}

#[test]
fn unlock_records_the_reason_and_demotes_a_second_pending() {
    let cfg = cfg();
    let runner = RecordingRunner::new();
    let fs = MemoryFs::new();
    let clock = FixedClock(8500);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();

    let mut state = State::default();
    {
        let st = state.stage_mut("prod");
        st.releases.push(release(1, "demo-app-old", "reg:app-1", ReleaseStatus::CutoverPending));
        st.releases.push(release(2, "demo-app-9", "reg:app-2", ReleaseStatus::CutoverPending));
    }

    let unlock_opts = Options {
        reason: Some("migrations hand-applied".to_string()),
        ..opts()
    };
    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, state, unlock_opts, model());
    assert_eq!(engine.unlock().unwrap().as_deref(), Some("demo-app-9"));

    let prod = engine.into_state();
    let stage = prod.stage("prod").unwrap();
    assert_eq!(stage.find("demo-app-old").unwrap().status, ReleaseStatus::RolledBack);
    assert_eq!(stage.find("demo-app-9").unwrap().reason.as_deref(), Some("migrations hand-applied"));
    assert_eq!(stage.pending_count(), 0);
}

#[test]
fn resume_refuses_more_than_one_pending() {
    let cfg = cfg();
    let runner = RecordingRunner::new();
    let fs = MemoryFs::new();
    let clock = FixedClock(8100);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();
    let mut state = State::default();
    {
        let st = state.stage_mut("prod");
        st.releases.push(release(1, "a", "i1", ReleaseStatus::CutoverPending));
        st.releases.push(release(2, "b", "i2", ReleaseStatus::CutoverPending));
    }
    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, state, opts(), model());
    let err = engine.resume().unwrap_err();
    assert_eq!(err.exit_code(), 2);
    assert!(err.to_string().contains("more than one"));
}

#[test]
fn finalize_garbage_collects_evicted_images() {
    let cfg = cfg(); // retention defaults: keep_releases 1
    let runner = RecordingRunner::new()
        .with_stdout("inspect demo-postgres", "reg:db-1")
        .with_stdout("list-transports", "async");
    let fs = MemoryFs::new();
    let clock = FixedClock(1000);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();
    let mut state = State::default();
    {
        let st = state.stage_mut("prod");
        for id in 1..=4 {
            st.releases.push(release(id, &format!("demo-app-{id}"), &format!("img{id}"), ReleaseStatus::Superseded));
        }
        st.releases.push(release(5, "demo-app-5", "imgC", ReleaseStatus::Active));
        st.current = Some("demo-app-5".into());
    }
    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, state, opts(), model());
    engine.deploy().unwrap();
    let calls = runner.display_calls();
    // after finalize: superseded = img1..img4 + imgC (demo-app-5 demoted); keep 3 -> evict img1,img2
    assert!(calls.iter().any(|c| c == "docker image rm img1"));
    assert!(calls.iter().any(|c| c == "docker image rm img2"));
    assert!(!calls.iter().any(|c| c == "docker image rm imgC")); // current never removed
}

/// The defect: a deploy that pulled and then died left its tag on the host with no
/// release entry, so GC could never see it again. The pull ledger makes it visible.
#[test]
fn a_tag_pulled_by_a_deploy_that_never_finalized_is_reclaimed_later() {
    let mut state = State::default();
    {
        let prod = state.stage_mut("prod");
        prod.record_pull("app", "reg:app-dead", 10);
    }
    let cfg = cfg(); // keep_managed_images 1
    let runner = RecordingRunner::new()
        .with_stdout("inspect demo-postgres", "reg:db-1")
        .with_stdout("list-transports", "async");
    let fs = MemoryFs::new();
    let clock = FixedClock(1000);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();
    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, state, opts(), model());
    engine.deploy().unwrap();
    // keep_managed_images defaults to 1, so the tag the dead deploy left is past the keep
    // count as soon as this deploy pulls its own -> reclaimed here, and the ledger drops it
    assert!(runner.display_calls().iter().any(|c| c == "docker image rm reg:app-dead"));
    let after = engine.into_state();
    let ledger: Vec<&str> = after.stage("prod").unwrap().pulled.iter().map(|p| p.tag.as_str()).collect();
    assert!(!ledger.contains(&"reg:app-dead"));
}

#[test]
fn a_registry_port_is_not_a_tag_separator() {
    assert_eq!(repository_of("reg.example.com/team/app:sha-1"), "reg.example.com/team/app");
    assert_eq!(repository_of("localhost:5000/app"), "localhost:5000/app");
    assert_eq!(repository_of("localhost:5000/app:v2"), "localhost:5000/app");
    assert_eq!(repository_of("reg.example.com/app@sha256:abc"), "reg.example.com/app");
    assert_eq!(repository_of("postgres"), "postgres");
}

fn qualified_cfg() -> config::Config {
    config::load(
        cfg_src(),
        Some("prod"),
        &["registry=reg.example.com/demo".to_string()],
        &HashMap::new(),
    )
    .unwrap()
}

#[test]
fn gc_all_offers_only_host_tags_no_stage_records_and_names_each_survivors_rule() {
    let cfg = qualified_cfg();
    let runner = RecordingRunner::new()
        .with_stdout(
            "docker images reg.example.com/demo",
            "reg.example.com/demo:app-1\nreg.example.com/demo:app-orphan\nreg.example.com/demo:nginx-live\nreg.example.com/demo:<none>\n",
        )
        .with_stdout("docker ps -a --format", "reg.example.com/demo:nginx-live\nunrelated:latest\n");
    let fs = MemoryFs::new();
    let clock = FixedClock(1000);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();
    let mut state = State::default();
    state.stage_mut("prod").record_pull("app", "reg.example.com/demo:app-1", 10);

    let engine = Engine::new(cfg, &runner, &fs, &clock, &reporter, &interrupt, state, opts(), model());
    let plan = engine.gc_plan(true).unwrap();

    assert_eq!(plan.orphans, vec!["reg.example.com/demo:app-orphan".to_string()]);
    let protected: Vec<&str> = plan.protected.iter().map(|(tag, _)| tag.as_str()).collect();
    assert_eq!(protected, vec!["reg.example.com/demo:app-1", "reg.example.com/demo:nginx-live"]);
    assert!(plan.protected[0].1.contains("pull ledger"));
    assert!(plan.protected[1].1.contains("container"));
    // an untagged image is never proposed: removing it by id would untag other repositories
    assert!(!plan.removals().iter().any(|tag| tag.contains("<none>")));
    // the host is only ever asked about repositories this config resolves to
    let queries: Vec<String> = runner.display_calls().into_iter().filter(|c| c.starts_with("docker images")).collect();
    assert_eq!(
        queries,
        vec!["docker images reg.example.com/demo --format '{{.Repository}}:{{.Tag}}'".to_string()]
    );
}

#[test]
fn gc_without_all_proposes_recorded_tags_and_still_never_asks_the_host() {
    let cfg = qualified_cfg(); // keep_managed_images 1
    let runner = RecordingRunner::new();
    let fs = MemoryFs::new();
    let clock = FixedClock(1000);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();
    let mut state = State::default();
    {
        let prod = state.stage_mut("prod");
        prod.record_pull("app", "reg.example.com/demo:app-1", 10);
        prod.record_pull("app", "reg.example.com/demo:app-2", 20);
        prod.record_pull("app", "reg.example.com/demo:app-3", 30);
    }
    let engine = Engine::new(cfg, &runner, &fs, &clock, &reporter, &interrupt, state, opts(), model());
    let plan = engine.gc_plan(false).unwrap();
    // keep_managed_images defaults to 1 — the newest tag is the only one kept
    assert_eq!(
        plan.recorded,
        vec!["reg.example.com/demo:app-2".to_string(), "reg.example.com/demo:app-1".to_string()]
    );
    assert!(plan.orphans.is_empty());
    assert!(runner.display_calls().is_empty());
}

/// A tag Docker refuses to remove is still on the host, so it must stay on the ledger:
/// forgetting it is exactly how a tag became invisible to GC in the first place.
#[test]
fn a_removal_docker_refuses_keeps_its_ledger_row() {
    let cfg = qualified_cfg();
    let runner = RecordingRunner::new().with_response(
        "image rm reg.example.com/demo:app-1",
        CmdOutput {
            code: 1,
            stdout: String::new(),
            stderr: "image is being used by stopped container abc".into(),
        },
    );
    let fs = MemoryFs::new();
    let clock = FixedClock(1000);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();
    let mut state = State::default();
    {
        let prod = state.stage_mut("prod");
        prod.record_pull("app", "reg.example.com/demo:app-1", 10);
        prod.record_pull("app", "reg.example.com/demo:app-2", 20);
        prod.record_pull("app", "reg.example.com/demo:app-3", 30);
    }
    let mut engine = Engine::new(cfg, &runner, &fs, &clock, &reporter, &interrupt, state, opts(), model());
    let plan = engine.gc_plan(false).unwrap();
    // at keep_managed_images 1 the plan is app-2 + app-1; docker refuses only app-1
    assert_eq!(engine.gc(&plan).unwrap(), 1);
    let after = engine.into_state();
    let ledger: Vec<&str> = after.stage("prod").unwrap().pulled.iter().map(|p| p.tag.as_str()).collect();
    assert!(ledger.contains(&"reg.example.com/demo:app-1"));
    assert!(!ledger.contains(&"reg.example.com/demo:app-2"));
}

/// A bare one-word repository is a Docker Hub library name; on a shared host those
/// images belong to whoever pulled them, and dcd cannot show otherwise.
#[test]
fn gc_all_skips_a_public_library_repository_but_still_sweeps_the_provable_one() {
    let src = cfg_src().replace("registry: reg\n", "");
    let cfg = config::load(&src, Some("prod"), &[], &HashMap::new()).unwrap();
    let model = crate::compose::ComposeModel::from_json(
        br#"{"services":{
            "app":{"image":"reg.example.com/demo:app-1"},
            "worker":{"image":"reg.example.com/demo:app-1"},
            "postgres":{"image":"postgres:16","container_name":"demo-postgres"},
            "nginx":{"container_name":"demo-nginx"}
        }}"#,
    )
    .unwrap();
    let runner = RecordingRunner::new().with_stdout("docker images reg.example.com/demo", "reg.example.com/demo:old\n");
    let fs = MemoryFs::new();
    let clock = FixedClock(1000);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();
    let engine = Engine::new(cfg, &runner, &fs, &clock, &reporter, &interrupt, State::default(), opts(), model);
    let plan = engine.gc_plan(true).unwrap();
    assert_eq!(plan.orphans, vec!["reg.example.com/demo:old".to_string()]);
    assert!(reporter.lines().iter().any(|line| line.contains("not sweeping 'postgres'")));
    assert!(!runner.display_calls().iter().any(|c| c.contains("docker images postgres")));
}

/// A `/` proves nothing on its own: `bitnami/postgresql` is a Docker Hub namespace,
/// not our registry. And an image no service or worker uses is not ours to sweep.
#[test]
fn gc_all_skips_a_namespaced_hub_image_and_an_image_nothing_uses() {
    let src = cfg_src().replace("registry: reg\n", "");
    let cfg = config::load(&src, Some("prod"), &[], &HashMap::new()).unwrap();
    let model = crate::compose::ComposeModel::from_json(
        br#"{"services":{
            "app":{"image":"reg.example.com/demo:app-1"},
            "worker":{"image":"reg.example.com/demo:app-1"},
            "postgres":{"image":"bitnami/postgresql:16","container_name":"demo-postgres"},
            "nginx":{"container_name":"demo-nginx"}
        }}"#,
    )
    .unwrap();
    let runner = RecordingRunner::new().with_stdout("docker images reg.example.com/demo", "reg.example.com/demo:old\n");
    let fs = MemoryFs::new();
    let clock = FixedClock(1000);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();
    let engine = Engine::new(cfg, &runner, &fs, &clock, &reporter, &interrupt, State::default(), opts(), model);
    let plan = engine.gc_plan(true).unwrap();
    assert_eq!(plan.orphans, vec!["reg.example.com/demo:old".to_string()]);
    assert!(reporter.lines().iter().any(|line| line.contains("not sweeping 'bitnami/postgresql'")));
    // only the one provable repository is queried, and only once
    let queried: Vec<String> = runner.display_calls().into_iter().filter(|c| c.starts_with("docker images")).collect();
    assert_eq!(queried.len(), 1, "{queried:?}");
}

#[test]
fn gc_all_refuses_when_no_repository_can_be_shown_to_be_ours() {
    let src = cfg_src().replace("registry: reg\n", "").replace("app: app-1", "app: postgres:16");
    let cfg = config::load(&src, Some("prod"), &[], &HashMap::new()).unwrap();
    let runner = RecordingRunner::new();
    let fs = MemoryFs::new();
    let clock = FixedClock(1000);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();
    let engine = Engine::new(cfg, &runner, &fs, &clock, &reporter, &interrupt, State::default(), opts(), model());
    let err = engine.gc_plan(true).unwrap_err();
    assert!(err.to_string().contains("no repository of demo can be shown"), "{err}");
    assert!(runner.display_calls().is_empty());
}

/// Stages share one repository, so a stage-local view of "what is still needed"
/// would delete a sibling stage's rollback target (INV-6).
#[test]
fn gc_never_removes_a_tag_another_stage_still_records() {
    let mut state = State::default();
    state.stage_mut("beta").record_pull("app", "reg:app-dead", 10);
    {
        let prod = state.stage_mut("prod");
        prod.record_pull("app", "reg:app-dead", 10);
        prod.record_pull("app", "reg:app-2", 20);
    }
    let cfg = cfg();
    let runner = RecordingRunner::new()
        .with_stdout("inspect demo-postgres", "reg:db-1")
        .with_stdout("list-transports", "async");
    let fs = MemoryFs::new();
    let clock = FixedClock(3000);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();
    let mut engine = Engine::new(cfg, &runner, &fs, &clock, &reporter, &interrupt, state, opts(), model());
    engine.deploy().unwrap();
    assert!(!runner.display_calls().iter().any(|c| c == "docker image rm reg:app-dead"));
}

#[test]
fn infra_drains_workers_before_recreating_db_and_waits_after() {
    let cfg = cfg();
    let runner = RecordingRunner::new()
        .with_stdout("inspect demo-postgres", "reg:db-OLD") // != desired reg:db-1 -> recreate
        .with_stdout("compose.service=worker", "demo-worker-async")
        .with_stdout("list-transports", "async");
    let fs = MemoryFs::new();
    let clock = FixedClock(9000);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();
    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, State::default(), opts(), model());
    engine.deploy().unwrap();
    let calls = runner.display_calls();
    let idx = |needle: &str| calls.iter().position(|c| c.contains(needle)).unwrap_or(usize::MAX);
    let stop = idx("docker stop --signal");
    let recreate = idx("up -d --wait --wait-timeout 120 postgres");
    let wait = idx("sh -c pg_isready");
    assert!(stop < recreate, "worker drain must precede DB recreate");
    assert!(recreate < wait, "wait gate must follow recreate");
}

#[test]
fn second_synthetic_config_drives_plan_with_zero_core_changes() {
    // The generality proof: a project with no database, static workers, no
    // migrations and a different router drives the same recipe unchanged.
    let src = r#"
version: 2
project: blogapp
compose:
  files: [compose.prod.yml]
  env: { COMPOSE_PROJECT_NAME: blogapp }
release:
  service: app
  container_prefix: blogapp-app
  healthcheck: { exec_in: web, cmd: 'wget -qO- http://{container}:9000/up', retries: 2, interval: 1s }
cutover:
  service: web
  backend_port: 9000
  reload: { exec_in: web, cmd: 'nginx -s reload' }
services:
  web:
    wait: { cmd: 'wget -qO- localhost/up', retries: 2, interval: 1s }
workers:
  service: worker
  provider: { static: [default, mail] }
stages:
  prod: {}
"#;
    let model = crate::compose::ComposeModel::from_json(
        br#"{"services":{
            "app":{"image":"app1","restart":"unless-stopped"},
            "worker":{"image":"app1"},
            "web":{"image":"web1","container_name":"blogapp-web"}
        }}"#,
    )
    .unwrap();
    let cfg = config::load(src, Some("prod"), &[], &HashMap::new()).unwrap();
    let runner = RecordingRunner::new().with_stdout("inspect blogapp-web", "web1");
    let fs = MemoryFs::new();
    let clock = FixedClock(1234);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();
    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, State::default(), opts(), model);
    engine.deploy().unwrap();
    let calls = runner.display_calls();
    let compose = "docker compose -p blogapp --env-file /dev/null -f compose.prod.yml -f dcd-image-override.prod.yml";

    assert!(!calls.iter().any(|c| c.contains("migrate"))); // no migrate config -> skipped
    assert!(!calls.iter().any(|c| c.contains("postgres"))); // no postgres service
    assert!(calls
        .iter()
        .any(|c| c == &format!("{compose} run -d --name blogapp-app-1234 --use-aliases --no-deps app")));
    // static workers, one container each from the SAME service
    assert!(calls.iter().any(|c| c == &format!("{compose} run -d --name worker-default --no-deps worker default")));
    assert!(calls.iter().any(|c| c == &format!("{compose} run -d --name worker-mail --no-deps worker mail")));
    assert!(calls
        .iter()
        .any(|c| c == "docker exec blogapp-web sh -c 'wget -qO- http://blogapp-app-1234:9000/up'"));
}

#[test]
fn chain_env_reaches_containers_as_bare_keys_with_overlays() {
    // TC-035/TC-036: chain keys ride as bare -e / bare compose names; explicit maps
    // arrive as per-command overlays; no env value lands in any written file.
    let cfg = cfg();
    let runner = RecordingRunner::new()
        .with_stdout("inspect demo-postgres", "reg:db-1")
        .with_stdout("list-transports", "async");
    let fs = MemoryFs::new();
    let clock = FixedClock(4000);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();

    let mut options = opts();
    options.container_env = [
        ("DATABASE_URL".to_string(), "postgres://secret@db".to_string()),
        ("APP_SECRET".to_string(), "hunter2".to_string()),
    ]
    .into_iter()
    .collect();

    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, State::default(), options, model());
    engine.deploy().unwrap();

    let calls = runner.display_calls();
    assert!(calls.iter().any(|c| c
        == "docker compose -p demo --env-file /dev/null -f base.yml -f dcd-image-override.prod.yml run -d --name demo-app-4000 --use-aliases --no-deps -e APP_SECRET -e DATABASE_URL app"));

    // creation carries the compose.env overlay; values never enter the argv
    let overlay = runner.env_overlay_of("docker compose -p demo --env-file /dev/null -f base.yml -f dcd-image-override.prod.yml run -d --name demo-app-4000").unwrap();
    assert_eq!(overlay.get("REGISTRY").map(String::as_str), Some("reg"));

    // and so do the plain compose calls
    let overlay = runner.env_overlay_of("up -d --no-recreate --wait --wait-timeout 120 postgres").unwrap();
    assert_eq!(overlay.get("REGISTRY").map(String::as_str), Some("reg"));

    // v2 writes no workers file at all, so there is one fewer place a value could rest
    assert!(!fs.exists(std::path::Path::new("./workers.yml")).unwrap());
    // workers are created with bare `-e KEY`, names only
    assert!(calls
        .iter()
        .any(|c| c.contains("run -d --name worker-async") && c.contains("-e APP_SECRET") && !c.contains("hunter2")));
    let state_json = String::from_utf8(fs.read(std::path::Path::new("./dcd-state.json")).unwrap()).unwrap();
    assert!(!state_json.contains("hunter2"));
    assert!(!calls.iter().any(|c| c.contains("hunter2")), "no value in any argv");

    // TC-037: the release records its delivered key names
    let state = engine.into_state();
    let release = state.stage("prod").unwrap().find("demo-app-4000").unwrap().clone();
    assert_eq!(release.env_keys, vec!["APP_SECRET", "DATABASE_URL"]);
}

#[test]
fn worker_creation_carries_the_compose_env_overlay() {
    // v2 replaces `workers.template.env`: worker containers declare their own env
    // on the compose service, and dcd supplies only compose.env for ${VAR}
    // substitution plus the chain as bare `-e KEY` (spec §7.12).
    let cfg = cfg();
    let runner = RecordingRunner::new()
        .with_stdout("inspect demo-postgres", "reg:db-1")
        .with_stdout("list-transports", "async");
    let fs = MemoryFs::new();
    let clock = FixedClock(4200);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();
    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, State::default(), opts(), model());
    engine.deploy().unwrap();

    let overlay = runner.env_overlay_of("run -d --name worker-async").unwrap();
    assert_eq!(overlay.get("REGISTRY").map(String::as_str), Some("reg"));

    // and the restart policy is re-applied, or the worker dies at the next reboot
    let calls = runner.display_calls();
    assert!(calls.iter().any(|c| c == "docker update --restart unless-stopped worker-async"));
}

#[test]
fn rollback_warns_when_recorded_env_keys_drift_from_the_chain() {
    // TC-037: recorded [DB_URL, TZ] vs current chain delivering [NEW_KEY, TZ].
    let cfg = cfg();
    let runner = RecordingRunner::new()
        .with_stdout("inspect demo-postgres", "reg:db-1")
        .with_stdout("list-transports", "async");
    let fs = MemoryFs::new();
    let clock = FixedClock(5100);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();

    let mut state = State::default();
    {
        let st = state.stage_mut("prod");
        let mut old = release(1, "demo-app-1", "reg:app-old", ReleaseStatus::Superseded);
        old.env_keys = vec!["DB_URL".to_string(), "TZ".to_string()];
        st.releases.push(old);
        st.releases.push(release(2, "demo-app-2", "reg:app-new", ReleaseStatus::Active));
        st.current = Some("demo-app-2".into());
    }
    let mut options = opts();
    options.container_env = [("NEW_KEY".to_string(), "secret-value".to_string())].into_iter().collect();

    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, state, options, model());
    engine.rollback().unwrap();

    let warned = reporter.lines().into_iter().find(|l| l.contains("different env keys"));
    let warned = warned.expect("expected a drift warning");
    assert!(warned.contains("+NEW_KEY"), "got: {warned}");
    assert!(warned.contains("-DB_URL"), "got: {warned}");
    // The VALUE of NEW_KEY is "secret-value"; only its NAME may be reported.
    assert!(!warned.contains("secret-value"), "the drift warning printed a value: {warned}");
}

#[test]
fn unchanged_env_keys_produce_no_drift_warning() {
    let cfg = cfg();
    let runner = RecordingRunner::new()
        .with_stdout("inspect demo-postgres", "reg:db-1")
        .with_stdout("list-transports", "async");
    let fs = MemoryFs::new();
    let clock = FixedClock(5200);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();
    let mut state = State::default();
    {
        let st = state.stage_mut("prod");
        let mut old = release(1, "demo-app-1", "reg:app-old", ReleaseStatus::Superseded);
        old.env_keys = Vec::new(); // exactly what the (empty) chain delivers today
        st.releases.push(old);
        st.releases.push(release(2, "demo-app-2", "reg:app-new", ReleaseStatus::Active));
        st.current = Some("demo-app-2".into());
    }
    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, state, opts(), model());
    engine.rollback().unwrap();
    assert!(
        !reporter.lines().iter().any(|l| l.contains("different env keys")),
        "no drift, no warning"
    );
}

#[test]
fn env_exclude_withholds_a_chain_key_from_the_release() {
    let src = r#"
version: 2
project: demo
compose:
  files: [base.yml]
release:
  container_prefix: demo-app
  run:
    image: reg:app-1
    env_exclude: ['DEPLOY_.*']
  healthcheck: { exec_in: nginx, cmd: 'curl {container}', retries: 1, interval: 1s }
cutover:
  service: nginx
  backend_port: 8080
  reload: { exec_in: nginx, cmd: 'nginx -s reload' }
services:
  nginx: { recreate: never }
stages:
  prod: {}
"#;
    let cfg = config::load(src, Some("prod"), &[], &HashMap::new()).unwrap();
    let runner = RecordingRunner::new();
    let fs = MemoryFs::new();
    let clock = FixedClock(4100);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();
    let mut options = opts();
    options.container_env = [
        ("DEPLOY_ROOT_TOKEN".to_string(), "x".to_string()),
        ("APP_SECRET".to_string(), "y".to_string()),
    ]
    .into_iter()
    .collect();
    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, State::default(), options, model());
    engine.deploy().unwrap();
    let run_line = runner
        .display_calls()
        .into_iter()
        .find(|c| c.contains("run -d --name demo-app-"))
        .unwrap();
    assert!(run_line.contains("-e APP_SECRET"));
    assert!(!run_line.contains("DEPLOY_ROOT_TOKEN"), "excluded key must not be delivered");
}

#[test]
fn lua_after_hook_runs_through_the_engine() {
    let cfg = cfg();
    let plugin = r#"
        task('warmup', function(ctx)
          ctx.in_release('php warmup ' .. ctx.cfg.project)
        end)
        after('healthcheck', 'warmup')
    "#;
    let host = crate::lua::LuaHost::load(&cfg, &[("p".into(), plugin.into())]).unwrap();
    let runner = RecordingRunner::new()
        .with_stdout("inspect demo-postgres", "reg:db-1")
        .with_stdout("list-transports", "async");
    let fs = MemoryFs::new();
    let clock = FixedClock(1000);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();

    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, State::default(), opts(), model())
        .with_plugins(&host);
    engine.deploy().unwrap();

    // the Lua after_healthcheck hook ran a ctx.in_release command through the engine
    assert!(runner
        .display_calls()
        .iter()
        .any(|c| c == "docker exec demo-app-1000 sh -c 'php warmup demo'"));
}

#[test]
fn plugin_mutating_cfg_changes_engine_behavior() {
    // before_finalize lowers retention via plain assignment; finalize must honor it.
    let cfg = cfg();
    let plugin = r#"
        before('finalize', function(ctx) ctx.cfg.retention.keep_releases = 1 end)
    "#;
    let host = crate::lua::LuaHost::load(&cfg, &[("p".into(), plugin.into())]).unwrap();
    let runner = RecordingRunner::new()
        .with_stdout("inspect demo-postgres", "reg:db-1")
        .with_stdout("list-transports", "async");
    let fs = MemoryFs::new();
    let clock = FixedClock(1000);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();
    let mut state = State::default();
    {
        let st = state.stage_mut("prod");
        for id in 1..=4 {
            st.releases.push(release(id, &format!("demo-app-{id}"), &format!("img{id}"), ReleaseStatus::Superseded));
        }
        st.releases.push(release(5, "demo-app-5", "imgC", ReleaseStatus::Active));
        st.current = Some("demo-app-5".into());
    }
    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, state, opts(), model())
        .with_plugins(&host);
    engine.deploy().unwrap();
    let calls = runner.display_calls();
    // default keep_releases is 3 (evicts img1,img2); the plugin lowered it to 1, so the
    // engine now also evicts img3 and img4 — proof the mutation reached the typed config.
    assert!(calls.iter().any(|c| c == "docker image rm img3"));
    assert!(calls.iter().any(|c| c == "docker image rm img4"));
    assert!(!calls.iter().any(|c| c == "docker image rm imgC")); // newest superseded kept
}

#[test]
fn plugin_mutating_state_persists_through_the_engine() {
    // after_cutover stamps the live release via plain assignment; it must reach disk + state.
    let cfg = cfg();
    let plugin = r#"
        after('cutover', function(ctx)
          ctx.state.releases[#ctx.state.releases].reason = 'plugin-stamped'
        end)
    "#;
    let host = crate::lua::LuaHost::load(&cfg, &[("p".into(), plugin.into())]).unwrap();
    let runner = RecordingRunner::new()
        .with_stdout("inspect demo-postgres", "reg:db-1")
        .with_stdout("list-transports", "async");
    let fs = MemoryFs::new();
    let clock = FixedClock(1000);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();
    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, State::default(), opts(), model())
        .with_plugins(&host);
    engine.deploy().unwrap();

    let persisted = State::from_json(&fs.read(std::path::Path::new("./dcd-state.json")).unwrap()).unwrap();
    assert_eq!(
        persisted.stage("prod").unwrap().find("demo-app-1000").unwrap().reason.as_deref(),
        Some("plugin-stamped"),
    );
    let prod = engine.into_state();
    assert_eq!(prod.stage("prod").unwrap().find("demo-app-1000").unwrap().reason.as_deref(), Some("plugin-stamped"));
}

/// `ctx.state` is a plugin's to shape — the pull ledger is not. Losing it would make
/// every tag dcd pulled unreclaimable again, silently.
#[test]
fn a_plugin_cannot_wipe_the_pull_ledger() {
    let cfg = cfg();
    let plugin = r#"
        after('cutover', function(ctx)
          ctx.state.pulled = {}
        end)
    "#;
    let host = crate::lua::LuaHost::load(&cfg, &[("p".into(), plugin.into())]).unwrap();
    let runner = RecordingRunner::new()
        .with_stdout("inspect demo-postgres", "reg:db-1")
        .with_stdout("list-transports", "async");
    let fs = MemoryFs::new();
    let clock = FixedClock(1000);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();
    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, State::default(), opts(), model())
        .with_plugins(&host);
    engine.deploy().unwrap();

    let after = engine.into_state();
    let ledger: Vec<&str> = after.stage("prod").unwrap().pulled.iter().map(|p| p.tag.as_str()).collect();
    assert_eq!(ledger, vec!["reg:app-1", "reg:db-1"]);
}

/// v1 rendered one compose service PER WORKER, so those containers carry
/// `service=worker-async` — invisible to v2's `service=worker` filter. Left alone
/// the first v2 deploy collides on the name post-cutover, on every existing install.
#[test]
fn the_first_v2_deploy_reaps_v1_workers_that_discovery_cannot_see() {
    let cfg = cfg();
    let runner = RecordingRunner::new()
        .with_stdout("inspect demo-postgres", "reg:db-1")
        .with_stdout("list-transports", "async")
        .with_stdout(
            r#"--format '{{.Names}} {{.Label "com.docker.compose.service"}}'"#,
            "worker-async worker-async
worker-keep worker
",
        );
    let fs = MemoryFs::new();
    let clock = FixedClock(1000);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();
    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, State::default(), opts(), model());
    engine.deploy().unwrap();

    let calls = runner.display_calls();
    // the v1-labelled worker goes...
    assert!(calls.iter().any(|c| c == "docker rm -f worker-async"), "{calls:?}");
    // ...and one already carrying the v2 service label is left alone
    assert!(!calls.iter().any(|c| c == "docker rm -f worker-keep"), "{calls:?}");
}

/// It is a one-time migration, not a per-deploy sweep: a stage that already has a
/// v2 release must never have its live workers reaped out from under it.
#[test]
fn the_v1_worker_reap_does_not_run_once_a_release_is_recorded() {
    let cfg = cfg();
    let runner = RecordingRunner::new()
        .with_stdout("inspect demo-postgres", "reg:db-1")
        .with_stdout("list-transports", "async")
        .with_stdout(
            r#"--format {{.Names}} {{.Label "com.docker.compose.service"}}"#,
            "worker-async worker-async\n",
        );
    let fs = MemoryFs::new();
    let clock = FixedClock(2000);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();
    let mut state = State::default();
    {
        let st = state.stage_mut("prod");
        st.releases.push(release(1, "demo-app-1", "reg:app-1", ReleaseStatus::Active));
        st.current = Some("demo-app-1".into());
    }
    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, state, opts(), model());
    engine.deploy().unwrap();

    assert!(!runner.display_calls().iter().any(|c| c == "docker rm -f worker-async"));
}

/// `--image app=<ref>` is applied by pinning the compose model (spec §5.5). The
/// regression it guards: the flag used to be translated into `--set
/// docker.images.<name>`, a config path v2 removed, so every run carrying it died
/// in config load. Pinning the model is what makes one pin reach the generated
/// override, the pull and the release record at once.
#[test]
fn an_image_pin_reaches_the_override_the_pull_and_the_release_record() {
    let cfg = cfg();
    let mut pinned = model();
    pinned.pin_image("app", "reg:app-99").unwrap();

    let runner = RecordingRunner::new()
        .with_stdout("inspect demo-postgres", "reg:db-1")
        .with_stdout("list-transports", "async");
    let fs = MemoryFs::new();
    let clock = FixedClock(7000);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();

    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, State::default(), opts(), pinned);
    engine.deploy().unwrap();

    let calls = runner.display_calls();
    assert!(calls.iter().any(|c| c == "docker pull reg:app-99"), "the pinned tag is what gets pulled");
    assert!(calls.iter().any(|c| c == "docker pull reg:app-1"), "a pin is per-service: the worker keeps its own tag");

    let override_document = String::from_utf8(fs.read(std::path::Path::new("./dcd-image-override.prod.yml")).unwrap()).unwrap();
    assert!(override_document.contains("  app:\n    image: reg:app-99"), "override missing the pin: {override_document}");
    assert!(override_document.contains("  worker:\n    image: reg:app-1"), "the pin must not leak to other services: {override_document}");

    let state = String::from_utf8(fs.read(std::path::Path::new("./dcd-state.json")).unwrap()).unwrap();
    assert!(state.contains("reg:app-99"), "the release record must replay the pinned image: {state}");
}

/// A typo must name the services that do exist, not deploy something unpinned.
#[test]
fn an_image_pin_for_an_unknown_service_is_a_config_error() {
    let error = model().pin_image("ap", "reg:app-99").unwrap_err();
    let message = error.to_string();
    assert!(message.contains("--image 'ap' is not a service"), "{message}");
    assert!(message.contains("app"), "the error must list the known services: {message}");
}

/// §8.2: `docker compose config` inlines every resolved env value, so its stdout is
/// the whole secret set. dcd's own model resolution bypasses the reporter, but a
/// `compose:` hook or `ctx.compose` routes through `run_argv` — where `-v` would
/// otherwise print all of it.
#[test]
fn a_compose_config_hook_never_traces_its_resolved_output() {
    let source = cfg_src().replace(
        "stages:\n  prod: {}",
        "hooks:\n  after_pull:\n    - compose: ['config']\nstages:\n  prod: {}",
    );
    let cfg = config::load(&source, Some("prod"), &[], &HashMap::new()).unwrap();

    let runner = RecordingRunner::new()
        .with_stdout("inspect demo-postgres", "reg:db-1")
        .with_stdout("list-transports", "async")
        .with_stdout(
            "config",
            r#"{"services":{"app":{"image":"reg:app-1","environment":{"DB_PASSWORD":"hunter2-SECRET"}}}}"#,
        );
    let fs = MemoryFs::new();
    let clock = FixedClock(1000);
    let reporter = Reporter::capture_verbose(Mode::Plain);
    let interrupt = Interrupt::inert();

    let mut engine = Engine::new(cfg, &runner, &fs, &clock, &reporter, &interrupt, State::default(), opts(), model());
    engine.deploy().unwrap();

    let trace = reporter.lines().join("\n");
    assert!(!trace.contains("hunter2-SECRET"), "the resolved model reached the trace: {trace}");
    assert!(trace.contains("output suppressed"), "the suppression must be visible: {trace}");
}
