//! Real-Docker integration tests (spec §10.2). Gated on `DCD_E2E=1` so the normal
//! `cargo test` run stays daemon-free; CI sets it with a Docker service.
//!
//!   DCD_E2E=1 cargo test --test e2e -- --test-threads=1
//!
//! Each test provisions a scratch network + an `nginx:alpine` managed service, drives
//! the real `dcd` binary, and cleans everything up via the `Fixture` drop guard.

use std::path::PathBuf;
use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;

fn enabled() -> bool {
    std::env::var("DCD_E2E").is_ok()
}

fn docker(args: &[&str]) -> std::process::Output {
    Command::new("docker").args(args).output().expect("docker available")
}

struct Fixture {
    project: String,
    dir: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Fixture {
        let project = format!("dcdit{}{}", std::process::id(), tag);
        let dir = std::env::temp_dir().join(&project);
        std::fs::create_dir_all(&dir).unwrap();

        let dcd_yaml = format!(
            r#"version: 2
project: {project}
deploy_root: .
compose:
  files: [compose.prod.yml]
release:
  service: app
  container_prefix: {project}-app
  drain: 'true'
  healthcheck: {{ exec_in: nginx, cmd: 'wget -qO- -T 2 http://{{container}}/', retries: 15, interval: 1s }}
cutover:
  service: nginx
  upstream_file: upstream.conf
  backend_port: 80
  reload: {{ exec_in: nginx, cmd: 'true' }}
services:
  nginx:
    recreate: never
    wait: {{ exec_in: nginx, cmd: 'wget -qO- -T 2 http://localhost/ >/dev/null 2>&1 || true', retries: 10, interval: 1s }}
stages: {{ it: {{}} }}
"#
        );
        std::fs::write(dir.join("dcd.yaml"), dcd_yaml).unwrap();

        // Under v2 the compose file is the single source of container definition
        // (ADR-013): the release service lives here too, behind the `dcd-release`
        // profile so a hand-run `compose up` never starts a second copy.
        let compose = format!(
            r#"services:
  app:
    image: nginx:alpine
    profiles: ["dcd-release"]
    networks:
      default:
        aliases: [app]
  nginx:
    image: nginx:alpine
    container_name: {project}-nginx
networks:
  default:
    name: {project}_net
"#
        );
        std::fs::write(dir.join("compose.prod.yml"), compose).unwrap();

        Fixture { project, dir }
    }

    fn deploy(&self) -> std::process::Output {
        self.dcd(&["deploy", "it"])
    }

    fn dcd(&self, args: &[&str]) -> std::process::Output {
        Command::cargo_bin("dcd")
            .unwrap()
            .current_dir(&self.dir)
            .args(args)
            .output()
            .unwrap()
    }

