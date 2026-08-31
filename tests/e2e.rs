//! Real-Docker integration tests (spec §10.2). `#[ignore]`d so the normal `cargo
//! test` run stays daemon-free AND reports them as *ignored* — an env-var gate that
//! returned early instead reported a green tick while asserting nothing, which is
//! how this whole suite stayed broken through the v2 migration unnoticed.
//!
//!   cargo test --test e2e -- --test-threads=1 --include-ignored
//!
//! Each test provisions a scratch network + an `nginx:alpine` managed service, drives
//! the real `dcd` binary, and cleans everything up via the `Fixture` drop guard.

use std::path::PathBuf;
use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;

/// Every `docker` call here is an ASSERTION SUBSTRATE, so a failed one must not
/// read as "nothing found": a broken `docker ps` returning empty stdout satisfies
/// `assert!(running(...).is_empty())` for entirely the wrong reason.
fn docker(args: &[&str]) -> std::process::Output {
    let out = Command::new("docker").args(args).output().expect("docker available");
    assert!(
        out.status.success(),
        "docker {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// For `Drop` only: a panic here would land during unwind from a failing assertion
/// and abort the process, destroying the failure report.
fn docker_quietly(args: &[&str]) -> Option<std::process::Output> {
    Command::new("docker").args(args).output().ok()
}

/// Every file under `root`, recursively — `sync` writes into subdirectories, so a
/// flat `read_dir` scan misses exactly the places an upload could land. Returns the
/// number of files actually read alongside the hits, because a scan that read
/// nothing and a scan that found nothing are otherwise the same answer.
fn scan_for(root: &std::path::Path, needle: &str) -> (usize, Vec<String>) {
    let mut scanned = 0;
    let mut hits = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else { continue };
            scanned += 1;
            if String::from_utf8_lossy(&bytes).contains(needle) {
                hits.push(path.display().to_string());
            }
        }
    }
    (scanned, hits)
}

struct Fixture {
    project: String,
    dir: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Fixture {
        let project = format!("dcdit{}{}", std::process::id(), tag);
        let dir = std::env::temp_dir().join(&project);
        // Cleared first, like the sibling fixtures: a panicked earlier run with the
        // same pid leaves state a later one would inherit — and `it_013`'s subject
        // is the ABSENCE of files at fixed paths.
        let _ = std::fs::remove_dir_all(&dir);
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
    restart: unless-stopped
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
        self.dcd_command(args).output().unwrap()
    }

    fn dcd_command(&self, args: &[&str]) -> Command {
        let mut command = Command::cargo_bin("dcd").unwrap();
        command.current_dir(&self.dir).args(args);
        command
    }

