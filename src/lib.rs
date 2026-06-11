pub mod cli;
pub mod config;
pub mod docker;
pub mod effects;
pub mod engine;
pub mod error;
pub mod lock;
pub mod lua;
pub mod redact;
pub mod signal;
pub mod state;
pub mod ui;

pub use error::{DcdError, Result};

pub fn run() -> Result<()> {
    cli::run()
}
