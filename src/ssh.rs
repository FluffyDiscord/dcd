//! Pure `Argv` → `Argv` wrapping for the SSH transport (spec §2.7). Nothing here
//! executes; every command is unit-asserted, exactly like `docker.rs`.
//!
//! `ssh host -- a b c` does not `execve`: the target's shell re-parses the joined
//! arguments, so `Argv` stops being a syscall-level boundary and quoting becomes
//! the whole of the safety argument. That quoting lives in one function.

use std::path::{Path, PathBuf};

use crate::effects::Argv;

#[derive(Debug, Clone)]
pub struct SshTarget {
    target: String,
    control_path: PathBuf,
    config_file: Option<PathBuf>,
}

impl SshTarget {
    pub fn new(target: impl Into<String>, control_path: PathBuf) -> Self {
        SshTarget {
            target: target.into(),
            control_path,
            config_file: None,
        }
    }

    /// Point ssh at a specific config file instead of the operator's own. dcd
    /// deliberately does not model ports, keys or jump hosts (§2.7) — this is the
    /// escape hatch for a caller that must supply a whole ssh configuration.
    pub fn with_config_file(mut self, config_file: PathBuf) -> Self {
        self.config_file = Some(config_file);
        self
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    pub fn get_connect_timeout_seconds() -> u64 {
        10
    }

    pub fn get_server_alive_interval_seconds() -> u64 {
        15
    }

    pub fn get_server_alive_count_max() -> u64 {
        4
    }

    pub fn get_control_persist_seconds() -> u64 {
        60
    }

    /// The longest `ControlPath` a `sockaddr_un` can carry, NUL included, so 107
    /// bytes is the last usable length. Over it, ssh fails outright rather than
    /// degrading to an unmultiplexed connection.
    pub fn max_control_path_bytes() -> usize {
        107
    }

    /// `%C` is a SHA-1 hex digest of `%l%h%p%r` — 40 characters, measured with
    /// `ssh -G -o ControlPath=/tmp/cm-%C host` — so the path ssh actually binds is
    /// 38 bytes longer than the template dcd writes. Measuring the template is
    /// measuring the wrong string, in either direction: too small a constant misses
    /// the overflow, too large a one rejects paths that fit.
    pub fn expanded_control_path_bytes(&self) -> usize {
        const CONTROL_PATH_HASH_BYTES: usize = 40;
        let template = self.control_path.as_os_str().len();
        let tokens = self.control_path.display().to_string().matches("%C").count();
        template + tokens * (CONTROL_PATH_HASH_BYTES - "%C".len())
    }

    pub fn control_path_fits(&self) -> bool {
        self.expanded_control_path_bytes() <= SshTarget::max_control_path_bytes()
    }

    /// Splits a trailing `:port` off the target. OpenSSH takes the port as a flag,
    /// not as part of the destination, and an ssh_config `Host` alias cannot cover
    /// a target chosen at run time (`--ssh`).
    fn destination_and_port(&self) -> (String, Option<String>) {
        let Some((host, port)) = self.target.rsplit_once(':') else {
            return (self.target.clone(), None);
        };
        if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) {
            return (host.to_string(), Some(port.to_string()));
        }
        (self.target.clone(), None)
    }

    fn options(&self) -> Vec<String> {
        let mut options = Vec::new();
        if let Some(config) = &self.config_file {
            options.push("-F".to_string());
            options.push(config.display().to_string());
        }
        if let (_, Some(port)) = self.destination_and_port() {
            options.push("-p".to_string());
            options.push(port);
        }
        for (key, value) in [
            ("BatchMode", "yes".to_string()),
            ("ConnectTimeout", SshTarget::get_connect_timeout_seconds().to_string()),
            (
                "ServerAliveInterval",
                SshTarget::get_server_alive_interval_seconds().to_string(),
            ),
            (
                "ServerAliveCountMax",
                SshTarget::get_server_alive_count_max().to_string(),
            ),
            ("ControlMaster", "auto".to_string()),
            ("ControlPath", self.control_path.display().to_string()),
            (
                "ControlPersist",
                SshTarget::get_control_persist_seconds().to_string(),
            ),
        ] {
            options.push("-o".to_string());
            options.push(format!("{key}={value}"));
        }
        options
    }

