use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::{enforce_check, Access, Argv, Clock, CmdOutput, CommandRunner, FileSystem, RunError, RunOpts};

type EnvOverlay = Option<std::collections::BTreeMap<String, String>>;

/// One recorded command: everything the runner was actually given. `stdin` is part
/// of it because over ssh that is the ONLY carrier env values travel on (INV-12) —
/// discarding it left no unit test able to observe what dcd really sent.
struct RecordedCall {
    argv: Argv,
    access: Access,
    env: EnvOverlay,
    stdin: Option<Vec<u8>>,
}

/// Records every command and returns canned outputs keyed by an argv substring.
/// Unit tests assert the recorded argv sequence with no Docker.
#[derive(Default)]
pub struct RecordingRunner {
    calls: RefCell<Vec<RecordedCall>>,
    responses: Vec<(String, CmdOutput)>,
    unmatched: RefCell<Vec<String>>,
}

impl RecordingRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Canned stdout/exit for any command whose display contains `needle`.
    pub fn with_response(mut self, needle: &str, output: CmdOutput) -> Self {
        self.responses.push((needle.to_string(), output));
        self
    }

    pub fn with_stdout(self, needle: &str, stdout: &str) -> Self {
        self.with_response(
            needle,
            CmdOutput {
                code: 0,
                stdout: stdout.to_string(),
                stderr: String::new(),
            },
        )
    }

    pub fn calls(&self) -> Vec<Argv> {
        self.calls.borrow().iter().map(|call| call.argv.clone()).collect()
    }

    pub fn display_calls(&self) -> Vec<String> {
        self.calls.borrow().iter().map(|call| call.argv.display()).collect()
    }

    pub fn env_overlay_of(&self, needle: &str) -> EnvOverlay {
        self.calls
            .borrow()
            .iter()
            .find(|call| call.argv.display().contains(needle))
            .and_then(|call| call.env.clone())
    }

    /// What was piped to the command — over ssh, the document carrying every env
    /// value.
    pub fn stdin_of(&self, needle: &str) -> Option<Vec<u8>> {
        self.calls
            .borrow()
            .iter()
            .find(|call| call.argv.display().contains(needle))
            .and_then(|call| call.stdin.clone())
    }

    pub fn access_of(&self, needle: &str) -> Option<Access> {
        self.calls
            .borrow()
            .iter()
            .find(|call| call.argv.display().contains(needle))
            .map(|call| call.access)
    }

    /// Commands the double answered with a default `exit 0, stdout ""` because no
    /// canned response matched. Engine code that parses stdout then takes its
    /// "nothing to do" branch and the test passes because the double said nothing —
    /// so a needle that stops matching after an argv change degrades to green.
    /// Assert this is empty in any test whose subject reads a command's output.
    pub fn unmatched(&self) -> Vec<String> {
        self.unmatched.borrow().clone()
    }

    fn lookup(&self, argv: &Argv) -> CmdOutput {
        let shown = argv.display();
        let matched = self
            .responses
            .iter()
            .find(|(needle, _)| shown.contains(needle.as_str()))
            .map(|(_, out)| out.clone());
        match matched {
            Some(out) => out,
            None => {
                self.unmatched.borrow_mut().push(shown);
                CmdOutput::ok()
            }
        }
    }
}

impl CommandRunner for RecordingRunner {
    fn run(&self, argv: &Argv, access: Access, opts: &RunOpts) -> Result<CmdOutput, RunError> {
        self.calls.borrow_mut().push(RecordedCall {
            argv: argv.clone(),
            access,
            env: opts.env.clone(),
            stdin: opts.stdin.clone(),
        });
        enforce_check(argv, self.lookup(argv), opts)
    }
}

/// Read commands run for real against `inner`; mutating commands are stubbed and
/// recorded — the basis of an honest `--dry-run` (spec §2.4).
pub struct DryRunRunner<R: CommandRunner> {
    inner: R,
    stubbed: RefCell<Vec<Argv>>,
}

impl<R: CommandRunner> DryRunRunner<R> {
    pub fn new(inner: R) -> Self {
        DryRunRunner {
            inner,
            stubbed: RefCell::new(Vec::new()),
        }
    }

    pub fn stubbed(&self) -> Vec<Argv> {
        self.stubbed.borrow().clone()
    }
}

