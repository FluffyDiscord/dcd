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
  env_file: compose.env
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
