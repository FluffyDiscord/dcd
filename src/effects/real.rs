use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{enforce_check, Access, Argv, Clock, CmdOutput, CommandRunner, FileSystem, RunError, RunOpts};

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
        let output = command
            .output()
            .map_err(|source| RunError::Spawn {
                argv: argv.display(),
                source,
            })?;

        let out = CmdOutput {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        };
        enforce_check(argv, out, opts)
    }
}

pub struct SystemFs;

impl FileSystem for SystemFs {
    fn write(&self, path: &Path, bytes: &[u8], mode: Option<u32>) -> std::io::Result<()> {
        match mode {
            // OpenOptions applies the mode only when CREATING; the chmod covers a
            // pre-existing file whose mode differs. New files never see the umask default.
            Some(mode) => {
                use std::io::Write;
                let mut file = open_with_mode(path, mode)?;
                file.write_all(bytes)?;
                set_mode(path, mode)
            }
            None => fs::write(path, bytes),
        }
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        fs::read(path)
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
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
            .run(&argv, Access::Read, &RunOpts { check: true, env: Some(overlay) })
            .unwrap();

        let lines: Vec<&str> = out.stdout.lines().collect();
        assert_eq!(lines[0], cwd.canonicalize().unwrap().to_str().unwrap());
        assert_eq!(lines[1], "chain-value");
        assert_eq!(lines[2], "from-overlay");
    }
}
