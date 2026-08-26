//! Per-stage advisory lock (spec INV-4). The lock cannot outlive its holder in
//! either mode: locally the kernel drops the `flock(2)` on any process exit,
//! SIGKILL included; remotely a leased holder on the target drops it on channel
//! EOF or on heartbeat loss. A sidecar `.meta` file carries the human-readable
//! holder for the busy message.

use crate::effects::{Argv, CmdOutput};
use crate::ssh::SshTarget;

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use fs2::FileExt;

use crate::error::{DcdError, Result};

/// How a stage lock is held. `Local` is the `flock(2)` this process owns; `Leased`
/// is a `flock(2)` owned by a process on the target whose lifetime is the ssh
/// channel plus a heartbeat (INV-4).
#[derive(Debug)]
pub enum StageLock {
    Local { _file: File, meta_path: PathBuf },
    Leased(LeasedLock),
}

/// A lock held on the target: a real `flock(2)` owned by a remote process whose
/// lifetime is this ssh channel plus a heartbeat. Dropping this closes the
/// channel, the remote reader sees EOF and exits, and the kernel there releases
/// the lock — so the lock cannot outlive its holder even on SIGKILL. A severed
/// network is covered by the lease: no heartbeat for `lease_seconds` and the
/// remote `read -t` times out, with the same result (INV-4).
pub struct LeasedLock {
    stage: String,
    child: Option<std::process::Child>,
    beating: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl std::fmt::Debug for LeasedLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeasedLock").field("stage", &self.stage).finish()
    }
}

impl Drop for LeasedLock {
    fn drop(&mut self) {
        self.beating.store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(mut child) = self.child.take() {
            drop(child.stdin.take()); // EOF: the remote holder exits, the kernel drops the lock
            let _ = child.wait();
        }
    }
}

impl LeasedLock {
    pub fn lease_seconds() -> u64 {
        30
    }

    pub fn heartbeat_seconds() -> u64 {
        10
    }

    /// The lease the target runs under `flock`: announce the lock, then exit the
    /// moment the channel goes quiet for a whole lease. Nobody judges whether the
    /// holder is alive — the kernel releases the flock when this shell exits (INV-4).
    pub fn lease_script() -> String {
        format!(
            "echo {ACQUIRED}\nwhile read -t {} _; do :; done\n",
            LeasedLock::lease_seconds()
        )
    }

    pub fn stage(&self) -> &str {
        &self.stage
    }

    /// Spawns the remote holder and waits for it to confirm it actually took the
    /// lock. `flock -n` fails immediately when another deploy holds it, so the
    /// confirmation line is what distinguishes "acquired" from "busy" — never a
    /// timing guess.
    ///
    /// This is the ONE command whose stdin must stay a live channel (the
    /// heartbeat), so it cannot also carry a script the way every other command
    /// does. Instead the loop is written to a file first, and the holder is
    /// invoked as bare words — `flock -n <lock> sh <loop>` — which every login
    /// shell parses identically, because none of them contains a metacharacter.
    pub fn acquire(target: &SshTarget, deploy_root: &Path, stage: &str, holder: &str) -> Result<StageLock> {
        let lock = lock_path(deploy_root, stage).display().to_string();
        let loop_file = lease_path(deploy_root, stage).display().to_string();
        LeasedLock::require_bare_words(&[&lock, &loop_file])?;

        LeasedLock::put_file(target, &loop_file, &LeasedLock::lease_script())?;

        let argv = target.bare_argv(&Argv::of(["flock", "-n", &lock, "sh", &loop_file]));
        let (program, args) = argv.0.split_first().expect("the invocation always starts with ssh");
        let mut child = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| DcdError::Config(format!("cannot start the remote lock holder: {e}")))?;

        let mut stdout = child.stdout.take().expect("stdout is piped");
        let mut confirmation = [0u8; ACQUIRED.len() + 1];
        let took_it = std::io::Read::read_exact(&mut stdout, &mut confirmation).is_ok()
            && confirmation.starts_with(ACQUIRED.as_bytes());
        if !took_it {
            // No token can mean three different things, and only one of them is a
            // lock. `flock -n` exits 1 when another holder has it; ssh exits 255
            // for its own failures; anything else is the target refusing to run the
            // lease at all. Reporting all three as "another deploy holds the stage"
            // sends the operator hunting a deploy that does not exist.
            let _ = child.kill();
            let out = child.wait_with_output();
            let (code, stderr) = match &out {
                Ok(out) => (out.status.code(), String::from_utf8_lossy(&out.stderr).trim().to_string()),
                Err(_) => (None, String::new()),
            };
            return match code {
                Some(255) => Err(DcdError::Config(format!(
                    "cannot reach {} to take the {stage} lock: {stderr}",
                    target.target()
                ))),
                Some(1) | None => Err(DcdError::LockHeld {
                    stage: stage.to_string(),
                    holder: LeasedLock::read_holder(target, deploy_root, stage),
                }),
                Some(code) => Err(DcdError::Config(format!(
                    "the {stage} lock holder could not start on the target (exit {code}): {stderr}"
                ))),
            };
        }

