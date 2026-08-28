use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{enforce_check, Access, Argv, Clock, CmdOutput, CommandRunner, FileSystem, RunError, RunOpts};
use crate::ssh::SshTarget;

/// Spawns every child with the chain runner env and `deploy_root` as its cwd
/// (spec §5.2.4); per-command `RunOpts::env` overlays win over the runner env.
#[derive(Default)]
pub struct SystemRunner {
    env: BTreeMap<String, String>,
    cwd: Option<PathBuf>,
}

impl SystemRunner {
    pub fn with_context(env: BTreeMap<String, String>, cwd: PathBuf) -> Self {
        SystemRunner { env, cwd: Some(cwd) }
    }

    /// The resolved chain, but in dcd's OWN working directory: `docker compose
    /// config` reads the compose files from the checkout, on this machine, while
    /// still needing the chain to interpolate `${REGISTRY}` and friends.
    pub fn with_env(env: BTreeMap<String, String>) -> Self {
        SystemRunner { env, cwd: None }
    }
}

impl CommandRunner for SystemRunner {
    fn run(&self, argv: &Argv, _access: Access, opts: &RunOpts) -> Result<CmdOutput, RunError> {
        let Some((program, args)) = argv.0.split_first() else {
            return Err(RunError::Spawn {
                argv: String::new(),
                source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty argv"),
            });
        };

        let mut command = Command::new(program);
        command.args(args).envs(&self.env);
        if let Some(overlay) = &opts.env {
            command.envs(overlay);
        }
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        let output = run_with_stdin(command, argv, opts.stdin.as_deref())?;

        let out = CmdOutput {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        };
        enforce_check(argv, out, opts)
    }
}

/// Spawns a child, optionally piping bytes to its stdin, and waits for it. The
/// stdin write happens on this thread between spawn and wait: the payloads dcd
/// sends (an env document, a compose file) are far below a pipe buffer, and a
/// child that never reads its stdin still gets EOF when the handle drops.
fn run_with_stdin(
    mut command: Command,
    argv: &Argv,
    stdin: Option<&[u8]>,
) -> Result<std::process::Output, RunError> {
    let Some(payload) = stdin else {
        return command.output().map_err(|source| RunError::Spawn {
            argv: argv.display(),
            source,
        });
    };

    use std::io::Write;
    use std::process::Stdio;
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| RunError::Spawn {
            argv: argv.display(),
            source,
        })?;
    if let Some(mut pipe) = child.stdin.take() {
        let _ = pipe.write_all(payload);
    }
    child.wait_with_output().map_err(|source| RunError::Spawn {
        argv: argv.display(),
        source,
    })
}

/// The sibling a staged write renames from. Carries the pid so two writers on one
/// shared `deploy_root` cannot collide on a fixed `.tmp` name.
fn staging_path(path: &Path) -> std::path::PathBuf {
    let mut staged = path.as_os_str().to_os_string();
    staged.push(format!(".tmp.{}", std::process::id()));
    std::path::PathBuf::from(staged)
}

pub struct SystemFs;

impl FileSystem for SystemFs {
    /// Staged and renamed, like the ssh backend: `dcd-state.json` is the INV-3 write,
    /// and a torn one fails every later command on the stage, `unlock` included.
    /// Writing in place made that possible locally while the remote path was safe —
    /// two backends of one trait with different durability is a trap, not a detail.
    fn write(&self, path: &Path, bytes: &[u8], mode: Option<u32>) -> std::io::Result<()> {
        use std::io::Write;
        let staged = staging_path(path);
        let outcome = (|| {
            match mode {
                // OpenOptions applies the mode only when CREATING, so the staged
                // file never exists at the umask default even briefly.
                Some(mode) => {
                    let mut file = open_with_mode(&staged, mode)?;
                    file.write_all(bytes)?;
                    file.sync_all()?;
                    set_mode(&staged, mode)?;
                }
                None => fs::write(&staged, bytes)?,
            }
            fs::rename(&staged, path)
        })();
        if outcome.is_err() {
            let _ = fs::remove_file(&staged);
        }
        outcome
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        fs::read(path)
    }

    fn exists(&self, path: &Path) -> std::io::Result<bool> {
        Ok(path.exists())
    }

