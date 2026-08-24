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
version: 1
project: demo
network: demo_net
registry: reg
docker:
  images:
    app: app-1
    database: db-1
  services:
    postgres:
      image: database
      container: demo-postgres
      on_recreate_drain_workers: true
      wait: { exec_in: demo-postgres, cmd: 'pg_isready', retries: 3, interval: 1s }
    nginx:
      container: demo-nginx
      recreate: never
compose:
  files: [base.yml]
  env:
    REGISTRY: reg
directories:
  - { path: .docker/logs }
release:
  image: app
  container_prefix: demo-app
  run: { network_alias: app-rr, env: { TZ: UTC } }
  healthcheck: { exec_in: demo-nginx, cmd: 'curl -sf http://{container}:2114/health', retries: 3, interval: 1s }
  migrate: { before: 'migrate before', after: 'migrate after' }
  drain: 'graceful-stop'
cutover:
  backend_port: 8080
  reload: { exec_in: demo-nginx, cmd: 'nginx -s reload' }
workers:
  compose_file: workers.yml
  provider: { command_in_release: 'list-transports' }
  template: { image: app, entrypoint: ['php', 'consume'], command: ['{name}'] }
stages:
  prod: {}
"#
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

    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, State::default(), opts());
    engine.deploy().unwrap();

    let calls = runner.display_calls();
    let has = |needle: &str| calls.iter().any(|c| c == needle);

    assert!(has("docker pull reg:app-1"));
    assert!(has("docker pull reg:db-1"));
    assert!(has("docker inspect demo-postgres --format {{.Config.Image}}"));
    assert!(has("docker compose -p demo --env-file /dev/null -f base.yml up -d --no-recreate postgres"));
    assert!(has("docker exec demo-postgres sh -c pg_isready"));
    assert!(has("docker run --rm --network demo_net --name demo-migrate-1000 -e TZ reg:app-1 migrate before"));
    assert!(has("docker run -d --name demo-app-1000 --network demo_net --network-alias app-rr --restart unless-stopped -e TZ reg:app-1"));
    assert!(has("docker exec demo-nginx sh -c curl -sf http://demo-app-1000:2114/health"));
    assert!(has("docker exec demo-nginx sh -c nginx -s reload"));
    assert!(has("docker exec demo-app-1000 migrate after"));
    assert!(has("docker compose -p demo --env-file /dev/null -f base.yml -f workers.yml up -d worker-async worker-scheduler"));

    // healthcheck targets the container NAME, never the shared alias (spec §7.7)
    assert!(!calls.iter().any(|c| c.contains("http://app-rr:")));

    let state = engine.into_state();
    let prod = state.stage("prod").unwrap();
    assert_eq!(prod.current.as_deref(), Some("demo-app-1000"));
    assert_eq!(prod.find("demo-app-1000").unwrap().status, crate::state::ReleaseStatus::Active);

    // no env file is ever rendered (spec §5.2.4); workers compose + state were written
    assert!(!fs.exists(std::path::Path::new("./compose.env")));
    assert!(fs.exists(std::path::Path::new("./workers.yml")));
    assert!(fs.exists(std::path::Path::new("./dcd-state.json")));
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

    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, State::default(), opts());
    engine.deploy().unwrap();

    let lines = reporter.lines();
    let traced: Vec<&String> = lines.iter().filter(|line| line.starts_with("$ ")).collect();
    assert_eq!(traced.len(), runner.display_calls().len());
    assert!(traced.iter().any(|line| *line == "$ docker pull reg:app-1"));
    assert!(lines.iter().any(|line| line.starts_with("  exit 0 in ")));

    // captured stdout is surfaced instead of being dropped
    assert!(lines.iter().any(|line| line == "  | async"));

    // env values never reach the trace — passthrough is a bare `-e KEY` (§5.2.4)
    assert!(!lines.iter().any(|line| line.contains("TZ=")));
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

    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, State::default(), opts());
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

    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, State::default(), opts());
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
    );
    engine.deploy().unwrap();

    // no files written in dry-run
    assert!(!fs.exists(std::path::Path::new("./compose.env")));
    assert!(!fs.exists(std::path::Path::new("./dcd-state.json")));
    // every mutating docker command was stubbed, not executed
    let stubbed = runner.stubbed();
    assert!(stubbed.iter().any(|a| a.display().starts_with("docker run -d --name demo-app-3000")));
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

    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, state, opts());
    engine.rollback().unwrap();

    let calls = runner.display_calls();
    assert!(calls.iter().any(|c| c == "docker pull reg:app-old"));
    assert!(calls.iter().any(|c| c
        == "docker run -d --name demo-app-5000 --network demo_net --network-alias app-rr --restart unless-stopped -e TZ reg:app-old"));
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

    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, state, opts());
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
    let mut e1 = Engine::new(cfg.clone(), &runner1, &fs, &clock, &reporter, &interrupt, State::default(), opts());
    let err = e1.deploy().unwrap_err();
    assert_eq!(err.exit_code(), 4); // post-cutover failure, black is live

    // The cutover_pending release MUST be on disk (INV-3) so resume can find it.
    let bytes = fs.read(std::path::Path::new("./dcd-state.json")).unwrap();
    let persisted = State::from_json(&bytes).unwrap();
    let pending = persisted.stage("prod").unwrap().cutover_pending().unwrap();
    assert_eq!(pending.container, "demo-app-7000");

    // A fresh engine resumes from the persisted state and finalizes.
    let runner2 = RecordingRunner::new().with_stdout("list-transports", "async");
    let mut e2 = Engine::new(cfg.clone(), &runner2, &fs, &clock, &reporter, &interrupt, persisted, opts());
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

    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, state, opts());
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

    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, state, opts());
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

    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, state, opts());
    assert_eq!(engine.unlock().unwrap(), None);
    assert!(runner.display_calls().is_empty());
    assert!(!fs.exists(std::path::Path::new("./dcd-state.json")));
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

    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, state, opts());
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
    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, state, unlock_opts);
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
    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, state, opts());
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
    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, state, opts());
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
    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, state, opts());
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

    let engine = Engine::new(cfg, &runner, &fs, &clock, &reporter, &interrupt, state, opts());
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
    assert_eq!(queries, vec!["docker images reg.example.com/demo --format {{.Repository}}:{{.Tag}}".to_string()]);
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
    let engine = Engine::new(cfg, &runner, &fs, &clock, &reporter, &interrupt, state, opts());
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
    let mut engine = Engine::new(cfg, &runner, &fs, &clock, &reporter, &interrupt, state, opts());
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
    let src = cfg_src()
        .replace("registry: reg\n", "")
        .replace("app: app-1", "app: reg.example.com/demo:app-1")
        .replace("database: db-1", "database: postgres:16");
    let cfg = config::load(&src, Some("prod"), &[], &HashMap::new()).unwrap();
    let runner = RecordingRunner::new().with_stdout("docker images reg.example.com/demo", "reg.example.com/demo:old\n");
    let fs = MemoryFs::new();
    let clock = FixedClock(1000);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();
    let engine = Engine::new(cfg, &runner, &fs, &clock, &reporter, &interrupt, State::default(), opts());
    let plan = engine.gc_plan(true).unwrap();
    assert_eq!(plan.orphans, vec!["reg.example.com/demo:old".to_string()]);
    assert!(reporter.lines().iter().any(|line| line.contains("not sweeping 'postgres'")));
    assert!(!runner.display_calls().iter().any(|c| c.contains("docker images postgres")));
}

