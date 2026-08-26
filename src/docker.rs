//! Builds the exact `docker` / `docker compose` argument vectors of spec §7.
//! Pure (no execution) so every command is unit-asserted; the recipe pairs each
//! with its `Access` (read vs mutate) at the call site.

use crate::config::Config;
use crate::effects::Argv;

/// `docker ps --filter name=` is an unanchored regex, so a prefix has to be
/// escaped as well as anchored or a decoy container is swept by `docker rm -f`.
fn regex_escape(raw: &str) -> String {
    let mut escaped = String::with_capacity(raw.len());
    for character in raw.chars() {
        if "\\.+*?()|[]{}^$".contains(character) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

pub struct Docker<'a> {
    cfg: &'a Config,
}

impl<'a> Docker<'a> {
    pub fn new(cfg: &'a Config) -> Self {
        Docker { cfg }
    }

    pub fn pull(&self, image: &str) -> Argv {
        Argv::of(["docker", "pull", image])
    }

    pub fn network_inspect(&self, network: &str) -> Argv {
        Argv::of(["docker", "network", "inspect", network])
    }

    pub fn inspect_image(&self, container: &str) -> Argv {
        Argv::of(["docker", "inspect", container, "--format", "{{.Config.Image}}"])
    }

    /// The container's own health, as Docker reports it. `{{if .State.Health}}`
    /// guards the common case of an image with no healthcheck at all, which would
    /// otherwise blow up the template.
    pub fn inspect_health(&self, container: &str) -> Argv {
        Argv::of([
            "docker",
            "inspect",
            container,
            "--format",
            "{{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}",
        ])
    }

    pub fn exec_sh(&self, container: &str, cmd: &str) -> Argv {
        Argv::of(["docker", "exec", container, "sh", "-c", cmd])
    }

    pub fn exec_args(&self, container: &str, args: &[String]) -> Argv {
        let mut argv = vec!["docker".into(), "exec".into(), container.to_string()];
        argv.extend(args.iter().cloned());
        Argv(argv)
    }

    pub fn rm_f(&self, container: &str) -> Argv {
        Argv::of(["docker", "rm", "-f", container])
    }

    pub fn stop(&self, containers: &[String], timeout_secs: u64, signal: &str) -> Argv {
        let mut argv = vec![
            "docker".into(),
            "stop".into(),
            "--signal".into(),
            signal.to_string(),
            "--timeout".into(),
            timeout_secs.to_string(),
        ];
        argv.extend(containers.iter().cloned());
        Argv(argv)
    }

    pub fn cp(&self, src: &str, dst: &str) -> Argv {
        Argv::of(["docker", "cp", src, dst])
    }

    pub fn image_rm(&self, tag: &str) -> Argv {
        Argv::of(["docker", "image", "rm", tag])
    }

    /// Tagged images in one repository. `<none>` tags are filtered by the caller —
    /// removing by ID would untag the same layers in foreign repositories too.
    pub fn images_in(&self, repository: &str) -> Argv {
        Argv::of(["docker", "images", repository, "--format", "{{.Repository}}:{{.Tag}}"])
    }

    /// What every container on the host runs, stopped ones included: a stopped
    /// container still pins its image, and its release may still be a rollback target.
    pub fn container_images(&self) -> Argv {
        Argv::of(["docker", "ps", "-a", "--format", "{{.Image}}"])
    }

    pub fn ps_names(&self, name_prefix: &str, include_stopped: bool) -> Argv {
        let mut argv = vec!["docker".to_string(), "ps".to_string()];
        if include_stopped {
            argv.push("-a".into());
        }
        argv.push("--filter".into());
        argv.push(format!("name=^{}", regex_escape(name_prefix)));
        argv.push("--format".into());
        argv.push("{{.Names}}".into());
        Argv(argv)
    }

    /// Containers matching a name prefix, with the compose service each belongs to.
    /// Used once, to find v1 workers whose per-worker service labels v2's discovery
    /// filter can never match.
    pub fn labelled_ps_names(&self, name_prefix: &str) -> Argv {
        Argv::of([
            "docker",
            "ps",
            "-a",
            "--filter",
            &format!("label=com.docker.compose.project={}", self.cfg.project),
            "--filter",
            &format!("name=^{}", regex_escape(name_prefix)),
            "--format",
            "{{.Names}} {{.Label \"com.docker.compose.service\"}}",
        ])
    }

    /// INV-14: discovery is by compose SERVICE label, never a name prefix. Under
    /// ADR-013 the release container carries the project label too, so a prefix
    /// filter could match it — and the caller `docker stop`s everything returned.
    pub fn worker_ps_names(&self, worker_service: &str) -> Argv {
        Argv::of([
            "docker",
            "ps",
            "--filter",
            &format!("label=com.docker.compose.project={}", self.cfg.project),
            "--filter",
            &format!("label=com.docker.compose.service={worker_service}"),
            "--format",
            "{{.Names}}",
        ])
    }

    pub fn is_running(&self, container: &str) -> Argv {
        let filter = format!("name=^{container}$");
        Argv::of(["docker", "ps", "-q", "--filter", &filter])
    }

    /// `docker compose -p <project> --env-file /dev/null -f <each file> [-f <workers>] <args>`
    /// — always fully qualified so the project name is never inferred (spec §9). The
    /// `/dev/null` env-file pins compose's implicit `.env` discovery OFF (spec §5.2.4);
    /// env arrives via the process environment instead.
    pub fn compose(&self, args: &[&str]) -> Argv {
        let mut argv = vec![
            "docker".to_string(),
            "compose".into(),
            "-p".into(),
            self.cfg.project.clone(),
            "--env-file".into(),
            "/dev/null".into(),
        ];
        for file in &self.cfg.compose.files {
            argv.push("-f".into());
            argv.push(file.display().to_string());
        }
        argv.push("-f".into());
        argv.push(self.image_override_file());
        argv.extend(args.iter().map(|s| s.to_string()));
        Argv(argv)
    }

    /// Written on every run, local or remote: `compose(...)` passes it
    /// unconditionally, so a missing file would break every command (spec §7.0).
    pub fn image_override_file(&self) -> String {
        format!("dcd-image-override.{}.yml", self.cfg.stage)
    }

    /// The release container (spec §7.6). `compose run` is a *creation* primitive
    /// only — everything afterwards addresses the result by name through plain
    /// `docker` (INV-13). `--use-aliases` carries the service's declared network
    /// aliases, which is the v1 `network_alias` feature for free.
    pub fn run_black(&self, container: &str, service: &str, env_keys: &[String]) -> Argv {
        let mut args = vec![
            "run".to_string(),
            "-d".into(),
            "--name".into(),
            container.to_string(),
            "--use-aliases".into(),
            "--no-deps".into(),
        ];
        for key in env_keys {
            args.push("-e".into());
            args.push(key.clone());
        }
        args.push(service.to_string());
        self.compose(&args.iter().map(String::as_str).collect::<Vec<_>>())
    }

    /// Compose forces `restart=no` on one-off containers, so the policy the
    /// operator declared has to be re-applied or reboot-restart silently breaks.
    pub fn update_restart(&self, container: &str, policy: &str) -> Argv {
        Argv::of(["docker", "update", "--restart", policy, container])
    }

    /// One container per worker name, from ONE compose service (spec §7.12).
    /// Trailing words override the service's `command`, keeping its entrypoint.
    pub fn run_worker(&self, container: &str, service: &str, name: &str, extra_args: &[String], env_keys: &[String]) -> Argv {
        let mut args = vec![
            "run".to_string(),
            "-d".into(),
            "--name".into(),
            container.to_string(),
            "--no-deps".into(),
        ];
        for key in env_keys {
            args.push("-e".into());
            args.push(key.clone());
        }
        args.push(service.to_string());
        args.push(name.to_string());
        args.extend(extra_args.iter().cloned());
        self.compose(&args.iter().map(String::as_str).collect::<Vec<_>>())
    }

    /// `migrate:before` (spec §7.5). `-T` is mandatory — compose's `run` is
    /// interactive by default and stdin is already carrying the env document —
    /// and `--entrypoint` is mandatory because the trailing words would otherwise
    /// be handed to the service's own entrypoint as arguments.
    pub fn run_throwaway(&self, service: &str, command: &[String], env_keys: &[String]) -> Argv {
        let mut args = vec!["run".to_string(), "--rm".into(), "-T".into(), "--no-deps".into()];
        if let Some((program, _)) = command.split_first() {
            args.push("--entrypoint".into());
            args.push(program.clone());
        }
        for key in env_keys {
            args.push("-e".into());
            args.push(key.clone());
        }
        args.push(service.to_string());
        args.extend(command.iter().skip(1).cloned());
        self.compose(&args.iter().map(String::as_str).collect::<Vec<_>>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn config() -> Config {
        let src = r#"
version: 2
project: demo
compose:
  files: [base.yml, extra.yml]
release:
  service: app
  container_prefix: demo-app
  healthcheck: { exec_in: nginx, cmd: 'curl {container}' }
cutover:
  service: nginx
  backend_port: 8080
  reload: { exec_in: nginx, cmd: 'nginx -s reload' }
services:
  nginx: { recreate: never }
workers:
  service: worker
  provider: { static: [async] }
stages:
  prod: {}
"#;
        crate::config::load(src, None, &[], &HashMap::new()).unwrap()
    }

    fn keys(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// The override file is appended unconditionally, so it must be written in
    /// both local and remote mode or every compose call breaks (spec §7.0).
    #[test]
    fn compose_is_fully_qualified_pins_dotenv_off_and_ends_with_the_override() {
        let cfg = config();
        let d = Docker::new(&cfg);
        assert_eq!(
            d.compose(&["up", "-d", "nginx"]).display(),
            "docker compose -p demo --env-file /dev/null -f base.yml -f extra.yml -f dcd-image-override.prod.yml up -d nginx"
        );
    }

    /// §7.6: `compose run` is a creation primitive; `--use-aliases` carries the
    /// service's declared aliases, and values never enter the argv (INV-12).
    #[test]
    fn run_black_creates_the_release_from_its_compose_service() {
        let cfg = config();
        let d = Docker::new(&cfg);
        assert_eq!(
            d.run_black("demo-app-42", "app", &keys(&["DATABASE_URL", "TZ"])).display(),
            "docker compose -p demo --env-file /dev/null -f base.yml -f extra.yml -f dcd-image-override.prod.yml run -d --name demo-app-42 --use-aliases --no-deps -e DATABASE_URL -e TZ app"
        );
    }

    /// Compose forces `restart=no` on one-off containers; skipping this silently
    /// breaks reboot-restart.
    #[test]
    fn the_restart_policy_is_reapplied_after_creation() {
        let cfg = config();
        assert_eq!(
            Docker::new(&cfg).update_restart("demo-app-42", "unless-stopped").display(),
            "docker update --restart unless-stopped demo-app-42"
        );
    }

    /// §7.5: `-T` because stdin already carries the env document, `--entrypoint`
    /// because the trailing words would otherwise be arguments to the service's
    /// own entrypoint.
    #[test]
    fn migrate_before_overrides_the_entrypoint_and_disables_tty() {
        let cfg = config();
        let command = keys(&["php", "bin/console", "app:db:migrate", "before"]);
        assert_eq!(
            Docker::new(&cfg).run_throwaway("app", &command, &keys(&["TZ"])).display(),
            "docker compose -p demo --env-file /dev/null -f base.yml -f extra.yml -f dcd-image-override.prod.yml run --rm -T --no-deps --entrypoint php -e TZ app bin/console app:db:migrate before"
        );
    }

    /// §7.12: N containers from ONE service; trailing words override the
    /// service's `command` while keeping its entrypoint.
    #[test]
    fn each_worker_is_one_container_from_the_same_service() {
        let cfg = config();
        let d = Docker::new(&cfg);
        assert_eq!(
            d.run_worker("worker-async", "worker", "async", &keys(&["--limit=100"]), &keys(&["TZ"])).display(),
            "docker compose -p demo --env-file /dev/null -f base.yml -f extra.yml -f dcd-image-override.prod.yml run -d --name worker-async --no-deps -e TZ worker async --limit=100"
        );
    }

    /// INV-14: discovery is by compose SERVICE label. A name-prefix filter would
    /// match the release container, which the caller then `docker stop`s.
    #[test]
    fn worker_discovery_filters_on_the_service_label_never_a_name() {
        let cfg = config();
        let rendered = Docker::new(&cfg).worker_ps_names("worker").display();
        assert!(rendered.contains("label=com.docker.compose.service=worker"), "got: {rendered}");
        assert!(rendered.contains("label=com.docker.compose.project=demo"));
        assert!(!rendered.contains("name="), "a name filter would sweep the release container: {rendered}");
    }

    #[test]
    fn stopping_workers_carries_the_configured_signal_and_timeout() {
        let cfg = config();
        assert_eq!(
            Docker::new(&cfg).stop(&keys(&["worker-async"]), 120, "SIGTERM").display(),
            "docker stop --signal SIGTERM --timeout 120 worker-async"
        );
    }

    /// `--filter name=` is an unanchored regex, so a decoy container would be
    /// swept by the orphan reaper without the anchor and the escaping.
    #[test]
    fn name_filters_are_anchored_and_regex_escaped() {
        let cfg = config();
        let rendered = Docker::new(&cfg).ps_names("demo-app.v2", true).display();
        assert!(rendered.contains(r"name=^demo-app\.v2"), "got: {rendered}");
    }

    /// An image with no healthcheck must report `none`, not blow up the template.
    #[test]
    fn health_inspection_tolerates_an_image_without_a_healthcheck() {
        let cfg = config();
        let rendered = Docker::new(&cfg).inspect_health("demo-app-42").display();
        assert!(rendered.contains("{{if .State.Health}}"), "got: {rendered}");
        assert!(rendered.contains("{{else}}none{{end}}"));
    }
}