impl<R: CommandRunner> CommandRunner for DryRunRunner<R> {
    fn run(&self, argv: &Argv, access: Access, opts: &RunOpts) -> Result<CmdOutput, RunError> {
        match access {
            Access::Read => self.inner.run(argv, access, opts),
            Access::Mutate => {
                self.stubbed.borrow_mut().push(argv.clone());
                Ok(CmdOutput::ok())
            }
        }
    }
}

/// In-memory filesystem for unit tests.
#[derive(Default)]
pub struct MemoryFs {
    files: RefCell<HashMap<PathBuf, Vec<u8>>>,
    modes: RefCell<HashMap<PathBuf, Option<u32>>>,
    dirs: RefCell<HashSet<PathBuf>>,
}

impl MemoryFs {
    pub fn new() -> Self {
        Self::default()
    }

    /// The mode a write asked for. Dropping it left `dcd-state.json`'s `0600` — the
    /// one permission dcd sets deliberately — with no regression coverage at all.
    pub fn mode_of(&self, path: &Path) -> Option<u32> {
        self.modes.borrow().get(path).copied().flatten()
    }
}

impl FileSystem for MemoryFs {
    fn write(&self, path: &Path, bytes: &[u8], mode: Option<u32>) -> std::io::Result<()> {
        self.files.borrow_mut().insert(path.to_path_buf(), bytes.to_vec());
        self.modes.borrow_mut().insert(path.to_path_buf(), mode);
        Ok(())
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        self.files
            .borrow()
            .get(path)
            .cloned()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, path.display().to_string()))
    }

    fn exists(&self, path: &Path) -> std::io::Result<bool> {
        Ok(self.files.borrow().contains_key(path) || self.dirs.borrow().contains(path))
    }

    fn remove(&self, path: &Path) -> std::io::Result<()> {
        self.files.borrow_mut().remove(path);
        Ok(())
    }

    fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
        self.dirs.borrow_mut().insert(path.to_path_buf());
        Ok(())
    }
}

/// Deterministic clock for tests.
pub struct FixedClock(pub u64);

impl Clock for FixedClock {
    fn now_epoch(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_runner_records_and_cans() {
        let runner = RecordingRunner::new().with_stdout("transports", "async\nscheduler");
        let out = runner
            .run(&Argv::of(["docker", "exec", "app", "transports"]), Access::Read, &RunOpts::default())
            .unwrap();
        assert_eq!(out.stdout, "async\nscheduler");
        assert_eq!(runner.display_calls(), vec!["docker exec app transports"]);
    }

    #[test]
    fn recording_runner_enforces_check() {
        let runner = RecordingRunner::new().with_response(
            "boom",
            CmdOutput {
                code: 1,
                stdout: String::new(),
                stderr: "nope".into(),
            },
        );
        let err = runner.run(&Argv::of(["boom"]), Access::Mutate, &RunOpts::default());
        assert!(err.is_err());
        let ok = runner.run(&Argv::of(["boom"]), Access::Mutate, &RunOpts::unchecked());
        assert_eq!(ok.unwrap().code, 1);
    }

    #[test]
    fn dry_run_passes_reads_stubs_mutations() {
        let inner = RecordingRunner::new().with_stdout("inspect", "image:v1");
        let dry = DryRunRunner::new(inner);
        let read = dry
            .run(&Argv::of(["docker", "inspect", "c"]), Access::Read, &RunOpts::default())
            .unwrap();
        assert_eq!(read.stdout, "image:v1");
        let mutate = dry
            .run(&Argv::of(["docker", "run", "-d", "x"]), Access::Mutate, &RunOpts::default())
            .unwrap();
        assert_eq!(mutate.stdout, "");
        assert_eq!(dry.stubbed().len(), 1);
        assert_eq!(dry.stubbed()[0].display(), "docker run -d x");
    }

    #[test]
    fn memory_fs_roundtrip() {
        let fs = MemoryFs::new();
        let p = Path::new("/x/y.txt");
        assert!(!fs.exists(p).unwrap());
        fs.write(p, b"hi", Some(0o600)).unwrap();
        assert!(fs.exists(p).unwrap());
        assert_eq!(fs.read(p).unwrap(), b"hi");
        fs.remove(p).unwrap();
        assert!(!fs.exists(p).unwrap());
    }
}
