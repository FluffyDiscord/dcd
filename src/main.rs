use std::process::ExitCode;

fn main() -> ExitCode {
    match dcd::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("dcd: {err}");
            ExitCode::from(err.exit_code())
        }
    }
}