/// A `/` proves nothing on its own: `bitnami/postgresql` is a Docker Hub namespace,
/// not our registry. And an image no service or worker uses is not ours to sweep.
#[test]
fn gc_all_skips_a_namespaced_hub_image_and_an_image_nothing_uses() {
    let src = cfg_src()
        .replace("registry: reg\n", "")
        .replace("app: app-1", "app: reg.example.com/demo:app-1\n    toolbox: ghcr.io/other/toolbox:1")
        .replace("database: db-1", "database: bitnami/postgresql:16");
    let cfg = config::load(&src, Some("prod"), &[], &HashMap::new()).unwrap();
    let runner = RecordingRunner::new().with_stdout("docker images reg.example.com/demo", "reg.example.com/demo:old\n");
    let fs = MemoryFs::new();
    let clock = FixedClock(1000);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();
    let engine = Engine::new(cfg, &runner, &fs, &clock, &reporter, &interrupt, State::default(), opts());
    let plan = engine.gc_plan(true).unwrap();
    assert_eq!(plan.orphans, vec!["reg.example.com/demo:old".to_string()]);
    assert!(reporter.lines().iter().any(|line| line.contains("not sweeping 'bitnami/postgresql'")));
    // `toolbox` is declared but no service or worker template uses it — never queried
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
    let engine = Engine::new(cfg, &runner, &fs, &clock, &reporter, &interrupt, State::default(), opts());
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
    let mut engine = Engine::new(cfg, &runner, &fs, &clock, &reporter, &interrupt, state, opts());
    engine.deploy().unwrap();
    assert!(!runner.display_calls().iter().any(|c| c == "docker image rm reg:app-dead"));
}