    fn remove(&self, path: &Path) -> std::io::Result<()> {
        fs::remove_file(path)
    }

    fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
        fs::create_dir_all(path)
    }
}

#[cfg(unix)]
fn open_with_mode(path: &Path, mode: u32) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)
}

#[cfg(not(unix))]
fn open_with_mode(path: &Path, _mode: u32) -> std::io::Result<fs::File> {
    fs::OpenOptions::new().write(true).create(true).truncate(true).open(path)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

/// Runs every command on the target through one multiplexed ssh connection
/// (ADR-014). The env overlay does NOT cross the wire as a process env — it is
/// rendered into a document on stdin and sourced there, so no value ever reaches
/// an argv on either machine (INV-12).
pub struct SshRunner {
    target: SshTarget,
    env: BTreeMap<String, String>,
    deploy_root: PathBuf,
}

impl SshRunner {
    pub fn new(target: SshTarget, env: BTreeMap<String, String>, deploy_root: PathBuf) -> Self {
        SshRunner {
            target,
            env,
            deploy_root,
        }
    }

    pub fn target(&self) -> &SshTarget {
        &self.target
    }

    /// Exactly what `send` spawns. Exposed so INV-12 can be asserted against the
    /// argv that really reaches the process table, rather than against an object
    /// the env map was never given.
    pub fn spawn_argv(&self) -> Argv {
        self.target.invocation()
    }
}

impl CommandRunner for SshRunner {
    fn run(&self, argv: &Argv, _access: Access, opts: &RunOpts) -> Result<CmdOutput, RunError> {
        let mut delivered = self.env.clone();
        if let Some(overlay) = &opts.env {
            delivered.extend(overlay.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        let script = self.target.script_for(argv, Some(&self.deploy_root), &delivered);
        let out = self.send(&script, argv)?;
        enforce_check(argv, out, opts)
    }
}

impl SshRunner {
    /// One ssh invocation, script on stdin. Nothing here is quoted for the login
    /// shell, because the login shell only ever sees the word `sh`.
    fn send(&self, script: &[u8], argv: &Argv) -> Result<CmdOutput, RunError> {
        let invocation = self.target.invocation();
        let (program, args) = invocation.0.split_first().expect("the invocation always starts with ssh");
        let mut command = Command::new(program);
        command.args(args);
        let output = run_with_stdin(command, &invocation, Some(script))?;

        let out = CmdOutput {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        };

        // ssh exits 255 for its OWN failures — auth, DNS, a dropped connection.
        // Reporting that as an ordinary non-zero would let the engine classify a
        // severed link as an application failure, and the best-effort call sites
        // would swallow it and keep issuing commands (spec §2.7).
        // A link severed AFTER the remote command wrote some stdout is still a
        // transport failure, so an empty stdout cannot be the only signal — ssh
        // announces its own failures on stderr, and those are what identify it.
        let ssh_spoke = out.stderr.contains("ssh:")
            || out.stderr.contains("Connection closed")
            || out.stderr.contains("Connection reset")
            || out.stderr.contains("Connection refused")
            || out.stderr.contains("Permission denied")
            || out.stderr.contains("Host key verification failed")
            || out.stderr.contains("Timeout, server")
            || out.stderr.contains("Broken pipe");
        if out.code == 255 && (out.stdout.is_empty() || ssh_spoke) {
            return Err(RunError::Transport {
                argv: argv.display(),
                stderr: format!(
                    "connection to {} lost or refused: {}",
                    self.target.target(),
                    out.stderr.trim()
                ),
            });
        }
        Ok(out)
    }

    /// A file operation, as a script with its payload embedded. Uploads cannot use
    /// a second stdin stream — stdin already carries the script, and a shell
    /// reading a script from a pipe does not hand the remainder to the command it
    /// runs (verified) — so bytes ride as base64 inside the script itself.
    fn file_op(&self, script: String) -> Result<CmdOutput, RunError> {
        self.send(script.as_bytes(), &Argv::of(["sh"]))
    }
}

/// The five file operations dcd owns, as commands on the target. Writes are
/// always staged and renamed: `persist_state` is the INV-3 write, and a torn
/// `dcd-state.json` would fail every later command on that stage, `unlock`
/// included.
pub struct SshFs {
    runner: SshRunner,
}

impl SshFs {
    pub fn new(runner: SshRunner) -> Self {
        SshFs { runner }
    }

    fn run(&self, script: String) -> std::io::Result<CmdOutput> {
        self.runner
            .file_op(script)
            .map_err(|e| std::io::Error::other(e.to_string()))
    }

    fn checked(&self, script: String, what: &str) -> std::io::Result<()> {
        let out = self.run(script)?;
        if out.success() {
            return Ok(());
        }
        Err(std::io::Error::other(format!("{what}: {}", out.stderr.trim())))
    }
}

impl FileSystem for SshFs {
    fn write(&self, path: &Path, bytes: &[u8], mode: Option<u32>) -> std::io::Result<()> {
        self.checked(crate::ssh::write_file_script(&path.display().to_string(), bytes, mode), "write")
    }

    /// `MISSING` distinguishes "the target says it is not there" from every other
    /// way the command can fail. Without it a dropped connection reads as a missing
    /// file, and `load_state` treats a live stage as a fresh one.
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        let quoted = crate::ssh::quote(&path.display().to_string());
        // base64 both ways: stdout is captured as a lossy String, so a raw `cat`
        // turned every non-UTF-8 byte into U+FFFD — silent corruption on a path the
        // write side already base64s precisely to avoid.
        let out = self.run(format!(
            "if [ -e {quoted} ]; then exec base64 {quoted}; else exit {MISSING}; fi\n"
        ))?;
        if out.code == MISSING {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                path.display().to_string(),
            ));
        }
        if !out.success() {
            return Err(std::io::Error::other(format!(
                "cannot read {} (exit {}): {}",
                path.display(),
                out.code,
                out.stderr.trim()
            )));
        }
        crate::ssh::base64_decode(&out.stdout).ok_or_else(|| {
            std::io::Error::other(format!("cannot decode {} from the target", path.display()))
        })
    }

    fn exists(&self, path: &Path) -> std::io::Result<bool> {
        let quoted = crate::ssh::quote(&path.display().to_string());
        let out = self.run(format!("if [ -e {quoted} ]; then exit 0; else exit {MISSING}; fi\n"))?;
        match out.code {
            0 => Ok(true),
            MISSING => Ok(false),
            code => Err(std::io::Error::other(format!(
                "cannot test {} (exit {}): {}",
                path.display(),
                code,
                out.stderr.trim()
            ))),
        }
    }

    fn remove(&self, path: &Path) -> std::io::Result<()> {
        let quoted = crate::ssh::quote(&path.display().to_string());
        self.checked(format!("exec rm -f {quoted}\n"), "remove")
    }

    fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
        let quoted = crate::ssh::quote(&path.display().to_string());
        self.checked(format!("exec mkdir -p {quoted}\n"), "mkdir")
    }
}

