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
            r#"version: 1
project: {project}
network: {project}_net
docker:
  images:
    app: nginx:alpine
  services:
    nginx:
      container: {project}-nginx
      recreate: never
      wait: {{ exec_in: {project}-nginx, cmd: 'wget -qO- -T 2 http://localhost/ >/dev/null 2>&1 || true', retries: 10, interval: 1s }}
compose:
  files: [compose.prod.yml]
release:
  image: app
  container_prefix: {project}-app
  run: {{ network_alias: app }}
  drain: 'true'
  healthcheck: {{ exec_in: {project}-nginx, cmd: 'wget -qO- -T 2 http://{{container}}/', retries: 15, interval: 1s }}
cutover:
  upstream_file: upstream.conf
  backend_port: 80
  reload: {{ exec_in: {project}-nginx, cmd: 'true' }}
stages: {{ it: {{}} }}
"#
        );
        std::fs::write(dir.join("dcd.yaml"), dcd_yaml).unwrap();

        let compose = format!(
            r#"services:
  nginx:
    image: nginx:alpine
    container_name: {project}-nginx
networks:
  default:
    name: {project}_net
    external: true
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