#[test]
fn infra_drains_workers_before_recreating_db_and_waits_after() {
    let cfg = cfg();
    let runner = RecordingRunner::new()
        .with_stdout("inspect demo-postgres", "reg:db-OLD") // != desired reg:db-1 -> recreate
        .with_stdout("worker- --format", "demo-worker-async")
        .with_stdout("list-transports", "async");
    let fs = MemoryFs::new();
    let clock = FixedClock(9000);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();
    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, State::default(), opts());
    engine.deploy().unwrap();
    let calls = runner.display_calls();
    let idx = |needle: &str| calls.iter().position(|c| c.contains(needle)).unwrap_or(usize::MAX);
    let stop = idx("docker stop --timeout");
    let recreate = idx("up -d postgres");
    let wait = idx("sh -c pg_isready");
    assert!(stop < recreate, "worker drain must precede DB recreate");
    assert!(recreate < wait, "wait gate must follow recreate");
}

#[test]
fn second_synthetic_config_drives_plan_with_zero_core_changes() {
    let src = r#"
version: 1
project: blogapp
network: blogapp_net
docker:
  images:
    app: app1
    web: web1
  services:
    web:
      image: web
      container: blogapp-web
      recreate: on-image-change
      wait: { exec_in: blogapp-web, cmd: 'wget -qO- localhost/up', retries: 2, interval: 1s }
compose:
  files: [compose.prod.yml]
  env: { COMPOSE_PROJECT_NAME: blogapp }
release:
  image: app
  container_prefix: blogapp-app
  run: { network_alias: app }
  healthcheck: { exec_in: blogapp-web, cmd: 'wget -qO- http://{container}:9000/up', retries: 2, interval: 1s }
cutover:
  backend_port: 9000
  reload: { exec_in: blogapp-web, cmd: 'nginx -s reload' }
workers:
  compose_file: workers.yml
  provider: { static: [default, mail] }
  template: { image: app, entrypoint: ['php', 'artisan', 'queue:work'], command: ['{name}'] }
stages:
  prod: {}
"#;
    let cfg = config::load(src, Some("prod"), &[], &HashMap::new()).unwrap();
    let runner = RecordingRunner::new().with_stdout("inspect blogapp-web", "web1");
    let fs = MemoryFs::new();
    let clock = FixedClock(1234);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();
    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, State::default(), opts());
    engine.deploy().unwrap();
    let calls = runner.display_calls();
    assert!(!calls.iter().any(|c| c.contains("migrate"))); // no migrate config -> skipped
    assert!(!calls.iter().any(|c| c.contains("postgres"))); // no postgres service
    assert!(calls.iter().any(|c| c == "docker run -d --name blogapp-app-1234 --network blogapp_net --network-alias app --restart unless-stopped app1"));
    assert!(calls.iter().any(|c| c.contains("up -d worker-default worker-mail"))); // static workers
    assert!(calls.iter().any(|c| c == "docker exec blogapp-web sh -c wget -qO- http://blogapp-app-1234:9000/up"));
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

    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, State::default(), options);
    engine.deploy().unwrap();

    let calls = runner.display_calls();
    assert!(calls.iter().any(|c| c
        == "docker run -d --name demo-app-4000 --network demo_net --network-alias app-rr --restart unless-stopped -e APP_SECRET -e DATABASE_URL -e TZ reg:app-1"));

    // run.env values arrive as the overlay of the docker run command, never argv
    let overlay = runner.env_overlay_of("docker run -d --name demo-app-4000").unwrap();
    assert_eq!(overlay.get("TZ").map(String::as_str), Some("UTC"));

    // compose calls carry the compose.env overlay
    let overlay = runner.env_overlay_of("up -d --no-recreate postgres").unwrap();
    assert_eq!(overlay.get("REGISTRY").map(String::as_str), Some("reg"));

    // the workers file lists key NAMES only; no secret value in any written file
    let workers_yaml = String::from_utf8(fs.read(std::path::Path::new("./workers.yml")).unwrap()).unwrap();
    assert!(workers_yaml.contains("            - APP_SECRET\n"));
    assert!(workers_yaml.contains("            - DATABASE_URL\n"));
    assert!(!workers_yaml.contains("hunter2"));
    let state_json = String::from_utf8(fs.read(std::path::Path::new("./dcd-state.json")).unwrap()).unwrap();
    assert!(!state_json.contains("hunter2"));
    assert!(!calls.iter().any(|c| c.contains("hunter2")), "no value in any argv");

    // TC-037: the release records its delivered key names
    let state = engine.into_state();
    let release = state.stage("prod").unwrap().find("demo-app-4000").unwrap().clone();
    assert_eq!(release.env_keys, vec!["APP_SECRET", "DATABASE_URL", "TZ"]);
}