/// The exit status `SshFs` reserves for "the path is not there", so it is never
/// confused with a transport failure or a shell that could not run.
const MISSING: i32 = 66;

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_epoch(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// TC-035's mechanism: the runner env + cwd reach every child, and a
    /// per-command overlay wins over the runner env.
    #[test]
    fn system_runner_delivers_env_cwd_and_overlay() {
        let cwd = std::env::temp_dir();
        let env: BTreeMap<String, String> = [
            ("DCD_TEST_CHAIN".to_string(), "chain-value".to_string()),
            ("DCD_TEST_SHARED".to_string(), "from-runner".to_string()),
        ]
        .into_iter()
        .collect();
        let runner = SystemRunner::with_context(env, cwd.clone());

        let argv = Argv::of(["sh", "-c", "pwd; printenv DCD_TEST_CHAIN; printenv DCD_TEST_SHARED"]);
        let overlay: BTreeMap<String, String> =
            [("DCD_TEST_SHARED".to_string(), "from-overlay".to_string())].into_iter().collect();
        let out = runner
            .run(&argv, Access::Read, &RunOpts { check: true, env: Some(overlay), stdin: None })
            .unwrap();

        let lines: Vec<&str> = out.stdout.lines().collect();
        assert_eq!(lines[0], cwd.canonicalize().unwrap().to_str().unwrap());
        assert_eq!(lines[1], "chain-value");
        assert_eq!(lines[2], "from-overlay");
    }
}
