//! Adaptive reporter (spec §8.2): rich on a TTY, plain otherwise, `--json` on
//! request — one event stream, rendered per environment.

use std::cell::RefCell;
use std::io::IsTerminal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Rich,
    Plain,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Skip,
    Fail,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Skip => "skip",
            Status::Fail => "FAIL",
        }
    }
}

enum Sink {
    Stdout,
    Capture(RefCell<Vec<String>>),
}

pub struct Reporter {
    mode: Mode,
    sink: Sink,
}

impl Reporter {
    pub fn auto(json: bool) -> Reporter {
        let mode = if json {
            Mode::Json
        } else if std::io::stdout().is_terminal() {
            Mode::Rich
        } else {
            Mode::Plain
        };
        Reporter {
            mode,
            sink: Sink::Stdout,
        }
    }

    pub fn capture(mode: Mode) -> Reporter {
        Reporter {
            mode,
            sink: Sink::Capture(RefCell::new(Vec::new())),
        }
    }

    pub fn lines(&self) -> Vec<String> {
        match &self.sink {
            Sink::Capture(buf) => buf.borrow().clone(),
            Sink::Stdout => Vec::new(),
        }
    }

    pub fn task(&self, task: &str, status: Status, ms: u64, detail: Option<&str>) {
        let detail = detail.map(|d| d.to_string());
        let line = match self.mode {
            Mode::Json => {
                let value = serde_json::json!({
                    "task": task,
                    "status": status.label(),
                    "ms": ms,
                    "detail": detail,
                });
                value.to_string()
            }
            Mode::Rich => {
                let symbol = match status {
                    Status::Ok => "\u{25b6}",
                    Status::Skip => "\u{2013}",
                    Status::Fail => "\u{2717}",
                };
                format!(
                    "{symbol} {task:<16} {:<5} {:>5}ms {}",
                    status.label(),
                    ms,
                    detail.as_deref().unwrap_or("")
                )
                .trim_end()
                .to_string()
            }
            Mode::Plain => format!(
                "{task}: {} {}",
                status.label(),
                detail.as_deref().unwrap_or("")
            )
            .trim_end()
            .to_string(),
        };
        self.write(line);
    }

    pub fn log(&self, message: &str) {
        match self.mode {
            Mode::Json => self.write(serde_json::json!({ "log": message }).to_string()),
            _ => self.write(message.to_string()),
        }
    }

    pub fn warn(&self, message: &str) {
        match self.mode {
            Mode::Json => self.write(serde_json::json!({ "warn": message }).to_string()),
            _ => self.write(format!("\u{26a0} {message}")),
        }
    }

    pub fn plan(&self, line: &str) {
        match self.mode {
            Mode::Json => self.write(serde_json::json!({ "plan": line }).to_string()),
            _ => self.write(format!("  {line}")),
        }
    }

    fn write(&self, line: String) {
        match &self.sink {
            Sink::Stdout => println!("{line}"),
            Sink::Capture(buf) => buf.borrow_mut().push(line),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_mode_emits_parseable_events() {
        let r = Reporter::capture(Mode::Json);
        r.task("cutover", Status::Ok, 300, Some("nginx reloaded"));
        let line = &r.lines()[0];
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(value["task"], "cutover");
        assert_eq!(value["status"], "ok");
        assert_eq!(value["ms"], 300);
        assert_eq!(value["detail"], "nginx reloaded");
    }

    #[test]
    fn plain_mode_is_grep_friendly() {
        let r = Reporter::capture(Mode::Plain);
        r.task("healthcheck", Status::Ok, 6000, Some("3/60"));
        r.task("migrate:before", Status::Skip, 0, None);
        assert_eq!(r.lines(), vec!["healthcheck: ok 3/60", "migrate:before: skip"]);
    }
}
