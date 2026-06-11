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
        let lock_path = deploy_root.join(format!(".dcd.{stage}.lock"));
        let meta_path = deploy_root.join(format!(".dcd.{stage}.lock.meta"));
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

impl Drop for StageLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.meta_path);
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
