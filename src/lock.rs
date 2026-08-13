//! Per-stage `flock(2)` advisory lock (spec INV-4). The OS releases it on any
//! process exit, including SIGKILL, so a crash never strands a stale lock; a
//! sidecar `.meta` file carries the human-readable holder for the busy message.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use fs2::FileExt;

use crate::error::{DcdError, Result};

#[derive(Debug)]
pub struct StageLock {
    _file: File,
    meta_path: PathBuf,
}

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
                Ok(StageLock {
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
        let _ = std::fs::remove_file(&self.meta_path);
    }
}

fn lock_path(deploy_root: &Path, stage: &str) -> PathBuf {
    deploy_root.join(format!(".dcd.{stage}.lock"))
}

fn meta_path(deploy_root: &Path, stage: &str) -> PathBuf {
    deploy_root.join(format!(".dcd.{stage}.lock.meta"))
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
