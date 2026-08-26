//! `dcd init --from-compose` (spec §8.4): a config derived from the compose file
//! the operator already has.
//!
//! The compose document is read as YAML, not resolved through `docker compose
//! config` — an unresolved `${APP_TAG}` is irrelevant to service names, ports and
//! healthchecks, and a scaffold that needs a working environment and a daemon to
//! run is a scaffold nobody can run first.
//!
//! Guesses are marked. `TODO:` appears only on the fields that are genuinely
//! red-black-specific and cannot be inferred from a compose file.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{DcdError, Result};

#[derive(Debug, Deserialize)]
struct ComposeDocument {
    #[serde(default)]
    services: indexmap::IndexMap<String, ComposeServiceDocument>,
}

#[derive(Debug, Default, Deserialize)]
struct ComposeServiceDocument {
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    ports: Vec<serde_yaml::Value>,
    #[serde(default)]
    healthcheck: Option<serde_yaml::Value>,
    #[serde(default)]
    profiles: Vec<String>,
}

/// Images whose presence marks a service as the traffic router. Matched on the
/// repository part of the reference, so `registry.example.com/nginx:1.27` counts.
fn router_images() -> &'static [&'static str] {
    &["nginx", "traefik", "caddy", "haproxy", "envoy", "openresty"]
}

/// Service names that mark the router when its image cannot: a router built in CI
/// is `${REGISTRY}:${ROUTER_TAG}`, which names nothing.
fn router_service_names() -> &'static [&'static str] {
    &["router", "nginx", "proxy", "traefik", "caddy", "haproxy", "gateway", "ingress"]
}

/// Service names that name the app in nearly every compose file in the wild.
fn app_service_names() -> &'static [&'static str] {
    &["app", "web", "api", "php", "backend"]
}

#[derive(Debug)]
pub struct Scaffold {
    document: ComposeDocument,
    compose_file: String,
    project: String,
}

impl Scaffold {
    /// `config_path` is where the generated `dcd.yaml` will land: it decides both
    /// the project name and how the compose file is addressed, since `compose.files`
    /// is resolved relative to the config, not to wherever the compose file was
    /// found.
    pub fn from_compose(path: &Path, config_path: &Path) -> Result<Scaffold> {
        let source = std::fs::read_to_string(path)
            .map_err(|e| DcdError::Config(format!("cannot read {}: {e}", path.display())))?;
        let document: ComposeDocument = serde_yaml::from_str(&source)
            .map_err(|e| DcdError::Config(format!("{} is not a compose file: {e}", path.display())))?;
        if document.services.is_empty() {
            return Err(DcdError::Config(format!("{} declares no services", path.display())));
        }

        let project = Scaffold::project_name(config_path);
        let compose_file = Scaffold::compose_reference(path, config_path);

        Ok(Scaffold { document, compose_file, project })
    }

    /// How the config must address the compose file: relative to the config's own
    /// directory when it sits under it, absolute otherwise. Emitting only the file
    /// name loses `infra/` and produces a config that cannot resolve.
    fn compose_reference(compose: &Path, config_path: &Path) -> String {
        let config_directory = Scaffold::directory_of(config_path);
        let absolute_compose = compose.canonicalize().unwrap_or_else(|_| compose.to_path_buf());
        let absolute_config = config_directory.canonicalize().unwrap_or(config_directory);
        if let Ok(below) = absolute_compose.strip_prefix(&absolute_config) {
            return below.display().to_string();
        }
        // A compose file one level up is an ordinary layout. Baking the absolute
        // path into a committed config would make it resolve on one machine only.
        let shared = absolute_config
            .ancestors()
            .find(|ancestor| absolute_compose.starts_with(ancestor));
        let climb = shared.and_then(|shared| {
            let up = absolute_config.strip_prefix(shared).ok()?.components().count();
            let down = absolute_compose.strip_prefix(shared).ok()?;
            Some(std::iter::repeat_n("..", up).collect::<PathBuf>().join(down))
        });
        climb.unwrap_or(absolute_compose).display().to_string()
    }

