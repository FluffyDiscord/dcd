//! Builds the exact `docker` / `docker compose` argument vectors of spec §7.
//! Pure (no execution) so every command is unit-asserted; the recipe pairs each
//! with its `Access` (read vs mutate) at the call site.

use crate::config::Config;
use crate::effects::Argv;

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

    pub fn network_inspect(&self) -> Argv {
        Argv::of(["docker", "network", "inspect", &self.cfg.network])
    }

    pub fn network_create(&self) -> Argv {
        Argv::of(["docker", "network", "create", &self.cfg.network])
    }

    pub fn inspect_image(&self, container: &str) -> Argv {
        Argv::of(["docker", "inspect", container, "--format", "{{.Config.Image}}"])
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

    pub fn stop(&self, containers: &[String], timeout_secs: u64) -> Argv {
        let mut argv = vec![
            "docker".into(),
            "stop".into(),
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

    pub fn ps_names(&self, name_prefix: &str, include_stopped: bool) -> Argv {
        let mut argv = vec!["docker".to_string(), "ps".to_string()];
        if include_stopped {
            argv.push("-a".into());
        }
        argv.push("--filter".into());
        argv.push(format!("name=^{name_prefix}"));
        argv.push("--format".into());
        argv.push("{{.Names}}".into());
        Argv(argv)
    }

    pub fn worker_ps_names(&self, name_filter: &str) -> Argv {
        Argv::of([
            "docker",
            "ps",
            "--filter",
            &format!("label=com.docker.compose.project={}", self.cfg.project),
            "--filter",
            &format!("name={name_filter}"),
            "--format",
            "{{.Names}}",
        ])
    }

    pub fn is_running(&self, container: &str) -> Argv {
        let filter = format!("name=^{container}$");
        Argv::of(["docker", "ps", "-q", "--filter", &filter])
    }

    /// `docker compose -p <project> --env-file <env> -f <each file> [-f <workers>] <args>`
    /// — always fully qualified so the project name is never inferred (spec §9).
    pub fn compose(&self, args: &[&str], include_workers: bool) -> Argv {
        let mut argv = vec![
            "docker".to_string(),
            "compose".into(),
            "-p".into(),
            self.cfg.project.clone(),
            "--env-file".into(),
            self.cfg.compose.env_file.display().to_string(),
        ];
        for file in &self.cfg.compose.files {
            argv.push("-f".into());
            argv.push(file.display().to_string());
        }
        if include_workers {
            if let Some(workers) = &self.cfg.workers {
                argv.push("-f".into());
                argv.push(workers.compose_file.display().to_string());
            }
        }
        argv.extend(args.iter().map(|s| s.to_string()));
        Argv(argv)
    }

    pub fn run_black(&self, container: &str, image: &str) -> Argv {
        let run = &self.cfg.release.run;
        let mut argv = vec![
            "docker".to_string(),
            "run".into(),
            "-d".into(),
            "--name".into(),
            container.to_string(),
            "--network".into(),
            self.cfg.network.clone(),
        ];
        if let Some(alias) = &run.network_alias {
            argv.push("--network-alias".into());
            argv.push(alias.clone());
        }
        argv.push("--restart".into());
        argv.push(run.restart.clone());
        for (key, value) in &run.env {
            argv.push("-e".into());
            argv.push(format!("{key}={value}"));
        }
        for volume in &run.volumes {
            argv.push("-v".into());
            argv.push(volume.clone());
        }
        argv.push(image.to_string());
        Argv(argv)
    }

    pub fn run_throwaway(&self, name: &str, image: &str, command: &[String]) -> Argv {
        let mut argv = vec![
            "docker".to_string(),
            "run".into(),
            "--rm".into(),
            "--network".into(),
            self.cfg.network.clone(),
            "--name".into(),
            name.to_string(),
            image.to_string(),
        ];
        argv.extend(command.iter().cloned());
        Argv(argv)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn config() -> Config {
        let src = r#"
version: 1
project: demo
network: demo_net
images:
  app: app-1
compose:
  files: [base.yml, extra.yml]
  env_file: compose.env
release:
  image: app
  container_prefix: demo-app
  run:
    network_alias: app-rr
    env: { TZ: UTC }
    volumes: ['/host:/ctr']
  healthcheck: { exec_in: demo-nginx, cmd: 'curl {container}' }
cutover:
  backend_port: 8080
  reload: { exec_in: demo-nginx, cmd: 'nginx -s reload' }
services:
  nginx: { container: demo-nginx, recreate: never }
workers:
  compose_file: workers.yml
  provider: { static: [async] }
  template: { image: app, entrypoint: [php], command: ['{name}'] }
"#;
        crate::config::load(src, None, &[], &HashMap::new()).unwrap().config
    }

    #[test]
    fn compose_is_always_fully_qualified() {
        let cfg = config();
        let d = Docker::new(&cfg);
        assert_eq!(
            d.compose(&["up", "-d", "nginx"], false).display(),
            "docker compose -p demo --env-file compose.env -f base.yml -f extra.yml up -d nginx"
        );
        assert_eq!(
            d.compose(&["up", "-d"], true).display(),
            "docker compose -p demo --env-file compose.env -f base.yml -f extra.yml -f workers.yml up -d"
        );
    }

    #[test]
    fn run_black_builds_full_argv() {
        let cfg = config();
        let d = Docker::new(&cfg);
        assert_eq!(
            d.run_black("demo-app-42", "reg/app:app-1").display(),
            "docker run -d --name demo-app-42 --network demo_net --network-alias app-rr --restart unless-stopped -e TZ=UTC -v /host:/ctr reg/app:app-1"
        );
    }

    #[test]
    fn throwaway_runs_command_as_argv_no_shell() {
        let cfg = config();
        let d = Docker::new(&cfg);
        let cmd: Vec<String> = ["php", "bin/console", "app:db:migrate", "before"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            d.run_throwaway("demo-migrate-42", "reg/app:app-1", &cmd).display(),
            "docker run --rm --network demo_net --name demo-migrate-42 reg/app:app-1 php bin/console app:db:migrate before"
        );
    }

    #[test]
    fn worker_discovery_uses_project_label_and_name_filter() {
        let cfg = config();
        let d = Docker::new(&cfg);
        assert_eq!(
            d.worker_ps_names("worker-").display(),
            "docker ps --filter label=com.docker.compose.project=demo --filter name=worker- --format {{.Names}}"
        );
    }

    #[test]
    fn stop_and_inspect_shapes() {
        let cfg = config();
        let d = Docker::new(&cfg);
        assert_eq!(
            d.stop(&["worker-a".into(), "worker-b".into()], 120).display(),
            "docker stop --timeout 120 worker-a worker-b"
        );
        assert_eq!(
            d.inspect_image("demo-postgres").display(),
            "docker inspect demo-postgres --format {{.Config.Image}}"
        );
    }
}