        LeasedLock::write_holder(target, deploy_root, stage, holder);

        let beating = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let mut stdin = child.stdin.take().expect("stdin is piped");
        let ticking = beating.clone();
        std::thread::spawn(move || {
            use std::io::Write;
            while ticking.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_secs(LeasedLock::heartbeat_seconds()));
                if !ticking.load(std::sync::atomic::Ordering::Relaxed) || writeln!(stdin).is_err() {
                    return;
                }
                let _ = stdin.flush();
            }
        });

        Ok(StageLock::Leased(LeasedLock {
            stage: stage.to_string(),
            child: Some(child),
            beating,
        }))
    }

    /// The holder is invoked as bare words, so its paths must survive an unquoted
    /// parse in a shell dcd did not choose. Anything else is refused up front
    /// rather than mis-executed on the target.
    fn require_bare_words(words: &[&str]) -> Result<()> {
        for word in words {
            let safe = !word.is_empty()
                && word
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/'));
            if !safe {
                return Err(DcdError::Config(format!(
                    "deploy_root must contain no whitespace or shell metacharacters — the stage lock is \
                     invoked as bare words so it parses identically in every login shell (got '{word}')"
                )));
            }
        }
        Ok(())
    }

    /// Writes a file on the target the one way dcd writes files there, with the
    /// payload embedded in the script. It cannot be appended to the script for the
    /// remote `sh` to `cat` off its own stdin: sh buffers the whole stream, so the
    /// file lands empty — and an empty lease script exits 0 without ever taking
    /// the lock, which every deploy then reads as "the stage is held".
    fn put_file(target: &SshTarget, path: &str, contents: &str) -> Result<()> {
        let script = crate::ssh::write_file_script(path, contents.as_bytes(), None);
        LeasedLock::send_checked(target, &script, "stage the lock lease")?;
        Ok(())
    }

    /// Runs a script on the target and reports what the remote shell actually did.
    /// The exit status is part of the answer: an ssh that authenticated but whose
    /// script failed — a read-only `deploy_root`, no `base64` on the target —
    /// otherwise looks like a successful write, and the next step mistranslates it
    /// into "another deploy holds the stage".
    fn send_script(target: &SshTarget, script: &str) -> std::io::Result<CmdOutput> {
        use std::io::Write;
        let invocation = target.invocation();
        let (program, args) = invocation.0.split_first().expect("the invocation always starts with ssh");
        let mut child = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        if let Some(mut pipe) = child.stdin.take() {
            let _ = pipe.write_all(script.as_bytes());
        }
        let out = child.wait_with_output()?;
        Ok(CmdOutput {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    /// `send_script` for the callers that must not continue on a failed remote
    /// command.
    fn send_checked(target: &SshTarget, script: &str, what: &str) -> Result<CmdOutput> {
        let out = LeasedLock::send_script(target, script)
            .map_err(|e| DcdError::Config(format!("cannot {what} on the target: {e}")))?;
        if !out.success() {
            return Err(DcdError::Config(format!(
                "cannot {what} on the target (exit {}): {}",
                out.code,
                out.stderr.trim()
            )));
        }
        Ok(out)
    }

    /// Best-effort: the sidecar is only the human-readable "who holds it" message,
    /// while the `flock` itself is the mutex.
    fn write_holder(target: &SshTarget, deploy_root: &Path, stage: &str, holder: &str) {
        let path = meta_path(deploy_root, stage).display().to_string();
        let script = crate::ssh::write_file_script(&path, format!("{holder}\n").as_bytes(), None);
        let _ = LeasedLock::send_script(target, &script);
    }

    /// A held lock is by definition still heartbeating, so the sidecar names a
    /// live deploy rather than a maybe-stale one.
    fn read_holder(target: &SshTarget, deploy_root: &Path, stage: &str) -> String {
        let path = meta_path(deploy_root, stage).display().to_string();
        let script = format!("exec cat {}\n", crate::ssh::quote(&path));
        LeasedLock::send_script(target, &script)
            .ok()
            .filter(CmdOutput::success)
            .map(|out| out.stdout.trim().to_string())
            .filter(|holder| !holder.is_empty())
            .unwrap_or_else(|| "unknown holder".to_string())
    }

    /// Whether a live holder heartbeats the lock on the TARGET right now. `flock
    /// -n` creating the file when it is absent is fine: `acquire` creates it too,
    /// and `unlock` removes it moments later.
    pub fn is_held(target: &SshTarget, deploy_root: &Path, stage: &str) -> bool {
        let lock = lock_path(deploy_root, stage).display().to_string();
        let script = format!("exec flock -n {} true\n", crate::ssh::quote(&lock));
        // Only `flock`'s own "someone else has it" exit means held. A target we
        // cannot reach, or one without `flock`, is not a running deploy — saying so
        // would warn the operator about a deploy that does not exist.
        matches!(LeasedLock::send_script(target, &script).map(|out| out.code), Ok(1))
    }

    /// The human-readable holder recorded on the target, for the `unlock` warning.
    pub fn holder(target: &SshTarget, deploy_root: &Path, stage: &str) -> String {
        LeasedLock::read_holder(target, deploy_root, stage)
    }

    /// Removes a remote lock even while a live holder heartbeats it — `dcd unlock`
    /// is an operator override, not a repair, because INV-4 means a stale remote
    /// lock cannot exist.
    pub fn force_release(target: &SshTarget, deploy_root: &Path, stage: &str) -> Result<Vec<PathBuf>> {
        let mut paths = StageLock::paths(deploy_root, stage).to_vec();
        paths.push(lease_path(deploy_root, stage));
        let quoted: Vec<String> = paths
            .iter()
            .map(|path| crate::ssh::quote(&path.display().to_string()))
            .collect();
        // Echoing what was actually removed keeps the remote branch honest: `rm -f`
        // ignores a missing file, so reporting the whole list would tell the
        // operator a lock was cleared on a stage that never had one.
        let script = format!(
            "for f in {}; do if [ -e \"$f\" ]; then rm -f \"$f\" && printf '%s\\n' \"$f\"; fi; done\n",
            quoted.join(" ")
        );
        let out = LeasedLock::send_checked(target, &script, "clear the lock")?;
        Ok(out.stdout.lines().map(PathBuf::from).collect())
    }
}

/// What the remote holder prints once `flock` has actually granted the lock. The
/// newline is the read terminator, and is deliberately NOT part of the token —
/// embedding it in the script would break the remote command across lines.
const ACQUIRED: &str = "dcd-lock-acquired";

impl StageLock {
    pub fn acquire(deploy_root: &Path, stage: &str, holder: &str) -> Result<StageLock> {
        let lock_path = lock_path(deploy_root, stage);
        let meta_path = meta_path(deploy_root, stage);
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| DcdError::Config(format!("cannot open lock {}: {e}", lock_path.display())))?;

        match file.try_lock_exclusive() {
            Ok(()) => {
                let _ = std::fs::write(&meta_path, format!("{holder}\n"));
                Ok(StageLock::Local {
                    _file: file,
                    meta_path,
                })
            }
            Err(_) => {
                let held_by = std::fs::read_to_string(&meta_path)
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|_| "unknown holder".to_string());
                Err(DcdError::LockHeld {
                    stage: stage.to_string(),
                    holder: held_by,
                })
            }
        }
    }
}