    fn directory_of(path: &Path) -> std::path::PathBuf {
        match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => std::path::PathBuf::from("."),
        }
    }

    /// The directory the generated config will live in. `project:` has no
    /// directory-derived default in the loader, so the scaffold has to write one.
    fn project_name(config_path: &Path) -> String {
        let directory = Scaffold::directory_of(config_path).canonicalize().ok();
        let name = directory
            .as_deref()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "app".to_string());
        let cleaned: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let trimmed = cleaned.trim_matches('-').to_lowercase();
        if trimmed.is_empty() {
            "app".to_string()
        } else {
            trimmed
        }
    }

    /// The release service: the one behind a `dcd-release` profile if the operator
    /// already marked it, else a conventional app name, else the single service
    /// that publishes ports and is not a router.
    fn release_service(&self) -> Option<String> {
        let profiled = self
            .document
            .services
            .iter()
            .find(|(_, service)| service.profiles.iter().any(|profile| profile == "dcd-release"));
        if let Some((name, _)) = profiled {
            return Some(name.clone());
        }

        let conventional = self
            .document
            .services
            .keys()
            .find(|name| app_service_names().contains(&name.as_str()));
        if let Some(name) = conventional {
            return Some(name.clone());
        }

        let candidates: Vec<&String> = self
            .document
            .services
            .iter()
            .filter(|(name, service)| !service.ports.is_empty() && !self.is_router(name))
            .map(|(name, _)| name)
            .collect();
        match candidates.as_slice() {
            [only] => Some((*only).clone()),
            _ => None,
        }
    }

    fn is_router(&self, name: &str) -> bool {
        if router_service_names().contains(&name) {
            return true;
        }
        let Some(service) = self.document.services.get(name) else {
            return false;
        };
        let Some(image) = &service.image else {
            return false;
        };
        let repository = image.rsplit('/').next().unwrap_or(image);
        let repository = repository.split(':').next().unwrap_or(repository);
        router_images().contains(&repository)
    }

    fn cutover_service(&self, release: Option<&str>) -> Option<String> {
        self.document
            .services
            .keys()
            .find(|name| Some(name.as_str()) != release && self.is_router(name))
            .cloned()
    }

    /// Everything that is neither the release nor the router: side containers dcd
    /// brings up and waits on, which is where `recreate` policy belongs.
    fn managed_services(&self, release: Option<&str>, cutover: Option<&str>) -> Vec<String> {
        self.document
            .services
            .keys()
            .filter(|name| Some(name.as_str()) != release && Some(name.as_str()) != cutover)
            .cloned()
            .collect()
    }

    fn has_healthcheck(&self, name: &str) -> bool {
        self.document
            .services
            .get(name)
            .is_some_and(|service| service.healthcheck.is_some())
    }

    /// The first published container port, which is what a router in front of the
    /// app would be pointed at.
    fn container_port(&self, name: &str) -> Option<String> {
        let service = self.document.services.get(name)?;
        let first = service.ports.first()?;
        let mapping = match first {
            serde_yaml::Value::String(text) => text.clone(),
            serde_yaml::Value::Number(number) => number.to_string(),
            serde_yaml::Value::Mapping(map) => {
                let target = map.get(serde_yaml::Value::String("target".to_string()))?;
                return Some(target.as_u64()?.to_string());
            }
            _ => return None,
        };
        let container_side = mapping.split(':').next_back()?;
        let port = container_side.split('/').next()?;
        port.parse::<u16>().ok().map(|port| port.to_string())
    }

    /// The generated `dcd.yaml`. Every line dcd could infer is filled in; the ones
    /// it cannot are `TODO:` so `dcd check` names them rather than a deploy failing
    /// halfway.
    pub fn render(&self) -> String {
        let release = self.release_service();
        let cutover = self.cutover_service(release.as_deref());
        let managed = self.managed_services(release.as_deref(), cutover.as_deref());

        let mut out = String::new();
        out.push_str("version: 2\n");
        out.push_str(&format!("project: {}\n", self.project));
        out.push_str("ssh: \"TODO: user@host of the target — omit the key to use a local Docker socket\"\n");
        out.push_str("deploy_root: \"TODO: absolute path on the target\"\n\n");
        // Rendered through serde_yaml: a path carrying a comma, '#' or a bracket
        // would otherwise silently reparse as a different list.
        let files = serde_yaml::to_string(&vec![self.compose_file.clone()])
            .unwrap_or_else(|_| format!("- {}\n", self.compose_file));
        out.push_str("compose:\n  files:\n");
        for line in files.lines() {
            out.push_str(&format!("    {line}\n"));
        }
        out.push('\n');

        out.push_str("release:\n");
        match &release {
            Some(service) => {
                out.push_str(&format!("  service: {service}\n"));
                if !self.has_healthcheck(service) {
                    out.push_str(&format!(
                        "  # {service} declares no `healthcheck:` — dcd requires a health gate. Add one\n  # to the compose service, or uncomment this probe, which runs from another\n  # service and MUST address the new container by {{container}}, never an alias.\n  # healthcheck: {{ exec_in: {}, cmd: 'curl -sf http://{{container}}:PORT/health' }}\n",
                        cutover.as_deref().unwrap_or("SERVICE")
                    ));
                }
            }
            None => out.push_str(
                "  service: \"TODO: the compose service to deploy red-black\"\n  # Mark it `profiles: [dcd-release]` so a hand-run `compose up` cannot\n  # start a second copy beside the release dcd is deploying.\n",
            ),
        }
        out.push_str("  # migrate: { before: 'TODO', after: 'TODO' }   # optional expand-contract\n");
        out.push_str("  # drain: 'TODO'                                # optional, runs in the old container\n\n");

        out.push_str("cutover:\n");
        match &cutover {
            Some(service) => out.push_str(&format!("  service: {service}\n")),
            None => out.push_str("  service: \"TODO: the compose service that routes traffic\"\n"),
        }
        let port = release.as_deref().and_then(|service| self.container_port(service));
        match port {
            Some(port) => out.push_str(&format!("  backend_port: {port}\n")),
            None => out.push_str("  # backend_port: TODO — the port the router should send traffic to\n  backend_port: 0\n"),
        }
        out.push_str("  upstream_file: nginx-upstream.conf\n");
        out.push_str("  # The router must bind-mount {deploy_root}/nginx-upstream.conf — only your\n  # compose file can put it inside the container.\n");
        out.push_str(&format!(
            "  reload: {{ exec_in: {}, cmd: 'TODO: the router reload command, e.g. nginx -s reload' }}\n",
            cutover.as_deref().unwrap_or("\"TODO: the router service\"")
        ));
        out.push_str(&format!(
            "  # validate: {{ exec_in: {}, cmd: 'nginx -t' }}\n\n",
            cutover.as_deref().unwrap_or("the router service")
        ));

        if managed.is_empty() {
            out.push_str("# services: {}   # policy for side containers — none detected\n\n");
        } else {
            out.push_str("services:\n");
            for service in &managed {
                out.push_str(&format!("  {service}:\n    recreate: on-image-change\n"));
                if !self.has_healthcheck(service) {
                    out.push_str(&format!(
                        "    # {service} declares no `healthcheck:` — dcd requires one for every\n    # service listed here. Add it to the compose service, or use this probe:\n    # wait: {{ cmd: 'a readiness command inside {service}' }}\n"
                    ));
                }
            }
            out.push('\n');
        }

        out.push_str("stages:\n  prod: {}\n");
        out
    }

    /// What `dcd init` prints, so the operator knows which lines were guessed.
    pub fn notes(&self) -> Vec<String> {
        let release = self.release_service();
        let cutover = self.cutover_service(release.as_deref());
        let mut notes = Vec::new();

        match &release {
            Some(service) => notes.push(format!("release.service: {service}")),
            None => notes.push("release.service: could not be guessed — left as TODO".to_string()),
        }
        match &cutover {
            Some(service) => notes.push(format!("cutover.service: {service}")),
            None => notes.push("cutover.service: no router image recognised — left as TODO".to_string()),
        }

        let mut ungated: Vec<String> = Vec::new();
        for name in release.iter().chain(cutover.iter()) {
            if !self.has_healthcheck(name) {
                ungated.push(name.clone());
            }
        }
        for name in self.managed_services(release.as_deref(), cutover.as_deref()) {
            if !self.has_healthcheck(&name) {
                ungated.push(name);
            }
        }
        if !ungated.is_empty() {
            notes.push(format!(
                "no `healthcheck:` on: {} — dcd requires a health gate on every service it manages",
                ungated.join(", ")
            ));
        }
        notes
    }

}

#[cfg(test)]
mod tests;