    /// A deploy this process keeps a handle on — the lock test has to kill the
    /// holder while it is really holding the lock, not a flock the test took
    /// on its behalf.
    fn spawn_dcd(&self, args: &[&str]) -> std::process::Child {
        self.dcd_command(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("dcd starts")
    }

    /// Appends a service to the compose file, ahead of the top-level `networks:`
    /// block `new` wrote. Only the worker test needs more than the two services.
    fn add_compose_service(&self, block: &str) {
        let path = self.dir.join("compose.prod.yml");
        let compose = std::fs::read_to_string(&path).unwrap();
        // The TOP-LEVEL networks block: `app` has an indented `networks:` of its
        // own, and splitting on the first match wrote the service into it.
        let (services, networks) = compose
            .split_once("\nnetworks:")
            .expect("the fixture compose ends with a top-level networks block");
        std::fs::write(&path, format!("{services}\n{block}networks:{networks}")).unwrap();
    }

    fn append_config(&self, block: &str) {
        let path = self.dir.join("dcd.yaml");
        let mut config = std::fs::read_to_string(&path).unwrap();
        config.push_str(block);
        std::fs::write(&path, config).unwrap();
    }

    fn state(&self) -> String {
        std::fs::read_to_string(self.dir.join("dcd-state.json")).expect("state file")
    }

    fn inspect(&self, container: &str, format: &str) -> String {
        let out = docker(&["inspect", container, "--format", format]);
        String::from_utf8_lossy(&out.stdout).trim().to_string()
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
        if let Some(containers) = docker_quietly(&["ps", "-aq", "--filter", &format!("name=^{}", self.project)]) {
            for id in String::from_utf8_lossy(&containers.stdout).split_whitespace() {
                docker_quietly(&["rm", "-f", id]);
            }
        }
        docker_quietly(&["network", "rm", &format!("{}_net", self.project)]);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
#[ignore = "drives real Docker; run with `-- --include-ignored`"]
fn it_001_happy_deploy_and_006_no_recreate() {
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
#[ignore = "drives real Docker; run with `-- --include-ignored`"]
fn it_002_failed_healthcheck_keeps_red_and_removes_black() {
    let fx = Fixture::new("badhealth");

    // A RED has to exist first, or "keeps red" is not exercised at all: against a
    // fresh fixture the assertions below are equally satisfied by a black that was
    // never started, and the exit code is every pre-cutover failure, not this one.
    assert!(fx.deploy().status.success(), "the first deploy must establish a red");
    let red = fx.running("app");
    assert_eq!(red.len(), 1, "expected one red, got {red:?}");
    let red = red[0].clone();

    // Point the healthcheck at a port nothing listens on -> it never passes.
    let yaml = std::fs::read_to_string(fx.dir.join("dcd.yaml"))
        .unwrap()
        .replace("http://{container}/", "http://{container}:9/");
    let yaml = yaml.replace("retries: 15", "retries: 2");
    std::fs::write(fx.dir.join("dcd.yaml"), yaml).unwrap();

    let out = fx.deploy();
    assert_eq!(out.status.code(), Some(1), "expected pre-cutover exit 1");
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        stderr.contains("healthcheck"),
        "the failure must be the health gate, not some earlier step: {stderr}"
    );

    // INV-1: red is still serving, and it is still what state calls current.
    let survivors = fx.running("app");
    assert_eq!(survivors, vec![red.clone()], "red must survive and the black must be gone");
    let state = std::fs::read_to_string(fx.dir.join("dcd-state.json")).expect("state exists from the first deploy");
    assert!(state.contains(&format!("\"current\": \"{red}\"")), "current must still name red: {state}");
}

#[test]
#[ignore = "drives real Docker; run with `-- --include-ignored`"]
fn it_007_env_baked_at_create_survives_restart_and_never_rests_in_deploy_root() {
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

    // survives a restart with no env present anywhere. `docker()` now asserts the
    // restart actually happened, so "survives a restart" cannot pass on one that
    // never occurred.
    docker(&["restart", &app[0]]);
    let inspect = docker(&["exec", &app[0], "printenv", "APP_SECRET"]);
    assert_eq!(String::from_utf8_lossy(&inspect.stdout).trim(), "e2e-hunter2");

    // No file ANYWHERE under deploy_root contains the value — recursively, because
    // `sync` writes into subdirectories, and with a planted control so that "found
    // nothing" is distinguishable from "scanned nothing".
    std::fs::create_dir_all(fx.dir.join("nested")).unwrap();
    std::fs::write(fx.dir.join("nested/planted-control"), "e2e-hunter2\n").unwrap();
    let (scanned, hits) = scan_for(&fx.dir, "e2e-hunter2");
    assert!(
        hits.iter().any(|path| path.ends_with("planted-control")),
        "the scan cannot find a secret that IS there — it proves nothing: scanned {scanned} file(s)"
    );
    std::fs::remove_file(fx.dir.join("nested/planted-control")).unwrap();

    let (scanned, hits) = scan_for(&fx.dir, "e2e-hunter2");
    assert!(scanned > 1, "expected to scan the files dcd wrote, scanned {scanned}");
    assert!(hits.is_empty(), "secret at rest in {hits:?}");

    let _ = std::fs::remove_dir_all(&env_dir);
}

#[test]
#[ignore = "drives real Docker; run with `-- --include-ignored`"]
fn it_008_unlock_accepts_a_stuck_release_clears_the_lock_and_leaves_the_stage_deployable() {
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
    let held = std::fs::OpenOptions::new().read(true).write(true).open(&lock_path).unwrap();
    held.try_lock().unwrap();

    let unlocked = fx.dcd(&["unlock", "it", "-y"]);
    assert!(
        unlocked.status.success(),
        "unlock failed: {}",
        String::from_utf8_lossy(&unlocked.stderr)
    );
    held.unlock().unwrap();

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

/// IT-005's other half, and the half a hand-held flock cannot show: two REAL
/// deploys racing, and a holder that is killed rather than asked to let go. The
/// flock belongs to the dead process's file descriptor, so the kernel reclaims it
/// — no `unlock`, no stale-lock heuristic, exactly as the remote lease does (IT-011).
#[test]
#[ignore = "drives real Docker; run with `-- --include-ignored`"]
fn it_005b_a_real_deploy_holds_the_stage_and_a_killed_one_releases_it() {
    let fx = Fixture::new("lockrace");
    let meta = fx.dir.join(".dcd.it.lock.meta");

    let mut holder = fx.spawn_dcd(&["deploy", "it"]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !meta.exists() {
        assert!(std::time::Instant::now() < deadline, "the deploy never took the lock");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let refused = fx.dcd(&["deploy", "it"]);
    assert_eq!(
        refused.status.code(),
        Some(3),
        "a second deploy must be refused: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("another deploy holds it"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );

    holder.kill().expect("the holder can be killed");
    let _ = holder.wait();

    // The lock is free because the process died, not because anything cleaned up:
    // asked of the kernel here, and of dcd by the deploy that follows.
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fx.dir.join(".dcd.it.lock"))
        .expect("the lock file outlives its holder");
    lock.try_lock().expect("the killed holder's flock was not reclaimed");
    lock.unlock().unwrap();
    drop(lock);

    let next = fx.deploy();
    assert!(
        next.status.success(),
        "the stage is not deployable after the holder died: {}",
        String::from_utf8_lossy(&next.stderr)
    );
    assert_eq!(fx.running("app").len(), 1);
}

#[test]
#[ignore = "drives real Docker; run with `-- --include-ignored`"]
fn it_005_concurrent_lock_refuses_second() {
    let fx = Fixture::new("lock");
    // hold the lock by hand (flock on the same path the deploy uses)
    let lock_path = fx.dir.join(".dcd.it.lock");
    std::fs::write(&lock_path, b"").unwrap();
    let held = std::fs::OpenOptions::new().read(true).write(true).open(&lock_path).unwrap();
    held.try_lock().unwrap();

    let out = fx.dcd(&["deploy", "it"]);
    assert_eq!(out.status.code(), Some(3), "expected lock-held exit 3, stderr: {}", String::from_utf8_lossy(&out.stderr));

    held.unlock().unwrap();
}

/// IT-017: `--image <service>=<ref>` pins that service for the run. The v2
/// migration left the flag translating to `--set docker.images.<name>` — a config
/// path v2 removed — so every CI deploy carrying it died in config load before
/// Docker was touched. Real Docker is what proves the pin reaches the container.
#[test]
#[ignore = "drives real Docker; run with `-- --include-ignored`"]
fn it_017_an_image_pin_is_the_image_the_release_container_runs() {
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
#[ignore = "drives real Docker; run with `-- --include-ignored`"]
fn it_018_an_image_pin_for_an_unknown_service_refuses_to_deploy() {
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
#[ignore = "drives real Docker; run with `-- --include-ignored`"]
fn it_013_a_dry_run_takes_no_lock_and_writes_nothing() {
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
    let held = std::fs::OpenOptions::new().read(true).write(true).open(&lock).unwrap();
    held.try_lock().unwrap();

    let while_held = fx.dcd(&["deploy", "it", "--dry-run"]);
    held.unlock().unwrap();
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

/// IT-003: `dcd rollback` re-points the stage at the previous release — its image,
/// not the current one — and runs no migration. "No migration ran" is observed in
/// the container the rollback created, not just believed from the record: the
/// deploy's `migrate:after` leaves a marker inside the release, and the rollback's
/// release must not have it while the deploy's did.
#[test]
#[ignore = "drives real Docker; run with `-- --include-ignored`"]
fn it_003_rollback_returns_the_previous_image_and_runs_no_migration() {
    let fx = Fixture::new("rollback");
    let config = fx.dir.join("dcd.yaml");
    let yaml = std::fs::read_to_string(&config)
        .unwrap()
        .replace("  drain: 'true'", "  drain: 'true'\n  migrate: { after: 'touch /tmp/migrated' }");
    std::fs::write(&config, yaml).unwrap();

    assert!(fx.deploy().status.success(), "the first deploy must establish the rollback target");
    let v1 = fx.running("app");
    assert_eq!(v1.len(), 1, "expected one app container, got {v1:?}");
    let v1 = v1[0].clone();

    let pinned = "nginx:1.27-alpine";
    let second = fx.dcd(&["deploy", "it", "--image", &format!("app={pinned}")]);
    assert!(second.status.success(), "second deploy failed: {}", String::from_utf8_lossy(&second.stderr));
    let v2 = fx.running("app");
    assert_eq!(v2.len(), 1, "the first release must be drained: {v2:?}");
    let v2 = v2[0].clone();
    assert_ne!(v2, v1);
    assert_eq!(fx.inspect(&v2, "{{.Config.Image}}"), pinned);
    // The control for the marker: this deploy DID run the migration.
    docker(&["exec", &v2, "test", "-f", "/tmp/migrated"]);

    let rolled = fx.dcd(&["rollback", "it", "-y"]);
    assert!(rolled.status.success(), "rollback failed: {}", String::from_utf8_lossy(&rolled.stderr));

    let after = fx.running("app");
    assert_eq!(after.len(), 1, "expected one app container after rollback, got {after:?}");
    let restored = after[0].clone();
    assert_ne!(restored, v2, "the rollback must replace the rolled-back container");
    assert_eq!(
        fx.inspect(&restored, "{{.Config.Image}}"),
        "nginx:alpine",
        "the rollback must replay the previous release's image, not the current one"
    );

    // No migration: the marker `migrate:after` would have left is absent. Raw
    // `Command`, because a non-zero exit is the expected answer here.
    let marker = Command::new("docker")
        .args(["exec", &restored, "test", "-f", "/tmp/migrated"])
        .output()
        .expect("docker available");
    assert!(!marker.status.success(), "rollback ran migrate:after");

    // The router points at the restored release, and the record says rolled back.
    let upstream = std::fs::read_to_string(fx.dir.join("upstream.conf")).expect("upstream file");
    assert!(upstream.contains(&restored), "upstream still points elsewhere: {upstream}");

    let status = fx.dcd(&["status", "it"]);
    let printed = String::from_utf8_lossy(&status.stdout).to_string();
    // The release rows are the indented ones; `current = …` names a container too.
    let release_line = |container: &str| {
        printed
            .lines()
            .find(|line| line.starts_with("  ") && line.contains(container))
            .unwrap_or_else(|| panic!("no release row for {container}: {printed}"))
            .to_string()
    };
    let rolled_back_line = release_line(&v2);
    assert!(rolled_back_line.contains("RolledBack"), "status must show the rollback: {printed}");
    let restored_line = release_line(&restored);
    assert!(restored_line.contains("Active"), "the restored release must be active: {printed}");
    assert!(
        !restored_line.contains("ran migrations"),
        "the rollback release must not be recorded as having migrated: {printed}"
    );
    assert!(printed.contains(&format!("current = {restored}")), "{printed}");
}

/// IT-004: a deploy that dies after the cutover leaves the stage incomplete (exit 4);
/// `dcd deploy --resume` finishes that same release rather than starting another.
/// The container identity is the whole point — a resume that quietly created a new
/// black would pass every state-only assertion.
#[test]
#[ignore = "drives real Docker; run with `-- --include-ignored`"]
fn it_004_resume_finishes_the_incomplete_release_without_replacing_it() {
    let fx = Fixture::new("resume");
    let config = fx.dir.join("dcd.yaml");
    let healthy = std::fs::read_to_string(&config).unwrap();

    assert!(fx.deploy().status.success(), "the first deploy must establish a red");

    let stuck = healthy.replace("  drain: 'true'", "  drain: 'true'\n  migrate: { after: 'false' }");
    std::fs::write(&config, &stuck).unwrap();
    let failed = fx.deploy();
    assert_eq!(failed.status.code(), Some(4), "expected post-cutover exit 4");
    let pending = fx.running("app");
    assert_eq!(pending.len(), 1, "the black is live and the red drained, got {pending:?}");
    let pending = pending[0].clone();
    assert!(fx.state().contains("cutover_pending"), "the stage must be stuck: {}", fx.state());

    // Fix the cause, then resume — the operator flow the README prescribes.
    std::fs::write(&config, &healthy).unwrap();
    let resumed = fx.dcd(&["deploy", "--resume", "it"]);
    assert!(resumed.status.success(), "resume failed: {}", String::from_utf8_lossy(&resumed.stderr));

    let after = fx.running("app");
    assert_eq!(after, vec![pending.clone()], "resume must finish THAT release, not start another");
    let state = fx.state();
    assert!(!state.contains("cutover_pending"), "resume left the stage incomplete: {state}");
    assert!(state.contains(&format!("\"current\": \"{pending}\"")), "current not advanced: {state}");
    assert!(state.contains("\"active\""), "the resumed release must end active: {state}");

    // And the stage is ordinarily deployable again.
    let next = fx.deploy();
    assert!(next.status.success(), "stage not deployable after resume: {}", String::from_utf8_lossy(&next.stderr));
}

/// IT-014 (INV-13/14): the release container is a one-off `compose run` container,
/// and it has to survive two things that sweep by label — an operator's
/// `compose up -d --remove-orphans`, and dcd's own worker drain, which filters on
/// `com.docker.compose.service={workers.service}`. If either ever matched the
/// release, a deploy would stop the container it just cut over to.
#[test]
#[ignore = "drives real Docker; run with `-- --include-ignored`"]
fn it_014_the_release_container_survives_compose_reconciliation_and_worker_drain() {
    let fx = Fixture::new("workers");
    fx.add_compose_service(
        "  worker:\n    image: nginx:alpine\n    entrypoint: [\"sh\", \"-c\", \"sleep 3600\"]\n",
    );
    fx.append_config(&format!(
        "workers:\n  service: worker\n  provider: {{ static: [async] }}\n  name_prefix: {}-worker-\n",
        fx.project
    ));

    let first = fx.deploy();
    assert!(
        first.status.success(),
        "deploy with workers failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let release = fx.running("app");
    assert_eq!(release.len(), 1, "expected one app container, got {release:?}");
    let release = release[0].clone();
    let worker = format!("{}-worker-async", fx.project);
    assert_eq!(fx.running("worker"), vec![worker.clone()], "the worker must exist to be drained");

    let id_before = fx.inspect(&release, "{{.Id}}");
    let started_before = fx.inspect(&release, "{{.State.StartedAt}}");
    let worker_id_before = fx.inspect(&worker, "{{.Id}}");

    // An operator reconciling the stack by hand. `--remove-orphans` is the sweep
    // that would take the release container with it if compose counted it as one.
    let reconcile = Command::new("docker")
        .current_dir(&fx.dir)
        .args(["compose", "-p", &fx.project, "-f", "compose.prod.yml", "up", "-d", "--remove-orphans"])
        .output()
        .expect("docker compose available");
    assert!(
        reconcile.status.success(),
        "compose up failed: {}",
        String::from_utf8_lossy(&reconcile.stderr)
    );
    assert_eq!(fx.running("app"), vec![release.clone()], "compose up --remove-orphans took the release");
    assert_eq!(fx.inspect(&release, "{{.Id}}"), id_before, "the release container was recreated");
    assert_eq!(
        fx.inspect(&release, "{{.State.StartedAt}}"),
        started_before,
        "the release container was restarted"
    );

    // Now dcd's own worker drain, which runs in `drain:red` — after the cutover,
    // while the new release container is live and carrying the project label.
    let second = fx.deploy();
    assert!(second.status.success(), "second deploy failed: {}", String::from_utf8_lossy(&second.stderr));
    let live = fx.running("app");
    assert_eq!(live.len(), 1, "the release must have survived the worker drain, got {live:?}");
    assert_ne!(live[0], release, "the second deploy creates its own release container");
    assert_eq!(fx.inspect(&live[0], "{{.State.Running}}"), "true");

    // The control: the drain really ran, so "the release survived" is not the
    // answer to a drain that never happened.
    assert_eq!(fx.running("worker"), vec![worker.clone()], "the worker must be back");
    assert_ne!(
        fx.inspect(&worker, "{{.Id}}"),
        worker_id_before,
        "the worker was never drained and recreated — the discovery path went unexercised"
    );
}

/// IT-015 (§7.6): `compose run` forces `restart=no` on a one-off container, so dcd
/// re-applies the policy the operator declared with `docker update`. Without it the
/// release simply does not come back after a host reboot — silent until the reboot.
#[test]
#[ignore = "drives real Docker; run with `-- --include-ignored`"]
fn it_015_the_declared_restart_policy_is_applied_to_the_release() {
    let fx = Fixture::new("restart");
    assert!(fx.deploy().status.success());

    let app = fx.running("app");
    assert_eq!(app.len(), 1, "expected one app container, got {app:?}");

    let policy = docker(&["inspect", &app[0], "--format", "{{.HostConfig.RestartPolicy.Name}}"]);
    assert_eq!(
        String::from_utf8_lossy(&policy.stdout).trim(),
        "unless-stopped",
        "compose run creates with restart=no; dcd must re-apply the compose service's policy"
    );
}