impl StageLock {
    /// The lock file and its sidecar, whether or not they exist.
    pub fn paths(deploy_root: &Path, stage: &str) -> [PathBuf; 2] {
        [lock_path(deploy_root, stage), meta_path(deploy_root, stage)]
    }

    /// What the sidecar says about the holder. The sidecar alone proves nothing —
    /// the OS drops the flock on any exit, so it can outlive the process that wrote it.
    pub fn holder(deploy_root: &Path, stage: &str) -> Option<String> {
        let meta = std::fs::read_to_string(meta_path(deploy_root, stage)).ok()?;
        let holder = meta.trim().to_string();
        if holder.is_empty() {
            return None;
        }
        Some(holder)
    }

    /// Whether a live process holds the flock right now — the only trustworthy
    /// "a deploy is running" signal (`unlock` warns before overriding it).
    pub fn is_held(deploy_root: &Path, stage: &str) -> bool {
        let Ok(file) = File::open(lock_path(deploy_root, stage)) else {
            return false;
        };
        let probe = file.try_lock_exclusive();
        probe.is_err()
    }

    /// Delete the stage lock and its sidecar even while another process holds the
    /// flock — `dcd unlock` is the escape hatch for a deploy that can no longer
    /// finish. Returns the paths that existed and were removed.
    pub fn force_release(deploy_root: &Path, stage: &str) -> Result<Vec<PathBuf>> {
        let mut removed = Vec::new();
        for path in StageLock::paths(deploy_root, stage) {
            if !path.exists() {
                continue;
            }
            std::fs::remove_file(&path)
                .map_err(|e| DcdError::Config(format!("cannot remove lock {}: {e}", path.display())))?;
            removed.push(path);
        }
        Ok(removed)
    }
}

