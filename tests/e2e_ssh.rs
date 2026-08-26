//! A full red-black deploy driven over ssh (spec §2.7, IT-016).
//!
//! `tests/ssh_shells.rs` proves the transport; this proves the deploy that rides
//! it. dcd runs here, on the test machine, and reaches a target that has sshd and
//! the docker CLI and nothing else — no dcd binary, no config, no state. The
//! target's docker CLI talks to the host daemon through a mounted socket, so the
//! containers a remote deploy creates are observable from this process.
//!
//!   DCD_E2E=1 cargo test --test e2e_ssh -- --test-threads=1

use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;

const IMAGE: &str = "dcd-ssh-deploy";
const CONTAINER: &str = "dcd-ssh-deploy";
const PORT: u16 = 22324;
const DEPLOY_ROOT: &str = "/srv/dcd";
const SECRET: &str = "ssh-e2e-hunter2";

/// The fixture drives the host daemon through a bind-mounted socket, so a
/// TCP-only `DOCKER_HOST` (a dind CI service) cannot run it. Saying so out loud
/// matters: a test that returns early reports GREEN, and this suite is the only
/// proof the remote deploy path works at all.
fn enabled() -> bool {
    if std::env::var("DCD_E2E").is_err() {
        return false;
    }
    if !Path::new("/var/run/docker.sock").exists() {
        panic!(
            "DCD_E2E=1 but /var/run/docker.sock is absent — this suite needs a socket-mountable \
             daemon and cannot run against a TCP-only DOCKER_HOST. Unset DCD_E2E to skip it."
        );
    }
    true
}

fn docker(args: &[&str]) -> std::process::Output {
    Command::new("docker").args(args).output().expect("docker available")
}

/// For `Drop` only: a panic here would land during unwind from a failing
/// assertion and abort the process, losing the failure it was reporting.
fn docker_quietly(args: &[&str]) -> Option<std::process::Output> {
    Command::new("docker").args(args).output().ok()
}