#[test]
fn template_env_wins_over_compose_env_in_the_workers_up_overlay() {
    // TC-036 second clause: the workers `up` carries compose.env with template.env over it.
    let src = cfg_src().replace(
        "  template: { image: app, entrypoint: ['php', 'consume'], command: ['{name}'] }",
        "  template: { image: app, entrypoint: ['php', 'consume'], command: ['{name}'], env: { REGISTRY: tmpl-wins, TZ: UTC } }",
    );
    let cfg = config::load(&src, Some("prod"), &[], &HashMap::new()).unwrap();
    let runner = RecordingRunner::new()
        .with_stdout("inspect demo-postgres", "reg:db-1")
        .with_stdout("list-transports", "async");
    let fs = MemoryFs::new();
    let clock = FixedClock(4200);
    let reporter = Reporter::capture(Mode::Plain);
    let interrupt = Interrupt::inert();
    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, State::default(), opts());
    engine.deploy().unwrap();

    let overlay = runner.env_overlay_of("up -d worker-async").unwrap();
    assert_eq!(overlay.get("REGISTRY").map(String::as_str), Some("tmpl-wins"));
    assert_eq!(overlay.get("TZ").map(String::as_str), Some("UTC"));
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
    options.container_env = [("NEW_KEY".to_string(), "v".to_string())].into_iter().collect();

    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, state, options);
    engine.rollback().unwrap();

    let warned = reporter.lines().into_iter().find(|l| l.contains("different env keys"));
    let warned = warned.expect("expected a drift warning");
    assert!(warned.contains("+NEW_KEY"), "got: {warned}");
    assert!(warned.contains("-DB_URL"), "got: {warned}");
    assert!(!warned.contains('v') || warned.contains("env"), "values never printed");
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
        old.env_keys = vec!["TZ".to_string()]; // exactly what run.env delivers today
        st.releases.push(old);
        st.releases.push(release(2, "demo-app-2", "reg:app-new", ReleaseStatus::Active));
        st.current = Some("demo-app-2".into());
    }
    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, state, opts());
    engine.rollback().unwrap();
    assert!(
        !reporter.lines().iter().any(|l| l.contains("different env keys")),
        "no drift, no warning"
    );
}

#[test]
fn env_exclude_withholds_a_chain_key_from_the_release() {
    let src = r#"
version: 1
project: demo
network: demo_net
registry: reg
docker:
  images: { app: app-1 }
  services:
    nginx: { container: demo-nginx, recreate: never }
compose:
  files: [base.yml]
release:
  image: app
  container_prefix: demo-app
  run:
    env_exclude: ['DEPLOY_.*']
  healthcheck: { exec_in: demo-nginx, cmd: 'curl {container}', retries: 1, interval: 1s }
cutover:
  backend_port: 8080
  reload: { exec_in: demo-nginx, cmd: 'nginx -s reload' }
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
    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, State::default(), options);
    engine.deploy().unwrap();
    let run_line = runner
        .display_calls()
        .into_iter()
        .find(|c| c.starts_with("docker run -d"))
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

    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, State::default(), opts())
        .with_plugins(&host);
    engine.deploy().unwrap();

    // the Lua after_healthcheck hook ran a ctx.in_release command through the engine
    assert!(runner.display_calls().iter().any(|c| c == "docker exec demo-app-1000 sh -c php warmup demo"));
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
    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, state, opts())
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
    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, State::default(), opts())
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
    let mut engine = Engine::new(cfg.clone(), &runner, &fs, &clock, &reporter, &interrupt, State::default(), opts())
        .with_plugins(&host);
    engine.deploy().unwrap();

    let after = engine.into_state();
    let ledger: Vec<&str> = after.stage("prod").unwrap().pulled.iter().map(|p| p.tag.as_str()).collect();
    assert_eq!(ledger, vec!["reg:app-1", "reg:db-1"]);
}
