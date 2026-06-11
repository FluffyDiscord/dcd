use std::collections::HashMap;

use super::*;
use crate::config;
use crate::effects::{CmdOutput, FixedClock, MemoryFs, RecordingRunner};
use crate::redact::Redactor;
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
    }
}

fn cfg() -> config::Config {
    let src = r#"
version: 1
project: demo
network: demo_net
registry: reg
images:
  app: app-1
  database: db-1
compose:
  files: [base.yml]
  env_file: compose.env
  env:
    REGISTRY: reg
preflight:
  directories:
    - { path: .docker/logs }
services:
  postgres:
    image: database
    container: demo-postgres
    on_recreate_drain_workers: true
    wait: { exec_in: demo-postgres, cmd: 'pg_isready', retries: 3, interval: 1s }
  nginx:
    container: demo-nginx
    recreate: never
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
"#;
    config::load(src, Some("prod"), &[], &HashMap::new()).unwrap().config
}

fn opts() -> Options {
    Options {
        dry_run: false,
        sleep_enabled: false,
        reason: None,
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
    let redactor = Redactor::default();
    let interrupt = Interrupt::inert();

    let mut engine = Engine::new(&cfg, &runner, &fs, &clock, &reporter, &redactor, &interrupt, State::default(), opts());
    engine.deploy().unwrap();

    let calls = runner.display_calls();
    let has = |needle: &str| calls.iter().any(|c| c == needle);

    assert!(has("docker pull reg:app-1"));
    assert!(has("docker pull reg:db-1"));
    assert!(has("docker inspect demo-postgres --format {{.Config.Image}}"));
    assert!(has("docker compose -p demo --env-file compose.env -f base.yml up -d --no-recreate postgres"));
    assert!(has("docker exec demo-postgres sh -c pg_isready"));
    assert!(has("docker run --rm --network demo_net --name demo-migrate-1000 reg:app-1 migrate before"));
    assert!(has("docker run -d --name demo-app-1000 --network demo_net --network-alias app-rr --restart unless-stopped -e TZ=UTC reg:app-1"));
    assert!(has("docker exec demo-nginx sh -c curl -sf http://demo-app-1000:2114/health"));
    assert!(has("docker exec demo-nginx sh -c nginx -s reload"));
    assert!(has("docker exec demo-app-1000 migrate after"));
    assert!(has("docker compose -p demo --env-file compose.env -f base.yml -f workers.yml up -d worker-async worker-scheduler"));

    // healthcheck targets the container NAME, never the shared alias (spec §7.7)
    assert!(!calls.iter().any(|c| c.contains("http://app-rr:")));

    let state = engine.into_state();
    let prod = state.stage("prod").unwrap();
    assert_eq!(prod.current.as_deref(), Some("demo-app-1000"));
    assert_eq!(prod.find("demo-app-1000").unwrap().status, crate::state::ReleaseStatus::Active);

    // compose.env + workers compose + state were written
    assert!(fs.exists(std::path::Path::new("./compose.env")));
    assert!(fs.exists(std::path::Path::new("./dcd-state.json")));
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
    let redactor = Redactor::default();
    let interrupt = Interrupt::inert();

    let mut engine = Engine::new(&cfg, &runner, &fs, &clock, &reporter, &redactor, &interrupt, State::default(), opts());
    let err = engine.deploy().unwrap_err();

    assert_eq!(err.exit_code(), 1); // pre-cutover, red would still be serving
    let calls = runner.display_calls();
    assert!(calls.iter().any(|c| c == "docker rm -f demo-app-2000")); // black cleaned up
    assert!(!calls.iter().any(|c| c.contains("nginx -s reload"))); // never cut over

    let state = engine.into_state();
    assert!(state.stage("prod").is_none()); // state unchanged
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
    let redactor = Redactor::default();
    let interrupt = Interrupt::inert();

    let mut engine = Engine::new(
        &cfg,
        &runner,
        &fs,
        &clock,
        &reporter,
        &redactor,
        &interrupt,
        State::default(),
        Options {
            dry_run: true,
            sleep_enabled: false,
            reason: None,
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
    let redactor = Redactor::default();
    let interrupt = Interrupt::inert();

    let mut state = State::default();
    {
        let st = state.stage_mut("prod");
        st.releases.push(release(1, "demo-app-1", "reg:app-old", ReleaseStatus::Superseded));
        st.releases.push(release(2, "demo-app-2", "reg:app-new", ReleaseStatus::Active));
        st.current = Some("demo-app-2".into());
    }

    let mut engine = Engine::new(&cfg, &runner, &fs, &clock, &reporter, &redactor, &interrupt, state, opts());
    engine.rollback().unwrap();

    let calls = runner.display_calls();
    assert!(calls.iter().any(|c| c == "docker pull reg:app-old"));
    assert!(calls.iter().any(|c| c
        == "docker run -d --name demo-app-5000 --network demo_net --network-alias app-rr --restart unless-stopped -e TZ=UTC reg:app-old"));
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
    let redactor = Redactor::default();
    let interrupt = Interrupt::inert();

    let mut state = State::default();
    {
        let st = state.stage_mut("prod");
        st.releases.push(release(1, "demo-app-prev", "reg:app-1", ReleaseStatus::Active));
        st.releases.push(release(2, "demo-app-9", "reg:app-2", ReleaseStatus::CutoverPending));
        st.current = Some("demo-app-prev".into());
    }

    let mut engine = Engine::new(&cfg, &runner, &fs, &clock, &reporter, &redactor, &interrupt, state, opts());
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
    let redactor = Redactor::default();
    let interrupt = Interrupt::inert();

    // Deploy that fails AFTER cutover (migrate:after errors) -> exit 4.
    let runner1 = RecordingRunner::new()
        .with_stdout("inspect demo-postgres", "reg:db-1")
        .with_response(
            "demo-app-7000 migrate after",
            CmdOutput { code: 1, stdout: String::new(), stderr: "boom".into() },
        );
    let mut e1 = Engine::new(&cfg, &runner1, &fs, &clock, &reporter, &redactor, &interrupt, State::default(), opts());
    let err = e1.deploy().unwrap_err();
    assert_eq!(err.exit_code(), 4); // post-cutover failure, black is live

    // The cutover_pending release MUST be on disk (INV-3) so resume can find it.
    let bytes = fs.read(std::path::Path::new("./dcd-state.json")).unwrap();
    let persisted = State::from_json(&bytes).unwrap();
    let pending = persisted.stage("prod").unwrap().cutover_pending().unwrap();
    assert_eq!(pending.container, "demo-app-7000");

    // A fresh engine resumes from the persisted state and finalizes.
    let runner2 = RecordingRunner::new().with_stdout("list-transports", "async");
    let mut e2 = Engine::new(&cfg, &runner2, &fs, &clock, &reporter, &redactor, &interrupt, persisted, opts());
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
    let redactor = Redactor::default();
    let interrupt = Interrupt::inert();
    let mut state = State::default();
    state.stage_mut("prod").releases.push(release(1, "demo-app-1", "reg:app-1", ReleaseStatus::CutoverPending));

    let mut engine = Engine::new(&cfg, &runner, &fs, &clock, &reporter, &redactor, &interrupt, state, opts());
    let err = engine.deploy().unwrap_err();
    assert_eq!(err.exit_code(), 4);
    assert!(runner.display_calls().is_empty()); // nothing ran
}

#[test]
fn resume_refuses_more_than_one_pending() {
    let cfg = cfg();
    let runner = RecordingRunner::new();
    let fs = MemoryFs::new();
    let clock = FixedClock(8100);
    let reporter = Reporter::capture(Mode::Plain);
    let redactor = Redactor::default();
    let interrupt = Interrupt::inert();
    let mut state = State::default();
    {
        let st = state.stage_mut("prod");
        st.releases.push(release(1, "a", "i1", ReleaseStatus::CutoverPending));
        st.releases.push(release(2, "b", "i2", ReleaseStatus::CutoverPending));
    }
    let mut engine = Engine::new(&cfg, &runner, &fs, &clock, &reporter, &redactor, &interrupt, state, opts());
    let err = engine.resume().unwrap_err();
    assert_eq!(err.exit_code(), 2);
    assert!(err.to_string().contains("more than one"));
}

#[test]
fn finalize_garbage_collects_evicted_images() {
    let cfg = cfg(); // retention defaults: keep_releases 3
    let runner = RecordingRunner::new()
        .with_stdout("inspect demo-postgres", "reg:db-1")
        .with_stdout("list-transports", "async");
    let fs = MemoryFs::new();
    let clock = FixedClock(1000);
    let reporter = Reporter::capture(Mode::Plain);
    let redactor = Redactor::default();
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
    let mut engine = Engine::new(&cfg, &runner, &fs, &clock, &reporter, &redactor, &interrupt, state, opts());
    engine.deploy().unwrap();
    let calls = runner.display_calls();
    // after finalize: superseded = img1..img4 + imgC (demo-app-5 demoted); keep 3 -> evict img1,img2
    assert!(calls.iter().any(|c| c == "docker image rm img1"));
    assert!(calls.iter().any(|c| c == "docker image rm img2"));
    assert!(!calls.iter().any(|c| c == "docker image rm imgC")); // current never removed
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
    let redactor = Redactor::default();
    let interrupt = Interrupt::inert();
    let mut engine = Engine::new(&cfg, &runner, &fs, &clock, &reporter, &redactor, &interrupt, State::default(), opts());
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
fn secret_value_in_stderr_is_redacted_in_the_error() {
    let cfg = cfg();
    let runner = RecordingRunner::new().with_response(
        "docker pull reg:app-1",
        CmdOutput { code: 1, stdout: String::new(), stderr: "denied for key s3cr3t-token".into() },
    );
    let fs = MemoryFs::new();
    let clock = FixedClock(9100);
    let reporter = Reporter::capture(Mode::Plain);
    let redactor = Redactor::new(["s3cr3t-token".to_string()]);
    let interrupt = Interrupt::inert();
    let mut engine = Engine::new(&cfg, &runner, &fs, &clock, &reporter, &redactor, &interrupt, State::default(), opts());
    let err = engine.deploy().unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("***"));
    assert!(!msg.contains("s3cr3t-token"));
}

#[test]
fn second_synthetic_config_drives_plan_with_zero_core_changes() {
    let src = r#"
version: 1
project: blogapp
network: blogapp_net
images:
  app: app1
  web: web1
compose:
  files: [compose.prod.yml]
  env_file: compose.env
  env: { COMPOSE_PROJECT_NAME: blogapp }
services:
  web:
    image: web
    container: blogapp-web
    recreate: on-image-change
    wait: { exec_in: blogapp-web, cmd: 'wget -qO- localhost/up', retries: 2, interval: 1s }
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
    let cfg = config::load(src, Some("prod"), &[], &HashMap::new()).unwrap().config;
    let runner = RecordingRunner::new().with_stdout("inspect blogapp-web", "web1");
    let fs = MemoryFs::new();
    let clock = FixedClock(1234);
    let reporter = Reporter::capture(Mode::Plain);
    let redactor = Redactor::default();
    let interrupt = Interrupt::inert();
    let mut engine = Engine::new(&cfg, &runner, &fs, &clock, &reporter, &redactor, &interrupt, State::default(), opts());
    engine.deploy().unwrap();
    let calls = runner.display_calls();
    assert!(!calls.iter().any(|c| c.contains("migrate"))); // no migrate config -> skipped
    assert!(!calls.iter().any(|c| c.contains("postgres"))); // no postgres service
    assert!(calls.iter().any(|c| c == "docker run -d --name blogapp-app-1234 --network blogapp_net --network-alias app --restart unless-stopped app1"));
    assert!(calls.iter().any(|c| c.contains("up -d worker-default worker-mail"))); // static workers
    assert!(calls.iter().any(|c| c == "docker exec blogapp-web sh -c wget -qO- http://blogapp-app-1234:9000/up"));
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
    let redactor = Redactor::default();
    let interrupt = Interrupt::inert();

    let mut engine = Engine::new(&cfg, &runner, &fs, &clock, &reporter, &redactor, &interrupt, State::default(), opts())
        .with_plugins(&host);
    engine.deploy().unwrap();

    // the Lua after_healthcheck hook ran a ctx.in_release command through the engine
    assert!(runner.display_calls().iter().any(|c| c == "docker exec demo-app-1000 sh -c php warmup demo"));
}