    /// The remote invocation: `ssh <opts> <dest> -- sh`. The script itself travels
    /// on **stdin**, so the command line ssh hands to the target's login shell is a
    /// single bare word.
    ///
    /// This is the whole point. ssh joins everything after the destination and lets
    /// the *login shell* — which dcd does not choose, and which may be fish, csh or
    /// ksh with quoting rules of its own — parse it before `sh` ever sees it. Put a
    /// script there and it is parsed twice, by two different grammars. Put `sh`
    /// there and every shell agrees on what one bare word means; the script is then
    /// parsed exactly once, by the POSIX shell dcd asked for.
    pub fn invocation(&self) -> Argv {
        let mut argv = vec!["ssh".to_string()];
        argv.extend(self.options());
        argv.push(self.destination_and_port().0);
        argv.push("--".to_string());
        argv.push(POSIX_SHELL.to_string());
        Argv(argv)
    }

    /// A metacharacter-free invocation, for the lock holder alone: its stdin is a
    /// heartbeat channel, so it cannot also carry a script. Every argument is a
    /// bare word, which needs no quoting in any shell — `deploy_root` is validated
    /// to contain no whitespace or shell metacharacters for exactly this reason.
    pub fn bare_argv(&self, argv: &Argv) -> Argv {
        let mut wrapped = self.invocation();
        wrapped.0.pop(); // the lock runs its own program, not a shell
        wrapped.0.extend(argv.0.iter().cloned());
        wrapped
    }

    /// The script `sh` reads from stdin: the env document, then the command.
    /// Quoting here is parsed once, by the shell dcd named — never by the login
    /// shell — so `quote()` is the only grammar in play (INV-12).
    pub fn script_for(
        &self,
        argv: &Argv,
        deploy_root: Option<&Path>,
        env: &std::collections::BTreeMap<String, String>,
    ) -> Vec<u8> {
        let mut script = String::new();
        if !env.is_empty() {
            script.push_str("set -a\n");
            script.push_str(&env_document(env));
            script.push_str("set +a\n");
        }
        if let Some(root) = deploy_root {
            script.push_str(&format!("cd {} || exit 1\n", quote(&root.display().to_string())));
        }
        let command = argv.0.iter().map(|part| quote(part)).collect::<Vec<_>>().join(" ");
        script.push_str(&format!("exec {command}\n"));
        script.into_bytes()
    }

}

/// The shell dcd names on the wire. Not the login shell, and never inferred.
pub const POSIX_SHELL: &str = "sh";

/// POSIX single-quoting: everything inside `'…'` is literal, and an embedded
/// quote is closed, escaped, and reopened. Total — there is no byte it cannot carry.
/// Standard base64, so file bytes survive inside a shell script unchanged. The
/// alphabet contains no shell metacharacter, which is the point.
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let triple = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        for shift in [18, 12, 6, 0] {
            out.push(ALPHABET[((triple >> shift) & 0x3f) as usize] as char);
        }
        let padding = 3 - chunk.len();
        out.truncate(out.len() - padding);
        out.push_str(&"=".repeat(padding));
    }
    out
}

/// The one way dcd writes a file on the target.
///
/// The payload is EMBEDDED in the script, never left for the remote shell to read
/// from its own stdin: `sh` buffers the whole stream while reading the script, so
/// a `cat > file` inside it sees EOF and writes nothing. Staged and renamed,
/// always — a torn write of the state file fails every later command on the
/// stage, `unlock` included.
pub fn write_file_script(path: &str, bytes: &[u8], mode: Option<u32>) -> String {
    let target = quote(path);
    let staged = quote(&format!("{path}.tmp.{}", std::process::id()));
    let encoded = base64(bytes);
    let chmod = match mode {
        Some(mode) => format!("chmod {mode:o} {staged} && "),
        None => String::new(),
    };
    format!("printf %s '{encoded}' | base64 -d > {staged} && {chmod}mv {staged} {target}\n")
}

