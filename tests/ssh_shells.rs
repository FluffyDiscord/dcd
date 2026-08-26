//! The transport, against real login shells (spec §2.7).
//!
//! sshd hands the ssh command line to the **user's login shell** before the
//! command dcd asked for ever runs. dcd does not choose that shell, and fish and
//! tcsh do not share POSIX quoting rules — so the only honest way to know the
//! transport survives them is to have a real sshd run it under each one.
//!
//! Gated on `DCD_E2E=1`, like the Docker suite: it builds one sshd image with a
//! user per shell and drives `SshRunner` against each.
//!
//!   DCD_E2E=1 cargo test --test ssh_shells -- --test-threads=1

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use dcd::effects::{Access, Argv, CommandRunner, RunOpts, SshRunner};
use dcd::ssh::SshTarget;

const IMAGE: &str = "dcd-ssh-shells";
const CONTAINER: &str = "dcd-ssh-shells";
const PORT: u16 = 22322;

/// Each user's login shell. `sh` here is busybox, which is what most container
/// images actually give you.
const SHELLS: &[(&str, &str)] = &[
    ("shsh", "busybox sh"),
    ("shdash", "dash"),
    ("shbash", "bash"),
    ("shzsh", "zsh"),
    ("shmksh", "mksh"),
    ("shloksh", "loksh"),
    ("shyash", "yash"),
    ("shfish", "fish"),
    ("shtcsh", "tcsh"),
];

/// Payloads that a naive implementation mangles: command substitution, quote
/// characters, and the metacharacters each shell family treats differently.
fn hostile_payloads() -> Vec<&'static str> {
    vec![
        "plain",
        "with space",
        "$HOME",
        "$(id)",
        "`id`",
        "a'b",
        "'; rm -rf /; '",
        "double\"quote",
        "semi;colon && echo pwned",
        "pipe | echo pwned",
        "back\\slash",
        "glob * ? [a-z] ~",
        "!bang",
        "#hash",
        "{brace}",
        "héllo-ünïcode",
        "--flag",
        "trailing   ",
    ]
}

fn enabled() -> bool {
    std::env::var("DCD_E2E").is_ok()
}

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn start() -> Fixture {
        let dir = std::env::temp_dir().join(format!("dcd-ssh-shells-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");

        let key = dir.join("id");
        run("ssh-keygen", &["-t", "ed25519", "-N", "", "-q", "-f", key.to_str().unwrap()]);

        let context = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ssh-shells");
        std::fs::copy(dir.join("id.pub"), context.join("authorized_key")).expect("stage the key");
        run("docker", &["build", "-q", "-t", IMAGE, context.to_str().unwrap()]);
        let _ = std::fs::remove_file(context.join("authorized_key"));

        let _ = Command::new("docker").args(["rm", "-f", CONTAINER]).output();
        run(
            "docker",
            &["run", "-d", "--name", CONTAINER, "-p", &format!("{PORT}:22"), IMAGE],
        );
        Fixture { dir }
    }

    /// dcd builds its own ssh argv, so the fixture key and host-key policy are
    /// supplied through a config file the test points ssh at.
    fn target_for(&self, user: &str) -> SshTarget {
        let config = self.dir.join("ssh_config");
        if !config.exists() {
            std::fs::write(
                &config,
                format!(
                    "Host 127.0.0.1\n  IdentityFile {}\n  IdentitiesOnly yes\n  StrictHostKeyChecking no\n  UserKnownHostsFile /dev/null\n  BatchMode yes\n  PreferredAuthentications publickey\n",
                    self.dir.join("id").display()
                ),
            )
            .expect("ssh config");
        }
        SshTarget::new(format!("{user}@127.0.0.1:{PORT}"), self.dir.join("cm-%C")).with_config_file(config)
    }

    fn wait_for_sshd(&self) {
        for _ in 0..50 {
            let runner = SshRunner::new(self.target_for("shsh"), BTreeMap::new(), PathBuf::from("/"));
            if runner
                .run(&Argv::of(["true"]), Access::Read, &RunOpts::default())
                .is_ok()
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        panic!("sshd never became reachable");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", CONTAINER]).output();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn run(program: &str, args: &[&str]) {
    let out = Command::new(program).args(args).output().expect("command runs");
    assert!(
        out.status.success(),
        "{program} {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Every payload must reach the command byte-identically, whatever login shell
/// the deploy user happens to have.
#[test]
fn every_login_shell_delivers_arguments_byte_identically() {
    if !enabled() {
        return;
    }
    let fixture = Fixture::start();
    fixture.wait_for_sshd();

    for (user, shell) in SHELLS {
        let runner = SshRunner::new(fixture.target_for(user), BTreeMap::new(), PathBuf::from("/"));
        for payload in hostile_payloads() {
            let out = runner
                .run(&Argv::of(["printf", "%s", payload]), Access::Read, &RunOpts::default())
                .unwrap_or_else(|e| panic!("{shell}: {payload:?} failed: {e}"));
            assert_eq!(out.stdout, *payload, "{shell} mangled {payload:?}");
        }
    }
}

/// INV-12 under every login shell: the value reaches the container's environment
/// and never appears in an argv on either machine.
#[test]
fn every_login_shell_delivers_env_values_without_argv_exposure() {
    if !enabled() {
        return;
    }
    let fixture = Fixture::start();
    fixture.wait_for_sshd();

    let secret = "hunter2 $(id) `id` 'quoted' \"double\" ;rm -rf /";
    let mut env = BTreeMap::new();
    env.insert("APP_SECRET".to_string(), secret.to_string());

    for (user, shell) in SHELLS {
        let target = fixture.target_for(user);
        assert!(
            !target.invocation().display().contains("hunter2"),
            "{shell}: no value may reach the ssh argv"
        );

        let runner = SshRunner::new(target, env.clone(), PathBuf::from("/"));
        let out = runner
            .run(
                &Argv::of(["sh", "-c", "printf %s \"$APP_SECRET\""]),
                Access::Read,
                &RunOpts::default(),
            )
            .unwrap_or_else(|e| panic!("{shell}: {e}"));
        assert_eq!(out.stdout, secret, "{shell} did not deliver the value intact");
    }
}

/// A failed `cd` must abort, not silently run the command in the login shell's
/// home directory — that would deploy against the wrong tree.
#[test]
fn a_missing_deploy_root_aborts_under_every_login_shell() {
    if !enabled() {
        return;
    }
    let fixture = Fixture::start();
    fixture.wait_for_sshd();

    for (user, shell) in SHELLS {
        let runner = SshRunner::new(
            fixture.target_for(user),
            BTreeMap::new(),
            PathBuf::from("/definitely/not/here"),
        );
        let result = runner.run(
            &Argv::of(["printf", "%s", "ran-anyway"]),
            Access::Read,
            &RunOpts::unchecked(),
        );
        let out = result.unwrap_or_else(|e| panic!("{shell}: {e}"));
        assert_ne!(out.code, 0, "{shell}: a failed cd must not report success");
        assert!(!out.stdout.contains("ran-anyway"), "{shell} ran the command anyway");
    }
}
