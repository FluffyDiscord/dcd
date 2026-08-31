//! A full red-black deploy driven over ssh (spec §2.7, IT-009…IT-016).
//!
//! `tests/ssh_shells.rs` proves the transport; this proves the deploy that rides
//! it. dcd runs here, on the test machine, and reaches a target that has sshd and
//! the docker CLI and nothing else — no dcd binary, no config, no state. The
//! target's docker CLI talks to the host daemon through a mounted socket, so the
//! containers a remote deploy creates are observable from this process.
//!
//!   cargo test --test e2e_ssh -- --test-threads=1 --include-ignored
//!
//! Needs a socket-mountable daemon: the fixture bind-mounts `/var/run/docker.sock`,
//! so a TCP-only `DOCKER_HOST` (a dind CI service) cannot run it.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use assert_cmd::cargo::CommandCargoExt;

/// Per test, like `ssh_shells`: several tests in one binary sharing one container
/// name, port and image tear each other's fixtures down — and two of these kill
/// sshd on purpose, which would take every other test's target with it.
const IMAGE_PREFIX: &str = "dcd-ssh-deploy";
/// Deliberately clear of `ssh_shells`' block (22322 + one per shell test): the two
/// suites can run concurrently — `cargo test` runs one binary per test target — and
/// a shared port makes whichever starts second fail on bind.
const BASE_PORT: u16 = 22400;
const DEPLOY_ROOT: &str = "/srv/dcd";
const SECRET: &str = "ssh-e2e-hunter2";