impl Drop for StageLock {
    fn drop(&mut self) {
        if let StageLock::Local { meta_path, .. } = self {
            let _ = std::fs::remove_file(meta_path);
        }
    }
}

fn lock_path(deploy_root: &Path, stage: &str) -> PathBuf {
    deploy_root.join(format!(".dcd.{stage}.lock"))
}

fn meta_path(deploy_root: &Path, stage: &str) -> PathBuf {
    deploy_root.join(format!(".dcd.{stage}.lock.meta"))
}

/// The lease loop `acquire` stages on the target. `unlock` removes it too —
/// otherwise the one file the remote lock mechanism writes is the one nothing
/// ever cleans up.
fn lease_path(deploy_root: &Path, stage: &str) -> PathBuf {
    deploy_root.join(format!(".dcd.{stage}.lease.sh"))
}

#[cfg(test)]
mod lease_tests {
    use super::*;

    /// INV-4: the remote holder must exit on a quiet channel, so a severed link
    /// releases the lock without anyone judging whether the holder is alive. This
    /// asserts the script `acquire` actually stages and runs — the lock mechanism
    /// is a lease FILE plus `flock -n <lock> sh <file>`, not a `flock -c` one-liner.
    #[test]
    fn the_lease_announces_the_lock_then_bounds_its_own_lifetime() {
        let script = LeasedLock::lease_script();
        assert_eq!(
            script,
            format!("echo dcd-lock-acquired\nwhile read -t {} _; do :; done\n", LeasedLock::lease_seconds())
        );
        assert!(
            script.starts_with(&format!("echo {ACQUIRED}")),
            "the token must be the FIRST thing the holder prints, or acquire cannot tell it apart from a busy lock"
        );
    }

    /// A heartbeat must arrive well inside the lease, or a healthy deploy would
    /// drop its own lock mid-run.
    #[test]
    fn heartbeat_is_comfortably_inside_the_lease() {
        assert!(LeasedLock::heartbeat_seconds() * 2 <= LeasedLock::lease_seconds());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dcd-lock-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn held_lock_blocks_then_reclaims_after_release() {
        let dir = temp_dir("a");
        let first = StageLock::acquire(&dir, "prod", "pid 1 since 16:40").unwrap();

        let second = StageLock::acquire(&dir, "prod", "pid 2 since 16:41");
        match second {
            Err(DcdError::LockHeld { stage, holder }) => {
                assert_eq!(stage, "prod");
                assert_eq!(holder, "pid 1 since 16:40");
            }
            other => panic!("expected LockHeld, got {other:?}"),
        }

        drop(first);
        let third = StageLock::acquire(&dir, "prod", "pid 3 since 16:42");
        assert!(third.is_ok());
        drop(third);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn force_release_drops_a_lock_a_live_process_still_holds() {
        let dir = temp_dir("c");
        let held = StageLock::acquire(&dir, "prod", "pid 1 since 16:40").unwrap();
        assert!(StageLock::is_held(&dir, "prod"));
        assert_eq!(StageLock::holder(&dir, "prod").as_deref(), Some("pid 1 since 16:40"));

        let removed = StageLock::force_release(&dir, "prod").unwrap();
        assert_eq!(removed.len(), 2);
        assert!(!StageLock::is_held(&dir, "prod"));
        assert_eq!(StageLock::holder(&dir, "prod"), None);

        // idempotent: nothing left to remove, and no error
        assert!(StageLock::force_release(&dir, "prod").unwrap().is_empty());

        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_released_lock_reads_as_unheld_even_with_a_stale_sidecar() {
        let dir = temp_dir("d");
        drop(StageLock::acquire(&dir, "prod", "pid 1 since 16:40").unwrap());
        std::fs::write(dir.join(".dcd.prod.lock.meta"), "pid 1 since 16:40\n").unwrap();

        assert!(!StageLock::is_held(&dir, "prod"));
        assert_eq!(StageLock::holder(&dir, "prod").as_deref(), Some("pid 1 since 16:40"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn distinct_stages_do_not_collide() {
        let dir = temp_dir("b");
        let a = StageLock::acquire(&dir, "beta", "x").unwrap();
        let b = StageLock::acquire(&dir, "prod", "y");
        assert!(b.is_ok());
        drop(a);
        drop(b);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
