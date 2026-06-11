//! Catches SIGINT/SIGTERM into a flag the executor polls between tasks and in
//! retry loops (spec §2.5), so an interrupt before cutover cleans up the black
//! and releases the lock through normal unwinding.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub struct Interrupt {
    flag: Arc<AtomicBool>,
}

impl Interrupt {
    pub fn install() -> Interrupt {
        let flag = Arc::new(AtomicBool::new(false));
        register(&flag);
        Interrupt { flag }
    }

    /// A never-triggered interrupt for tests and non-interactive contexts.
    pub fn inert() -> Interrupt {
        Interrupt {
            flag: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn triggered(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}

#[cfg(unix)]
fn register(flag: &Arc<AtomicBool>) {
    use signal_hook::consts::{SIGINT, SIGTERM};
    for signal in [SIGINT, SIGTERM] {
        let _ = signal_hook::flag::register(signal, Arc::clone(flag));
    }
}

#[cfg(not(unix))]
fn register(_flag: &Arc<AtomicBool>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggered_reflects_the_flag() {
        let flag = Arc::new(AtomicBool::new(false));
        let interrupt = Interrupt { flag: flag.clone() };
        assert!(!interrupt.triggered());
        flag.store(true, Ordering::Relaxed);
        assert!(interrupt.triggered());
    }
}