/// dcd resolves `ssh` through PATH and adds its own options, so shadowing the
/// binary is the only way to hand a test's key and host-key policy to it. The
/// shim also pins `BatchMode` — dcd already sets it, but the shim is what
/// guarantees no ssh started under this test can ever reach for a password
/// prompt on the developer's terminal.
fn write_ssh_shim(bin: &Path, config: &Path) {
    let real = Command::new("sh")
        .args(["-c", "command -v ssh"])
        .output()
        .expect("ssh on PATH");
    let real = String::from_utf8_lossy(&real.stdout).trim().to_string();
    assert!(!real.is_empty(), "ssh must be installed to run this test");

    let shim = bin.join("ssh");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\nexec {real} -F {} -o BatchMode=yes -o PreferredAuthentications=publickey \"$@\"\n",
            config.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// The suite is about shell quoting; its own helper must not be the thing that
/// breaks on a metacharacter.
fn shell_quote(raw: &str) -> String {
    format!("'{}'", raw.replace('\'', "'\\''"))
}

fn run(program: &str, args: &[&str]) {
    let out = Command::new(program).args(args).output().expect("command runs");
    assert!(
        out.status.success(),
        "{program} {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

struct Fixture {
    project: String,
    /// The checkout dcd is invoked from — compose files live here and are uploaded.
    workdir: PathBuf,
    /// Holds the `ssh` shim. dcd builds its own ssh argv and resolves the binary
    /// through PATH, so a shim is how the fixture key and host-key policy reach it
    /// without a dcd flag for it and without touching the developer's ~/.ssh.
    bin: PathBuf,
}

impl Fixture {
    fn start() -> Fixture {
        let project = format!("dcdssh{}", std::process::id());
        let base = std::env::temp_dir().join(&project);
        let _ = std::fs::remove_dir_all(&base);
        let bin = base.join("bin");
        let workdir = base.join("checkout");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&workdir).unwrap();

        let key = base.join("id_ed25519");
        run("ssh-keygen", &["-t", "ed25519", "-N", "", "-q", "-f", key.to_str().unwrap()]);
        let config = base.join("ssh_config");
        std::fs::write(
            &config,
            format!(
                "Host 127.0.0.1\n  IdentityFile {}\n  IdentitiesOnly yes\n  StrictHostKeyChecking no\n  UserKnownHostsFile /dev/null\n",
                key.display()
            ),
        )
        .unwrap();
        write_ssh_shim(&bin, &config);

        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ssh-deploy");
        let context = base.join("context");
        std::fs::create_dir_all(&context).unwrap();
        std::fs::copy(source.join("Dockerfile"), context.join("Dockerfile")).unwrap();
        std::fs::copy(base.join("id_ed25519.pub"), context.join("authorized_key")).unwrap();
        run("docker", &["build", "-q", "-t", IMAGE, context.to_str().unwrap()]);

        let _ = docker(&["rm", "-f", CONTAINER]);
        run(
            "docker",
            &[
                "run",
                "-d",
                "--name",
                CONTAINER,
                "-p",
                &format!("{PORT}:22"),
                "-v",
                "/var/run/docker.sock:/var/run/docker.sock",
                IMAGE,
            ],
        );

        let fixture = Fixture { project, workdir, bin };
        fixture.write_checkout();
        fixture.wait_for_sshd();
        fixture
    }

    /// Everything dcd needs lives in the checkout; the target gets the compose
    /// documents uploaded to it and nothing else.
    fn write_checkout(&self) {
        let project = &self.project;
        std::fs::write(
            self.workdir.join("dcd.yaml"),
            format!(
                r#"version: 2
project: {project}
ssh: root@127.0.0.1:{PORT}
deploy_root: {DEPLOY_ROOT}
compose:
  files: [compose.prod.yml]
release:
  service: app
  container_prefix: {project}-app
  drain: 'true'
  healthcheck: {{ exec_in: nginx, cmd: 'wget -qO- -T 2 http://{{container}}/', retries: 20, interval: 1s }}
cutover:
  service: nginx
  upstream_file: upstream.conf
  backend_port: 80
  reload: {{ exec_in: nginx, cmd: 'true' }}
services:
  nginx:
    recreate: never
    wait: {{ exec_in: nginx, cmd: 'wget -qO- -T 2 http://localhost/ >/dev/null 2>&1 || true', retries: 20, interval: 1s }}
stages: {{ prod: {{}} }}
"#
            ),
        )
        .unwrap();

        std::fs::write(
            self.workdir.join("compose.prod.yml"),
            format!(
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
            ),
        )
        .unwrap();

        // The chain sits in the checkout, never on the target: the point of the
        // last assertion is that its values reach the container without ever
        // being written under deploy_root (INV-12).
        std::fs::write(self.workdir.join(".env"), "APP_SECRET=\n").unwrap();
        std::fs::write(self.workdir.join(".env.prod"), format!("APP_SECRET={SECRET}\n")).unwrap();
    }

    /// The one ssh call the test makes on its own behalf. It goes through the same
    /// shim dcd will use, so a reachable target here means a reachable target there.
    fn wait_for_sshd(&self) {
        for _ in 0..60 {
            let out = self
                .shimmed("ssh")
                .args(["-p", &PORT.to_string(), "root@127.0.0.1", "--", "true"])
                .output()
                .expect("ssh available");
            if out.status.success() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        panic!("sshd never became reachable");
    }

    /// A command that finds the shimmed `ssh` first and can never read the
    /// terminal — an unreachable target must fail the test, never prompt whoever
    /// is running it.
    fn shimmed(&self, program: &str) -> Command {
        let path = std::env::var("PATH").unwrap_or_default();
        let mut command = Command::new(program);
        command
            .env("PATH", format!("{}:{path}", self.bin.display()))
            .stdin(std::process::Stdio::null());
        command
    }

    fn dcd(&self, args: &[&str]) -> std::process::Output {
        let path = std::env::var("PATH").unwrap_or_default();
        Command::cargo_bin("dcd")
            .unwrap()
            .current_dir(&self.workdir)
            .env("PATH", format!("{}:{path}", self.bin.display()))
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    }

    fn deploy(&self) -> std::process::Output {
        self.dcd(&["deploy", "prod"])
    }

    /// Containers on the HOST daemon — the ones the remote docker CLI created.
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

    fn on_target(&self, script: &str) -> String {
        let out = docker(&["exec", CONTAINER, "sh", "-c", script]);
        assert!(
            out.status.code().is_some(),
            "docker exec did not run on the target: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    /// `grep -rl` exits 1 when it finds nothing and 2 when it could not look. Only
    /// the first is a clean result — without the distinction the leak assertion
    /// passes identically when the path is absent or the exec failed.
    fn grep_on_target(&self, needle: &str, path: &str) -> Vec<String> {
        let script = format!("grep -rl -- {} {}", shell_quote(needle), shell_quote(path));
        let out = docker(&["exec", CONTAINER, "sh", "-c", &script]);
        let code = out.status.code();
        assert!(
            matches!(code, Some(0) | Some(1)),
            "grep could not search {path} (exit {code:?}): {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect()
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
        docker_quietly(&["rm", "-f", CONTAINER]);
        let _ = std::fs::remove_dir_all(self.workdir.parent().unwrap_or(Path::new("/nonexistent")));
    }
}

/// IT-009 + IT-016: two deploys over ssh against a target that has never seen dcd — the
/// compose files travel, the release cuts over, the old one drains, and the
/// secret that reached the container is nowhere under deploy_root.
#[test]
fn it_009_and_it_016_a_full_red_black_deploy_runs_over_ssh() {
    if !enabled() {
        return;
    }
    let fx = Fixture::start();

    let first = fx.deploy();
    assert!(first.status.success(), "first deploy failed: {}", String::from_utf8_lossy(&first.stderr));

    let red = fx.running("app");
    assert_eq!(red.len(), 1, "expected one app container, got {red:?}");
    let red = red[0].clone();

    // dcd uploaded the compose document and wrote its own files on the target.
    let listing = fx.on_target(&format!("ls {DEPLOY_ROOT}"));
    for expected in ["compose.prod.yml", "dcd-state.json", "upstream.conf"] {
        assert!(listing.contains(expected), "{expected} missing from the target: {listing}");
    }
    let state = fx.on_target(&format!("cat {DEPLOY_ROOT}/dcd-state.json"));
    assert!(state.contains(&red), "state on the target does not record the release: {state}");

    // The env chain never left the checkout, but its value is in the container.
    let printenv = docker(&["exec", &red, "printenv", "APP_SECRET"]);
    assert_eq!(String::from_utf8_lossy(&printenv.stdout).trim(), SECRET);

    // A second deploy cuts over to a new container and drains the old one.
    let second = fx.deploy();
    assert!(second.status.success(), "second deploy failed: {}", String::from_utf8_lossy(&second.stderr));
    let black = fx.running("app");
    assert_eq!(black.len(), 1, "the old release must be drained, got {black:?}");
    assert_ne!(black[0], red, "the second deploy must create a new container");

    // INV-12: nothing dcd wrote on the target carries the value. The positive
    // control is what proves the search works — otherwise "found nothing" and
    // "never looked" are the same result.
    let planted = format!("{DEPLOY_ROOT}/planted-control");
    fx.on_target(&format!("printf %s {} > {planted}", shell_quote(SECRET)));
    let control = fx.grep_on_target(SECRET, DEPLOY_ROOT);
    assert!(
        control.iter().any(|hit| hit.contains("planted-control")),
        "the search cannot find a secret that IS there: {control:?}"
    );
    fx.on_target(&format!("rm -f {planted}"));

    let leak = fx.grep_on_target(SECRET, DEPLOY_ROOT);
    assert!(leak.is_empty(), "secret at rest on the target: {leak:?}");
}