pub fn quote(raw: &str) -> String {
    if !raw.is_empty() && raw.bytes().all(is_shell_safe) {
        return raw.to_string();
    }
    format!("'{}'", raw.replace('\'', r"'\''"))
}

fn is_shell_safe(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'/' | b':' | b'=' | b',' | b'@' | b'+')
}

/// Renders the delivered environment as a document to source. Values never reach
/// an argv, so `ps` on either machine shows nothing (INV-12).
pub fn env_document(env: &std::collections::BTreeMap<String, String>) -> String {
    let mut document = String::new();
    for (key, value) in env {
        document.push_str(key);
        document.push('=');
        document.push_str(&quote(value));
        document.push('\n');
    }
    document
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::process::{Command, Stdio};

    fn target() -> SshTarget {
        SshTarget::new("deploy@prod", PathBuf::from("/run/dcd/cm-%C"))
    }

    /// Runs the script dcd would put on stdin through a REAL shell, exactly as the
    /// remote `sh` will, and returns what the command actually received.
    fn through(shell: &str, script: &[u8]) -> Option<String> {
        let mut child = Command::new(shell)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        child.stdin.as_mut()?.write_all(script).ok()?;
        let out = child.wait_with_output().ok()?;
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Shells a server might give the deploy user. Absent ones are skipped rather
    /// than failing the suite: this asserts compatibility where it can be observed.
    fn candidate_shells() -> Vec<&'static str> {
        // Only shells invocable as `<shell>` reading a script on stdin. Login shells
        // that need a subcommand (busybox) or a different grammar (fish, tcsh) are
        // covered by tests/ssh_shells.rs, against a real sshd.
        ["/bin/sh", "/bin/dash", "/bin/bash", "/bin/zsh", "/bin/ksh", "/bin/mksh", "/bin/yash"]
            .into_iter()
            .filter(|shell| std::path::Path::new(shell).exists())
            .collect()
    }

    fn hostile_payloads() -> Vec<&'static str> {
        vec![
            "plain",
            " ",
            "with space",
            "$HOME",
            "$(id)",
            "`id`",
            "a'b",
            "'; rm -rf /; '",
            "double\"quote",
            "semi;colon && echo pwned",
            "pipe | echo pwned",
            "back\\slash",
            "new\nline",
            "tab\there",
            "glob * ? [a-z] ~",
            "!bang",
            "héllo-ünïcode",
            "--flag",
            "}brace{",
        ]
    }

    /// The whole reason the script travels on stdin: ssh joins everything after the
    /// destination and hands it to the target's LOGIN shell — which dcd does not
    /// choose, and which may be fish, csh or ksh with quoting rules of its own.
    /// One bare word means the same thing in every one of them.
    #[test]
    fn the_login_shell_only_ever_sees_one_bare_word() {
        let argv = target().invocation();
        assert_eq!(argv.0.last().map(String::as_str), Some("sh"));
        assert_eq!(argv.0[argv.0.len() - 2], "--");

        let after_destination = &argv.0[argv.0.len() - 1];
        assert!(
            after_destination.chars().all(|c| c.is_ascii_alphanumeric()),
            "the remote command line must need no quoting in ANY shell, got {after_destination:?}"
        );
    }

    /// Every payload must reach the command byte-identically, through every shell a
    /// server might actually run.
    #[test]
    fn payloads_round_trip_through_every_available_shell() {
        let shells = candidate_shells();
        assert!(!shells.is_empty(), "no shell to test against");

        for shell in &shells {
            for payload in hostile_payloads() {
                let script = target().script_for(
                    &Argv::of(["printf", "%s", payload]),
                    None,
                    &BTreeMap::new(),
                );
                let Some(got) = through(shell, &script) else { continue };
                assert_eq!(got, payload, "{shell} mangled {payload:?}");
            }
        }
    }

    /// The regression that killed every ssh deploy: the lease script was appended
    /// to the script the remote `sh` reads from its own stdin, for a `cat >` inside
    /// it to pick up. sh buffers the whole stream, so `cat` saw EOF and the file
    /// landed EMPTY — an empty lease exits 0 without taking the lock, and dcd read
    /// that as "another deploy holds the stage". Writing through a real shell, with
    /// the script on stdin, is what proves the payload no longer competes with it.
    #[test]
    fn a_file_written_on_the_target_survives_the_script_travelling_on_stdin() {
        let shells = candidate_shells();
        assert!(!shells.is_empty(), "no shell to test against");

        let contents = "echo dcd-lock-acquired\nwhile read -t 30 _; do :; done\n";
        for shell in &shells {
            let directory = std::env::temp_dir().join(format!("dcd-write-{}-{}", std::process::id(), shell.replace('/', "_")));
            std::fs::create_dir_all(&directory).expect("scratch dir");
            let path = directory.join("lease.sh");

            let script = write_file_script(&path.display().to_string(), contents.as_bytes(), None);
            let Some(_) = through(shell, script.as_bytes()) else { continue };

            let written = std::fs::read_to_string(&path).unwrap_or_default();
            assert_eq!(written, contents, "{shell} wrote the wrong bytes");
            let _ = std::fs::remove_dir_all(&directory);
        }
    }

    /// Binary content and shell metacharacters both have to survive, since the same
    /// builder writes `dcd-state.json` and the uploaded compose documents.
    #[test]
    fn a_written_file_is_staged_and_never_exposes_its_bytes_to_the_shell() {
        let script = write_file_script("/srv/app/dcd-state.json", b"{\"quote\":\"a'b $(id) `id`\"}", Some(0o600));
        assert!(!script.contains("$(id)"), "content must not reach the shell as text: {script}");
        assert!(script.contains("chmod 600"), "mode must be applied before the rename: {script}");
        assert!(script.contains(".tmp."), "the write must be staged: {script}");
        assert!(script.contains("mv "), "the write must be renamed into place: {script}");
    }

    /// INV-12 through a real shell: the value reaches the command's environment,
    /// and never appears in any argv.
    #[test]
    fn env_values_arrive_through_the_script_and_never_through_argv() {
        let secret = "hunter2 $(id) `id` '\"quoted\"'";
        let mut env = BTreeMap::new();
        env.insert("APP_SECRET".to_string(), secret.to_string());

        let argv = Argv::of(["sh", "-c", "printf %s \"$APP_SECRET\""]);
        let script = target().script_for(&argv, None, &env);

        assert!(
            !target().invocation().display().contains(secret),
            "no value may reach the ssh argv"
        );
        for shell in candidate_shells() {
            let Some(got) = through(shell, &script) else { continue };
            assert_eq!(got, secret, "{shell} did not deliver the value intact");
        }
    }

    /// `cd` failing must abort rather than silently running the command in the
    /// wrong directory — a deploy against `$HOME` instead of `deploy_root`.
    #[test]
    fn a_missing_deploy_root_aborts_instead_of_running_elsewhere() {
        let script = target().script_for(
            &Argv::of(["printf", "%s", "ran-anyway"]),
            Some(Path::new("/definitely/not/here")),
            &BTreeMap::new(),
        );
        for shell in candidate_shells() {
            let Some(got) = through(shell, &script) else { continue };
            assert_eq!(got, "", "{shell} ran the command despite a failed cd");
        }
    }

    #[test]
    fn quoting_is_total_against_hostile_input() {
        for payload in hostile_payloads() {
            let script = format!("printf %s {}\n", quote(payload));
            let got = through("/bin/sh", script.as_bytes()).expect("sh runs");
            assert_eq!(got, payload, "quoting lost bytes for {payload:?}");
        }
    }

    #[test]
    fn a_four_kilobyte_argument_survives() {
        let long = "a$b'c ".repeat(700);
        let script = format!("printf %s {}\n", quote(&long));
        assert_eq!(through("/bin/sh", script.as_bytes()).unwrap(), long);
    }

    /// The lock cannot carry a script — its stdin is the heartbeat channel — so it
    /// is invoked as bare words instead. Nothing in that argv may need quoting.
    #[test]
    fn the_bare_lock_invocation_contains_no_metacharacters() {
        let argv = target().bare_argv(&Argv::of(["flock", "-n", "/srv/acme/.dcd.prod.lock", "sh", "/srv/acme/.dcd.prod.lease.sh"]));
        let rendered = argv.display();
        assert!(rendered.ends_with("-- flock -n /srv/acme/.dcd.prod.lock sh /srv/acme/.dcd.prod.lease.sh"));
        assert!(!rendered.contains('\''), "a quote here would need a login-shell grammar: {rendered}");
        assert!(!rendered.contains("sh -c"));
    }

    /// StrictHostKeyChecking is deliberately left at OpenSSH's default: defaulting
    /// it to `accept-new` would be silent TOFU into production.
    #[test]
    fn host_key_checking_is_never_relaxed() {
        assert!(!target().invocation().display().contains("StrictHostKeyChecking"));
    }

    #[test]
    fn a_control_path_over_the_socket_limit_is_rejected() {
        assert!(target().control_path_fits());
        let long = SshTarget::new("h", PathBuf::from(format!("/{}/cm-%C", "x".repeat(120))));
        assert!(!long.control_path_fits(), "an over-long ControlPath must be caught before ssh fails");

        // `%C` is 2 template bytes standing in for a 40-character digest, so the
        // socket ssh binds is 38 bytes longer than what dcd writes. Measuring the
        // template alone lets an over-long path through; over-stating the digest
        // rejects paths that work.
        let template = SshTarget::new("h", PathBuf::from("/run/dcd/cm-%C"));
        assert_eq!(
            template.expanded_control_path_bytes(),
            template.control_path.as_os_str().len() + 38
        );

        let fits = SshTarget::new("h", PathBuf::from(format!("/{}/cm-%C", "x".repeat(60))));
        assert_eq!(fits.expanded_control_path_bytes(), 105);
        assert!(fits.control_path_fits(), "105 bytes is under the limit and must be accepted");

        let overflows = SshTarget::new("h", PathBuf::from(format!("/{}/cm-%C", "x".repeat(63))));
        assert_eq!(overflows.expanded_control_path_bytes(), 108);
        assert!(!overflows.control_path_fits(), "108 bytes cannot fit a 107-byte sun_path");
    }



    /// OpenSSH takes the port as a flag; a target chosen at run time cannot rely
    /// on an ssh_config Host alias to carry it.
    #[test]
    fn a_trailing_port_becomes_a_flag_not_part_of_the_destination() {
        let target = SshTarget::new("deploy@127.0.0.1:2222", PathBuf::from("/run/dcd/cm"));
        let rendered = target.invocation().display();
        assert!(rendered.contains("-p 2222"), "got: {rendered}");
        assert!(rendered.contains(" deploy@127.0.0.1 -- "), "got: {rendered}");
        assert!(!rendered.contains("127.0.0.1:2222"));
    }

    /// An IPv6 literal or a Host alias containing a colon must not be mistaken for
    /// a port.
    #[test]
    fn a_non_numeric_suffix_is_left_alone() {
        let alias = SshTarget::new("prod-alias", PathBuf::from("/run/dcd/cm"));
        assert!(alias.invocation().display().contains(" prod-alias -- "));
        let weird = SshTarget::new("host:notaport", PathBuf::from("/run/dcd/cm"));
        assert!(weird.invocation().display().contains(" host:notaport -- "));
    }
}