/// The fixture drives the host daemon through a bind-mounted socket, so a
/// TCP-only `DOCKER_HOST` (a dind CI service) cannot run it. Failing loudly beats
/// returning early: this suite is the only proof the remote deploy path works, and
/// a test that returns early reports GREEN.
fn require_socket() {
    assert!(
        Path::new("/var/run/docker.sock").exists(),
        "this suite needs a socket-mountable daemon and cannot run against a TCP-only DOCKER_HOST"
    );
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

/// Polls `condition` until it holds, and fails naming what never happened —
/// a test that hangs instead reports nothing at all.
fn wait_until(what: &str, limit: Duration, mut condition: impl FnMut() -> bool) -> Duration {
    let started = Instant::now();
    while started.elapsed() < limit {
        if condition() {
            return started.elapsed();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("{what} did not happen within {limit:?}");
}

struct Fixture {
    project: String,
    /// The checkout dcd is invoked from — compose files live here and are uploaded.
    workdir: PathBuf,
    /// Holds the `ssh` shim. dcd builds its own ssh argv and resolves the binary
    /// through PATH, so a shim is how the fixture key and host-key policy reach it
    /// without a dcd flag for it and without touching the developer's ~/.ssh.
    bin: PathBuf,
    base: PathBuf,
    container: String,
    image: String,
    port: u16,
}

impl Fixture {
    fn start(tag: &str, port_offset: u16) -> Fixture {
        let container = format!("{IMAGE_PREFIX}-{}-{tag}", std::process::id());
        let project = format!("dcdssh{}{tag}", std::process::id());
        let base = std::env::temp_dir().join(&container);
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
        let image = container.clone();
        run("docker", &["build", "-q", "-t", &image, context.to_str().unwrap()]);

        let _ = docker(&["rm", "-f", &container]);
        let port = BASE_PORT + port_offset;
        run(
            "docker",
            &[
                "run",
                "-d",
                "--name",
                &container,
                "-p",
                &format!("{port}:22"),
                "-v",
                "/var/run/docker.sock:/var/run/docker.sock",
                &image,
            ],
        );

        let fixture = Fixture { project, workdir, bin, base, container, image, port };
        fixture.write_checkout();
        fixture.wait_for_sshd();
        fixture
    }

    /// Everything dcd needs lives in the checkout; the target gets the compose
    /// documents uploaded to it and nothing else. Hooks are appended per test —
    /// two of these tests hook a step in order to sever the link there.
    fn write_checkout(&self) {
        let project = &self.project;
        let port = self.port;
        std::fs::write(
            self.workdir.join("dcd.yaml"),
            format!(
                r#"version: 2
project: {project}
ssh: root@127.0.0.1:{port}
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

    fn append_config(&self, block: &str) {
        let path = self.workdir.join("dcd.yaml");
        let mut config = std::fs::read_to_string(&path).unwrap();
        config.push_str(block);
        std::fs::write(&path, config).unwrap();
    }

    /// The one ssh call the test makes on its own behalf. It goes through the same
    /// shim dcd will use, so a reachable target here means a reachable target there.
    fn wait_for_sshd(&self) {
        for _ in 0..60 {
            let out = self
                .shimmed("ssh")
                .args(["-p", &self.port.to_string(), "root@127.0.0.1", "--", "true"])
                .output()
                .expect("ssh available");
            if out.status.success() {
                return;
            }
            std::thread::sleep(Duration::from_millis(250));
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

    fn dcd_command(&self, args: &[&str]) -> Command {
        let path = std::env::var("PATH").unwrap_or_default();
        let mut command = Command::cargo_bin("dcd").unwrap();
        command
            .current_dir(&self.workdir)
            .env("PATH", format!("{}:{path}", self.bin.display()))
            .args(args)
            .stdin(std::process::Stdio::null());
        command
    }

    fn dcd(&self, args: &[&str]) -> std::process::Output {
        self.dcd_command(args).output().unwrap()
    }

    /// A deploy this process keeps a handle on — the lock tests have to kill or
    /// freeze the holder while it runs.
    fn spawn_dcd(&self, args: &[&str]) -> Child {
        self.dcd_command(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("dcd starts")
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

    /// Runs on the target through `docker exec`, NOT ssh — the transport tests
    /// kill sshd, and the target still has to be answerable afterwards.
    fn on_target(&self, script: &str) -> String {
        let out = docker(&["exec", &self.container, "sh", "-c", script]);
        assert!(
            out.status.code().is_some(),
            "docker exec did not run on the target: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    fn exists_on_target(&self, path: &str) -> bool {
        self.on_target(&format!("test -e {} && echo yes", shell_quote(path))).contains("yes")
    }

    /// Whether the stage lock on the TARGET is free right now, asked of the
    /// kernel that holds it rather than of dcd.
    fn stage_lock_is_free(&self) -> bool {
        let lock = format!("{DEPLOY_ROOT}/.dcd.prod.lock");
        let script = format!("flock -n {} -c true; echo $?", shell_quote(&lock));
        self.on_target(&script).trim() == "0"
    }

    /// `grep -rl` exits 1 when it finds nothing and 2 when it could not look. Only
    /// the first is a clean result — without the distinction the leak assertion
    /// passes identically when the path is absent or the exec failed.
    fn grep_on_target(&self, needle: &str, path: &str) -> Vec<String> {
        let script = format!("grep -rl -- {} {}", shell_quote(needle), shell_quote(path));
        let out = docker(&["exec", &self.container, "sh", "-c", &script]);
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
        docker_quietly(&["rm", "-f", &self.container]);
        docker_quietly(&["image", "rm", "-f", &self.image]);
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// IT-009 + IT-016 + IT-010: two deploys over ssh against a target that has never seen
/// dcd — the compose files travel, the release cuts over, the old one drains, and the
/// secret that reached the container is nowhere under deploy_root nor in the
/// target's process list.
#[test]
#[ignore = "drives real Docker over a real sshd; run with `-- --include-ignored`"]
fn it_009_and_it_016_a_full_red_black_deploy_runs_over_ssh() {
    require_socket();
    let fx = Fixture::start("deploy", 0);
    // IT-010: snapshot the TARGET's process list from inside the deploy, while
    // dcd's own commands are running there. Written by dcd, over the same transport.
    fx.append_config("hooks:\n  after_healthcheck:\n    - 'ps -eo args > ps-snapshot.txt'\n");

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

    // IT-010, INV-12's real proof: the value must not appear in the TARGET's process
    // list either. The snapshot was taken by an `after_healthcheck` hook, i.e. while
    // dcd's own commands were running there — the moment a `-e KEY=VALUE` argv or an
    // `ssh -o SetEnv=` would be visible to any user on the box.
    let snapshot = fx.on_target(&format!("cat {DEPLOY_ROOT}/ps-snapshot.txt"));
    assert!(
        snapshot.contains("sshd") || snapshot.contains("ps -eo args"),
        "the snapshot captured no process list at all: {snapshot}"
    );
    assert!(
        !snapshot.contains(SECRET),
        "a value reached the target's process list: {snapshot}"
    );
    // And the delivery still worked — otherwise "no secret in ps" is trivially true.
    let printenv = docker(&["exec", &black[0], "printenv", "APP_SECRET"]);
    assert_eq!(String::from_utf8_lossy(&printenv.stdout).trim(), SECRET);
}

/// IT-011 (INV-4): the remote lock is a lease, not a file — nothing on the target
/// judges whether the holder is alive, so a SIGKILLed dcd cannot leave the stage
/// locked. The ssh channel closes with the process, the lease loop reads EOF and
/// exits, and the kernel drops the `flock` the loop was holding. **No `unlock`.**
#[test]
#[ignore = "drives real Docker over a real sshd; run with `-- --include-ignored`"]
fn it_011_a_killed_deploy_leaves_no_remote_lock_behind() {
    require_socket();
    let fx = Fixture::start("kill", 1);

    let mut held = fx.spawn_dcd(&["deploy", "prod"]);
    wait_until("the remote lock was taken", Duration::from_secs(30), || {
        fx.exists_on_target(&format!("{DEPLOY_ROOT}/.dcd.prod.lock.meta"))
    });
    assert!(!fx.stage_lock_is_free(), "the lock must be held while the deploy runs");

    held.kill().expect("the deploy can be killed");
    let _ = held.wait();

    // The channel dies with the process, so this is fast — seconds of slack, not
    // the 30 s lease, which is the fallback for a holder that stops heartbeating.
    let took = wait_until("the remote lock was released", Duration::from_secs(10), || {
        fx.stage_lock_is_free()
    });
    assert!(took < Duration::from_secs(10));

    // The proof that matters to an operator: the next deploy just runs.
    let next = fx.deploy();
    assert_ne!(next.status.code(), Some(3), "the stage is still locked: {}", String::from_utf8_lossy(&next.stderr));
    assert!(next.status.success(), "the next deploy failed: {}", String::from_utf8_lossy(&next.stderr));
    assert_eq!(fx.running("app").len(), 1);
}

/// IT-011, the other half: a holder that is still connected but has stopped
/// heartbeating. SIGSTOP freezes dcd — its ssh child stays up and the channel
/// stays open, so only the heartbeat is gone. The lease is what must expire.
/// Slow by construction: `lease_seconds()` is 30.
#[test]
#[ignore = "drives real Docker over a real sshd and waits out a 30 s lease; run with `-- --include-ignored`"]
fn it_011_a_frozen_holder_loses_the_lock_when_its_lease_runs_out() {
    require_socket();
    let fx = Fixture::start("lease", 2);

    let mut held = fx.spawn_dcd(&["deploy", "prod"]);
    wait_until("the remote lock was taken", Duration::from_secs(30), || {
        fx.exists_on_target(&format!("{DEPLOY_ROOT}/.dcd.prod.lock.meta"))
    });

    let pid = held.id().to_string();
    run("kill", &["-STOP", &pid]);
    assert!(!fx.stage_lock_is_free(), "freezing the holder must not release the lock by itself");

    let took = wait_until("the lease expired", Duration::from_secs(60), || fx.stage_lock_is_free());
    assert!(
        took >= Duration::from_secs(20),
        "released after {took:?} — that is the channel closing, not the lease expiring"
    );

    let _ = Command::new("kill").args(["-CONT", &pid]).output();
    let _ = held.kill();
    let _ = held.wait();
}

/// IT-011b (INV-4): while the holder heartbeats, the stage is exclusive, and the
/// deploy that loses says who has it — read from the `.meta` sidecar on the target.
#[test]
#[ignore = "drives real Docker over a real sshd; run with `-- --include-ignored`"]
fn it_011b_a_second_deploy_is_refused_while_the_first_holds_the_stage() {
    require_socket();
    let fx = Fixture::start("excl", 3);

    let first = fx.spawn_dcd(&["deploy", "prod"]);
    wait_until("the remote lock was taken", Duration::from_secs(30), || {
        fx.exists_on_target(&format!("{DEPLOY_ROOT}/.dcd.prod.lock.meta"))
    });

    let refused = fx.deploy();
    assert_eq!(
        refused.status.code(),
        Some(3),
        "expected lock-held exit 3: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    let stderr = String::from_utf8_lossy(&refused.stderr).to_string();
    assert!(stderr.contains("another deploy holds prod"), "{stderr}");
    // The holder line is the sidecar's, not a guess: host, pid and start time.
    let holder = fx.on_target(&format!("cat {DEPLOY_ROOT}/.dcd.prod.lock.meta"));
    let holder = holder.trim();
    assert!(!holder.is_empty(), "the sidecar must name the holder");
    assert!(stderr.contains(holder), "the refusal must quote the sidecar ({holder}): {stderr}");
    assert!(holder.contains(&format!("pid {}", first.id())), "the sidecar names another process: {holder}");

    let out = first.wait_with_output().expect("the first deploy finishes");
    assert!(out.status.success(), "the holder failed: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(fx.running("app").len(), 1, "the deploy that held the lock is the one that ran");
}

/// IT-012a: the link dies BEFORE the cutover. Exit 6 (transport), and the red is
/// still serving — dcd must not have half-cut-over, and must not report the loss
/// as an application failure.
#[test]
#[ignore = "drives real Docker over a real sshd; run with `-- --include-ignored`"]
fn it_012a_transport_loss_before_the_cutover_exits_6_and_leaves_red_serving() {
    require_socket();
    let fx = Fixture::start("lost", 4);

    assert!(fx.deploy().status.success(), "the first deploy must establish a red");
    let red = fx.running("app");
    assert_eq!(red.len(), 1, "expected one app container, got {red:?}");
    let red = red[0].clone();

    // The hook runs on the target, over the transport it is about to sever.
    fx.append_config("hooks:\n  before_pull:\n    - 'pkill sshd'\n");
    let lost = fx.deploy();
    let stderr = String::from_utf8_lossy(&lost.stderr).to_string();
    assert_eq!(lost.status.code(), Some(6), "expected transport exit 6: {stderr}");
    assert!(
        stderr.contains("connection to") && stderr.contains("lost"),
        "the failure must name the lost connection: {stderr}"
    );

    // INV-1: the red is untouched and still the release of record.
    assert_eq!(fx.running("app"), vec![red.clone()], "red must still be serving");
    let state = fx.on_target(&format!("cat {DEPLOY_ROOT}/dcd-state.json"));
    assert!(state.contains(&format!("\"current\": \"{red}\"")), "current must still name red: {state}");
    assert!(!state.contains("cutover_pending"), "nothing may be pending before the cutover: {state}");
}

/// IT-012b: the same loss AFTER the cutover. The phase wins over the transport —
/// exit 4, not 6 — because the black is live and the operator's next move is
/// `--resume`/`rollback`/`unlock`, not "check the network".
#[test]
#[ignore = "drives real Docker over a real sshd; run with `-- --include-ignored`"]
fn it_012b_transport_loss_after_the_cutover_exits_4_with_the_black_live() {
    require_socket();
    let fx = Fixture::start("lostpost", 5);

    assert!(fx.deploy().status.success(), "the first deploy must establish a red");
    let red = fx.running("app")[0].clone();

    fx.append_config("hooks:\n  before_migrate_after:\n    - 'pkill sshd'\n");
    let lost = fx.deploy();
    let stderr = String::from_utf8_lossy(&lost.stderr).to_string();
    assert_eq!(lost.status.code(), Some(4), "expected post-cutover exit 4: {stderr}");

    let live = fx.running("app");
    assert_eq!(live.len(), 1, "the black must be live, got {live:?}");
    assert_ne!(live[0], red, "the cutover had already happened");

    // The record written before the link dropped is the one `--resume` reads.
    let state = fx.on_target(&format!("cat {DEPLOY_ROOT}/dcd-state.json"));
    assert!(state.contains("cutover_pending"), "the incomplete release must be recorded: {state}");
    assert!(state.contains(&live[0]), "the pending release must be the live black: {state}");
}