    fn running(&self, name_prefix: &str) -> Vec<String> {
        let out = docker(&[
            "ps",
            "--filter",
            &format!("name=^{}-{name_prefix}", self.project),
            "--format",
            "{{.Names}}",
        ]);
        String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let containers = docker(&["ps", "-aq", "--filter", &format!("name=^{}", self.project)]);
        for id in String::from_utf8_lossy(&containers.stdout).split_whitespace() {
            docker(&["rm", "-f", id]);
        }
        docker(&["network", "rm", &format!("{}_net", self.project)]);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn it_001_happy_deploy_and_006_no_recreate() {
    if !enabled() {
        return;
    }
    let fx = Fixture::new("happy");

    let out = fx.deploy();
    assert!(out.status.success(), "deploy failed: {}", String::from_utf8_lossy(&out.stderr));

    // the black app container is running, and state recorded it as current
    let app = fx.running("app");
    assert_eq!(app.len(), 1, "expected exactly one app container, got {app:?}");
    let state = std::fs::read_to_string(fx.dir.join("dcd-state.json")).unwrap();
    assert!(state.contains(&app[0]));
    assert!(state.contains("\"active\""));

    // IT-006: a second deploy leaves the managed nginx untouched (same container id)
    let nginx_before = docker(&["inspect", &format!("{}-nginx", fx.project), "--format", "{{.Id}}"]);
    let out2 = fx.deploy();
    assert!(out2.status.success(), "redeploy failed: {}", String::from_utf8_lossy(&out2.stderr));
    let nginx_after = docker(&["inspect", &format!("{}-nginx", fx.project), "--format", "{{.Id}}"]);
    assert_eq!(nginx_before.stdout, nginx_after.stdout, "managed nginx was recreated");
    // old app drained, exactly one app container remains
    assert_eq!(fx.running("app").len(), 1);
}

#[test]
fn it_002_failed_healthcheck_keeps_red_and_removes_black() {
    if !enabled() {
        return;
    }
    let fx = Fixture::new("badhealth");
    // point the healthcheck at a port nothing listens on -> never passes
    let yaml = std::fs::read_to_string(fx.dir.join("dcd.yaml"))
        .unwrap()
        .replace("http://{container}/", "http://{container}:9/");
    let yaml = yaml.replace("retries: 15", "retries: 2");
    std::fs::write(fx.dir.join("dcd.yaml"), yaml).unwrap();

    let out = fx.deploy();
    assert_eq!(out.status.code(), Some(1), "expected pre-cutover exit 1");
    // black was removed; no app container left; state never advanced
    assert!(fx.running("app").is_empty());
    assert!(!fx.dir.join("dcd-state.json").exists() || !std::fs::read_to_string(fx.dir.join("dcd-state.json")).unwrap().contains("active"));
}

#[test]
fn it_007_env_baked_at_create_survives_restart_and_never_rests_in_deploy_root() {
    if !enabled() {
        return;
    }
    let fx = Fixture::new("envchain");

    // Chain lives OUTSIDE deploy_root (spec IT-007) and carries a secret value.
    let env_dir = std::env::temp_dir().join(format!("{}-env", fx.project));
    std::fs::create_dir_all(&env_dir).unwrap();
    std::fs::write(env_dir.join(".env"), "APP_SECRET=\nCHAIN_MARKER=base\n").unwrap();
    std::fs::write(env_dir.join(".env.it"), "APP_SECRET=e2e-hunter2\nCHAIN_MARKER=stage\n").unwrap();

    let out = fx.dcd(&["deploy", "it", "--env-dir", env_dir.to_str().unwrap()]);
    assert!(out.status.success(), "deploy failed: {}", String::from_utf8_lossy(&out.stderr));

    let app = fx.running("app");
    assert_eq!(app.len(), 1);

    // env baked into the container config at create
    let inspect = docker(&["inspect", &app[0], "--format", "{{.Config.Env}}"]);
    let env_line = String::from_utf8_lossy(&inspect.stdout).to_string();
    assert!(env_line.contains("APP_SECRET=e2e-hunter2"), "env not baked: {env_line}");
    assert!(env_line.contains("CHAIN_MARKER=stage"), "later layer must win: {env_line}");

    // survives a restart with no env present anywhere
    docker(&["restart", &app[0]]);
    let inspect = docker(&["exec", &app[0], "printenv", "APP_SECRET"]);
    assert_eq!(String::from_utf8_lossy(&inspect.stdout).trim(), "e2e-hunter2");

    // no file under deploy_root contains the secret value
    for entry in std::fs::read_dir(&fx.dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            let bytes = std::fs::read(&path).unwrap();
            assert!(
                !String::from_utf8_lossy(&bytes).contains("e2e-hunter2"),
                "secret at rest in {}",
                path.display()
            );
        }
    }

    let _ = std::fs::remove_dir_all(&env_dir);
}

#[test]
fn it_008_unlock_accepts_a_stuck_release_clears_the_lock_and_leaves_the_stage_deployable() {
    if !enabled() {
        return;
    }
    let fx = Fixture::new("unlock");
    let config = fx.dir.join("dcd.yaml");
    let healthy_yaml = std::fs::read_to_string(&config).unwrap();

    assert!(fx.deploy().status.success(), "first deploy failed");
    let red = fx.running("app")[0].clone();

    // A second deploy that fails AFTER the cutover: the black is live and recorded
    // cutover_pending, `migrate:after` errors -> exit 4. That is the stuck state.
    let stuck_yaml = healthy_yaml.replace("  drain: 'true'", "  drain: 'true'\n  migrate: { after: 'false' }");
    std::fs::write(&config, &stuck_yaml).unwrap();
    let failed = fx.deploy();
    assert_eq!(failed.status.code(), Some(4), "expected post-cutover exit 4");
    let stuck_state = std::fs::read_to_string(fx.dir.join("dcd-state.json")).unwrap();
    assert!(stuck_state.contains("cutover_pending"));
    assert!(fx.dcd(&["deploy", "it"]).status.code() == Some(4), "a plain deploy must refuse while stuck");
    let live_before_unlock = fx.running("app");

    // Hold the stage lock by hand: unlock overrides a live flock by design.
    let lock_path = fx.dir.join(".dcd.it.lock");
    std::fs::write(&lock_path, b"").unwrap();
    use fs2::FileExt;
    let held = std::fs::OpenOptions::new().read(true).write(true).open(&lock_path).unwrap();
    held.try_lock_exclusive().unwrap();

    let unlocked = fx.dcd(&["unlock", "it", "-y"]);
    assert!(
        unlocked.status.success(),
        "unlock failed: {}",
        String::from_utf8_lossy(&unlocked.stderr)
    );
    fs2::FileExt::unlock(&held).unwrap();

    // State-only: the black is the release of record and the lock is gone. The container
    // set is exactly what the failed deploy left — unlock started, stopped, and removed
    // nothing (that deploy had already drained the red before `migrate:after` failed).
    let app = fx.running("app");
    assert_eq!(app, live_before_unlock, "unlock must not touch containers");
    let black = app[0].clone();
    assert_ne!(black, red, "the promoted release is the black, not the old red");
    let state = std::fs::read_to_string(fx.dir.join("dcd-state.json")).unwrap();
    assert!(!state.contains("cutover_pending"), "state still incomplete: {state}");
    assert!(state.contains(&format!("\"current\": \"{black}\"")), "current not advanced: {state}");
    assert!(!lock_path.exists());
    assert!(!fx.dir.join(".dcd.it.lock.meta").exists());

    // The payoff: a plain deploy runs fresh — no --resume, no hand-editing — and it is
    // that deploy which reaps every stale container.
    std::fs::write(&config, &healthy_yaml).unwrap();
    let after = fx.deploy();
    assert!(after.status.success(), "stage not deployable after unlock: {}", String::from_utf8_lossy(&after.stderr));
    let survivors = fx.running("app");
    assert_eq!(survivors.len(), 1, "next deploy must drain the leftovers, got {survivors:?}");
    assert!(!survivors.contains(&black), "the unlocked release is drained by the next deploy");
}

#[test]
fn it_005_concurrent_lock_refuses_second() {
    if !enabled() {
        return;
    }
    let fx = Fixture::new("lock");
    // hold the lock by hand (flock on the same path the deploy uses)
    let lock_path = fx.dir.join(".dcd.it.lock");
    std::fs::write(&lock_path, b"").unwrap();
    use fs2::FileExt;
    let held = std::fs::OpenOptions::new().read(true).write(true).open(&lock_path).unwrap();
    held.try_lock_exclusive().unwrap();

    let out = fx.dcd(&["deploy", "it"]);
    assert_eq!(out.status.code(), Some(3), "expected lock-held exit 3, stderr: {}", String::from_utf8_lossy(&out.stderr));

    fs2::FileExt::unlock(&held).unwrap();
}

/// IT-017: `--image <service>=<ref>` pins that service for the run. The v2
/// migration left the flag translating to `--set docker.images.<name>` — a config
/// path v2 removed — so every CI deploy carrying it died in config load before
/// Docker was touched. Real Docker is what proves the pin reaches the container.
#[test]
fn it_017_an_image_pin_is_the_image_the_release_container_runs() {
    if !enabled() {
        return;
    }
    let fx = Fixture::new("imagepin");
    let pinned = "nginx:1.27-alpine";

    let out = fx.dcd(&["deploy", "it", "--image", &format!("app={pinned}")]);
    assert!(out.status.success(), "pinned deploy failed: {}", String::from_utf8_lossy(&out.stderr));

    let app = fx.running("app");
    assert_eq!(app.len(), 1, "expected one app container, got {app:?}");
    let inspect = docker(&["inspect", &app[0], "--format", "{{.Config.Image}}"]);
    assert_eq!(String::from_utf8_lossy(&inspect.stdout).trim(), pinned);

    // The pin is per-service and must not have leaked to the managed nginx.
    let nginx = docker(&["inspect", &format!("{}-nginx", fx.project), "--format", "{{.Config.Image}}"]);
    assert_eq!(String::from_utf8_lossy(&nginx.stdout).trim(), "nginx:alpine");

    // It is also what a rollback would replay, so it has to be in the record.
    let state = std::fs::read_to_string(fx.dir.join("dcd-state.json")).unwrap();
    assert!(state.contains(pinned), "the release record must carry the pinned image: {state}");
}

/// A pin naming a service the compose files do not declare must stop the deploy,
/// naming the services that do exist — not deploy the unpinned image.
#[test]
fn it_018_an_image_pin_for_an_unknown_service_refuses_to_deploy() {
    if !enabled() {
        return;
    }
    let fx = Fixture::new("imagetypo");

    let out = fx.dcd(&["deploy", "it", "--image", "ap=nginx:1.27-alpine"]);
    assert!(!out.status.success(), "a typo must not deploy");
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(stderr.contains("--image 'ap' is not a service"), "{stderr}");
    assert!(fx.running("app").is_empty(), "nothing may start");
}

/// IT-013: `--dry-run` touches nothing. It shipped taking the stage lock and
/// creating `deploy_root` — on the real target over ssh, where it also wrote the
/// lease and held a live flock against a concurrent deploy. README and AGENTS.md
/// both prescribe running it against production, so this is the one command whose
/// side effects must be zero.
#[test]
fn it_013_a_dry_run_takes_no_lock_and_writes_nothing() {
    if !enabled() {
        return;
    }
    let fx = Fixture::new("dryrun");
    let lock = fx.dir.join(".dcd.it.lock");
    let state = fx.dir.join("dcd-state.json");

    let out = fx.dcd(&["deploy", "it", "--dry-run"]);
    assert!(out.status.success(), "dry run failed: {}", String::from_utf8_lossy(&out.stderr));

    let plan = String::from_utf8_lossy(&out.stdout);
    assert!(plan.contains("start:black"), "the plan must still be printed: {plan}");
    assert!(!lock.exists(), "--dry-run took the stage lock");
    assert!(!fx.dir.join(".dcd.it.lock.meta").exists(), "--dry-run wrote the lock sidecar");
    assert!(!state.exists(), "--dry-run wrote state");
    assert!(fx.running("app").is_empty(), "--dry-run started a container");

    // And it stays a no-op while a real lock is held: the plan is still produced,
    // labelled stale, rather than refused with exit 3.
    std::fs::write(&lock, b"").unwrap();
    use fs2::FileExt;
    let held = std::fs::OpenOptions::new().read(true).write(true).open(&lock).unwrap();
    held.try_lock_exclusive().unwrap();

    let while_held = fx.dcd(&["deploy", "it", "--dry-run"]);
    fs2::FileExt::unlock(&held).unwrap();
    assert!(
        while_held.status.success(),
        "a dry run must not be refused by a held lock: {}",
        String::from_utf8_lossy(&while_held.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&while_held.stdout),
        String::from_utf8_lossy(&while_held.stderr)
    );
    assert!(combined.contains("may be stale"), "a held lock must be reported: {combined}");
}
