use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{enforce_check, Access, Argv, Clock, CmdOutput, CommandRunner, FileSystem, RunError, RunOpts};

pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, argv: &Argv, _access: Access, opts: &RunOpts) -> Result<CmdOutput, RunError> {
        let Some((program, args)) = argv.0.split_first() else {
            return Err(RunError::Spawn {
                argv: String::new(),
                source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty argv"),
            });
        };

        let output = Command::new(program)
            .args(args)
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
        fs::write(path, bytes)?;
        if let Some(mode) = mode {
            set_mode(path, mode)?;
        }
        Ok(())
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
