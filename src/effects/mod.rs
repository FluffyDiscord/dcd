//! The side-effect seam: every process spawn, file touch, and clock read goes
//! through one of these traits so the engine and recipe are testable with no
//! Docker, and `--dry-run` is a runner swap.

mod real;
mod record;

pub use real::{SshFs, SshRunner, SystemClock, SystemFs, SystemRunner};
pub use record::{DryRunRunner, FixedClock, MemoryFs, RecordingRunner};

use std::path::Path;

/// Whether a command observes state (safe to run in `--dry-run`) or changes it
/// (stubbed in `--dry-run`). Commands that depend on a stubbed mutation having
/// happened (healthcheck/migrate/provider exec into the black) are `Mutate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Mutate,
}

/// A command as an argument vector — never a shell string, so it is injection-safe
/// and is exactly what `--dry-run` prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Argv(pub Vec<String>);

impl Argv {
    pub fn of<I, S>(parts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Argv(parts.into_iter().map(Into::into).collect())
    }

    pub fn display(&self) -> String {
        self.0.join(" ")
    }
}

#[derive(Debug, Clone)]
pub struct RunOpts {
    /// Error on a non-zero exit (the common case).
    pub check: bool,
    /// Per-command env overlay applied on top of the runner's own env — the
    /// delivery path for the explicit config env maps (spec §5.2.4).
    pub env: Option<std::collections::BTreeMap<String, String>>,
    /// Bytes piped to the command's stdin. Over SSH this is how env values reach
    /// the target at all, since a local process env does not cross the wire — and
    /// it is why they never appear in an argv on either machine (INV-12).
    pub stdin: Option<Vec<u8>>,
}

impl Default for RunOpts {
    fn default() -> Self {
        RunOpts { check: true, env: None, stdin: None }
    }
}

impl RunOpts {
    pub fn unchecked() -> Self {
        RunOpts { check: false, env: None, stdin: None }
    }
}

#[derive(Debug, Clone)]
pub struct CmdOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOutput {
    pub fn ok() -> Self {
        CmdOutput {
            code: 0,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    pub fn success(&self) -> bool {
        self.code == 0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("failed to spawn `{argv}`: {source}")]
    Spawn {
        argv: String,
        source: std::io::Error,
    },

    /// ssh itself failed — auth, DNS, a dropped link — rather than the command it
    /// carried. Kept apart so a severed network is not classified as the deploy
    /// being rejected (spec §8.1 exit 6).
    #[error("{stderr}")]
    Transport { argv: String, stderr: String },

    #[error("command failed ({code}): `{argv}`\n{stderr}")]
    NonZero {
        argv: String,
        code: i32,
        stdout: String,
        stderr: String,
    },
}

/// Runs commands to completion and captures their output.
pub trait CommandRunner {
    fn run(&self, argv: &Argv, access: Access, opts: &RunOpts) -> std::result::Result<CmdOutput, RunError>;
}

/// File operations the recipe and config need, behind a trait so tests use memory.
pub trait FileSystem {
    fn write(&self, path: &Path, bytes: &[u8], mode: Option<u32>) -> std::io::Result<()>;
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>>;
    /// Fallible on purpose: over ssh, "I could not ask" is not "the file is not
    /// there". Collapsing the two let a single failed probe read as a fresh stage
    /// and overwrite a live upstream file.
    fn exists(&self, path: &Path) -> std::io::Result<bool>;
    fn remove(&self, path: &Path) -> std::io::Result<()>;
    fn create_dir_all(&self, path: &Path) -> std::io::Result<()>;
}

/// Wall clock, behind a trait so `release_id` and timestamps are deterministic in tests.
pub trait Clock {
    fn now_epoch(&self) -> u64;
}

/// Applies the `RunOpts::check` rule to a completed command.
pub fn enforce_check(argv: &Argv, out: CmdOutput, opts: &RunOpts) -> std::result::Result<CmdOutput, RunError> {
    if opts.check && !out.success() {
        return Err(RunError::NonZero {
            argv: argv.display(),
            code: out.code,
            stdout: out.stdout,
            stderr: out.stderr,
        });
    }
    Ok(out)
}
