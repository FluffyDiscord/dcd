//! The resolved compose model (spec §7 preamble). Under ADR-013 compose declares
//! containers and dcd declares policy, so this is where dcd learns what the
//! operator actually wrote: service names, container names, images, health gates,
//! restart policies and network aliases.
//!
//! Resolution runs on the deploying machine, against the checkout — the only
//! arrangement in which `check`, `tasks` and `--dry-run` are honest.
//!
//! Nothing in here is ever printed: `docker compose config` inlines resolved env
//! values, so its stdout is parsed and discarded (spec §8.2).

use indexmap::IndexMap;
use serde::Deserialize;

use crate::effects::Argv;
use crate::error::{DcdError, Result};

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ComposeModel {
    #[serde(default)]
    pub services: IndexMap<String, ComposeService>,
    /// The top-level `networks:` block. A service refers to a network by its KEY;
    /// the name Docker actually knows it by lives here (spec §7.1).
    #[serde(default)]
    pub networks: IndexMap<String, ComposeNetwork>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ComposeNetwork {
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ComposeService {
    #[serde(default)]
    pub container_name: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub healthcheck: Option<serde_json::Value>,
    #[serde(default)]
    pub restart: Option<String>,
    #[serde(default)]
    pub deploy: Option<Deploy>,
    #[serde(default)]
    pub profiles: Vec<String>,
    #[serde(default)]
    pub ports: Vec<serde_json::Value>,
    #[serde(default)]
    pub volumes: Vec<ComposeVolume>,
    #[serde(default)]
    pub networks: IndexMap<String, Option<ComposeNetworkAttachment>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Deploy {
    #[serde(default)]
    pub restart_policy: Option<RestartPolicy>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RestartPolicy {
    #[serde(default)]
    pub condition: Option<String>,
    #[serde(default)]
    pub max_attempts: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ComposeVolume {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ComposeNetworkAttachment {
    #[serde(default)]
    pub aliases: Vec<String>,
}

impl ComposeModel {
    pub fn from_json(bytes: &[u8]) -> Result<ComposeModel> {
        serde_json::from_slice(bytes).map_err(|e| DcdError::Config(format!("cannot read the compose model: {e}")))
    }

    pub fn service(&self, name: &str) -> Option<&ComposeService> {
        self.services.get(name)
    }

    /// Networks declared across the model, under the names DOCKER knows them by.
    /// dcd never creates them — `compose up` does — but inspecting them gives a
    /// clear early error (spec §7.1). A service names a network by its compose
    /// KEY ("default"); resolving that key through the top-level block is what
    /// makes the inspect address a network that can actually exist.
    pub fn network_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for service in self.services.values() {
            for key in service.networks.keys() {
                let declared = self.networks.get(key).and_then(|network| network.name.clone());
                let name = declared.unwrap_or_else(|| key.clone());
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        names
    }

    pub fn service_names(&self) -> Vec<String> {
        self.services.keys().cloned().collect()
    }

    /// Names a missing service against the ones that exist, so a typo is a config
    /// error rather than a healthcheck timeout twenty steps later.
    pub fn require_service(&self, name: &str, field: &str) -> Result<&ComposeService> {
        self.service(name).ok_or_else(|| {
            DcdError::Config(format!(
                "{field} '{name}' is not a service in the compose files (known: {})",
                self.service_names().join(", ")
            ))
        })
    }

    /// Applies a `--image <service>=<ref>` pin. The model is the single source of
    /// every container fact (ADR-013), so pinning here is what makes the pin reach
    /// the generated override file, the pull ledger, retention and the release
    /// record at once — there is no second image path (spec §5.5).
    pub fn pin_image(&mut self, service: &str, image: &str) -> Result<()> {
        let known = self.service_names().join(", ");
        let declared = self.services.get_mut(service).ok_or_else(|| {
            DcdError::Config(format!(
                "--image '{service}' is not a service in the compose files (known: {known})"
            ))
        })?;
        declared.image = Some(image.to_string());
        Ok(())
    }
}

impl ComposeService {
    /// What `docker update --restart` is given after `compose run` (spec §7.6):
    /// compose forces `restart=no` on one-off containers, so the policy the
    /// operator declared has to be re-applied by hand.
    pub fn restart_policy(&self) -> Option<String> {
        if let Some(restart) = &self.restart {
            return Some(restart.clone());
        }
        let policy = self.deploy.as_ref()?.restart_policy.as_ref()?;
        let condition = policy.condition.as_deref()?;
        match condition {
            "any" => Some("always".to_string()),
            "none" => Some("no".to_string()),
            "on-failure" => Some(match policy.max_attempts {
                Some(attempts) => format!("on-failure:{attempts}"),
                None => "on-failure".to_string(),
            }),
            _ => None,
        }
    }

    pub fn has_healthcheck(&self) -> bool {
        self.healthcheck.is_some()
    }

    pub fn network_aliases(&self) -> Vec<String> {
        self.networks
            .values()
            .flatten()
            .flat_map(|attachment| attachment.aliases.clone())
            .collect()
    }

    /// Bind sources compose resolved against the *client's* project directory.
    /// dcd uploads compose documents and nothing they reference, so each of these
    /// has to exist on the target already (spec §7.1).
    pub fn bind_sources(&self) -> Vec<String> {
        self.volumes
            .iter()
            .filter(|volume| volume.kind.as_deref() == Some("bind"))
            .filter_map(|volume| volume.source.clone())
            .collect()
    }
}

/// Renders `release.run` (spec §5.3) into a one-service compose document, so a
/// project with no compose service for its app still travels the SAME creation
/// path as every other release — `compose run` against a declared service. The
/// alternative was a second `docker run` path and a second chance to drift from
/// the red-black invariants.
///
/// Env is deliberately absent: values reach the container as bare `-e KEY` from
/// the resolved chain (INV-12), and writing them here would put them on disk.
pub fn render_run_document(service: &str, run: &crate::config::RunSpec) -> String {
    let mut document = serde_norway::Mapping::new();
    let mut definition = serde_norway::Mapping::new();

    let key = |name: &str| serde_norway::Value::String(name.to_string());
    let text = |value: &str| serde_norway::Value::String(value.to_string());
    let list = |values: &[String]| {
        serde_norway::Value::Sequence(values.iter().map(|value| text(value)).collect())
    };

    definition.insert(key("image"), text(&run.image));
    definition.insert(key("restart"), text(&run.restart));
    definition.insert(key("profiles"), list(&[DCD_RELEASE_PROFILE.to_string()]));
    if !run.entrypoint.is_empty() {
        definition.insert(key("entrypoint"), list(&run.entrypoint));
    }
    if !run.command.is_empty() {
        definition.insert(key("command"), list(&run.command));
    }
    if !run.volumes.is_empty() {
        definition.insert(key("volumes"), list(&run.volumes));
    }
    if let Some(env_file) = &run.env_file {
        definition.insert(key("env_file"), list(&[env_file.display().to_string()][..]));
    }

    if let Some(network) = &run.network {
        let mut attachment = serde_norway::Mapping::new();
        if let Some(alias) = &run.network_alias {
            let mut aliases = serde_norway::Mapping::new();
            aliases.insert(key("aliases"), list(std::slice::from_ref(alias)));
            attachment.insert(text(network), serde_norway::Value::Mapping(aliases));
        } else {
            attachment.insert(text(network), serde_norway::Value::Null);
        }
        definition.insert(key("networks"), serde_norway::Value::Mapping(attachment));

        let mut declared = serde_norway::Mapping::new();
        let mut external = serde_norway::Mapping::new();
        external.insert(key("name"), text(network));
        external.insert(key("external"), serde_norway::Value::Bool(true));
        declared.insert(text(network), serde_norway::Value::Mapping(external));
        document.insert(key("networks"), serde_norway::Value::Mapping(declared));
    }

    let mut services = serde_norway::Mapping::new();
    services.insert(text(service), serde_norway::Value::Mapping(definition));
    document.insert(key("services"), serde_norway::Value::Mapping(services));

    serde_norway::to_string(&serde_norway::Value::Mapping(document))
        .unwrap_or_else(|_| format!("services:\n  {service}:\n    image: {}\n", run.image))
}

/// The profile the release service carries, so a hand-run `compose up` cannot
/// start a second copy beside the one dcd is deploying.
pub const DCD_RELEASE_PROFILE: &str = "dcd-release";

/// `compose --profile … config --format json`. The profile flags are mandatory:
/// a service carrying `profiles:` is absent from the model without them, so the
/// operator's own `release.service` would read as undeclared (spec §7 preamble).
/// They are never passed to `up`, which would start the release as a plain service.
pub fn config_argv(project: &str, files: &[std::path::PathBuf], profiles: &[String]) -> Argv {
    let mut argv = vec!["docker".to_string(), "compose".into(), "-p".into(), project.to_string()];
    for profile in profiles {
        argv.push("--profile".into());
        argv.push(profile.clone());
    }
    argv.push("--env-file".into());
    argv.push("/dev/null".into());
    for file in files {
        argv.push("-f".into());
        argv.push(file.display().to_string());
    }
    argv.push("config".into());
    argv.push("--format".into());
    argv.push("json".into());
    Argv(argv)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from a real `docker compose --profile dcd-release config --format json`
    /// run against `docs/examples/roadrunner_app/docker-compose.prod.yml` on
    /// compose 5.3.1 — so these assertions are about compose's actual output shape,
    /// not a hand-written guess at it.
    fn roadrunner() -> ComposeModel {
        ComposeModel::from_json(include_bytes!("testdata/roadrunner-model.json")).expect("the captured model parses")
    }

    /// A service names a network by its compose KEY; `docker network inspect`
    /// needs the name in the top-level block. Reading the key made preflight warn
    /// that the network was missing on every single deploy.
    /// `release.run` was accepted, validated, documented and in the JSON Schema —
    /// and never rendered, so `check` said "config ok" and the deploy then ran
    /// `compose run` against a service no compose file declared.
    #[test]
    fn the_run_fallback_renders_a_service_the_model_can_resolve() {
        let run = crate::config::RunSpec {
            image: "reg/app:v1".to_string(),
            network: Some("acme_default".to_string()),
            network_alias: Some("app".to_string()),
            restart: "unless-stopped".to_string(),
            entrypoint: vec!["/entrypoint.sh".to_string()],
            command: vec!["serve".to_string()],
            volumes: vec!["./data:/data".to_string()],
            ..Default::default()
        };
        let document = render_run_document("demo-release", &run);
        let model = serde_norway::from_str::<serde_norway::Value>(&document).expect("valid YAML");

        let service = &model["services"]["demo-release"];
        assert_eq!(service["image"].as_str(), Some("reg/app:v1"));
        assert_eq!(service["restart"].as_str(), Some("unless-stopped"));
        assert_eq!(service["profiles"][0].as_str(), Some("dcd-release"));
        assert_eq!(service["entrypoint"][0].as_str(), Some("/entrypoint.sh"));
        assert_eq!(service["networks"]["acme_default"]["aliases"][0].as_str(), Some("app"));
        assert_eq!(model["networks"]["acme_default"]["external"].as_bool(), Some(true));
    }

    /// INV-12: the generated document lands on disk, so no env value may enter it.
    #[test]
    fn the_run_fallback_never_writes_an_env_value() {
        let mut env = indexmap::IndexMap::new();
        env.insert("APP_SECRET".to_string(), "hunter2".to_string());
        let run = crate::config::RunSpec {
            image: "reg/app:v1".to_string(),
            restart: "unless-stopped".to_string(),
            env,
            ..Default::default()
        };
        let document = render_run_document("demo-release", &run);
        assert!(!document.contains("hunter2"), "a value reached disk: {document}");
        assert!(!document.contains("APP_SECRET"), "{document}");
    }

    #[test]
    fn a_network_is_reported_under_the_name_docker_knows_it_by() {
        assert_eq!(roadrunner().network_names(), vec!["acme_default".to_string()]);
    }

    /// Compose always emits `name`, but a hand-built model in a test need not.
    #[test]
    fn a_network_with_no_declared_name_falls_back_to_its_key() {
        let model = ComposeModel::from_json(br#"{"services":{"app":{"networks":{"default":{}}}}}"#).unwrap();
        assert_eq!(model.network_names(), vec!["default".to_string()]);
    }

    #[test]
    fn the_worked_example_resolves_every_service_the_config_references() {
        let model = roadrunner();
        for (service, field) in [
            ("app", "release.service"),
            ("nginx", "cutover.service"),
            ("worker", "workers.service"),
            ("postgres", "services.postgres"),
            ("valkey", "services.valkey"),
        ] {
            model.require_service(service, field).unwrap_or_else(|e| panic!("{e}"));
        }
    }

    #[test]
    fn a_missing_service_names_the_ones_that_exist() {
        let error = roadrunner().require_service("aap", "release.service").unwrap_err().to_string();
        assert!(error.contains("release.service 'aap' is not a service"), "got: {error}");
        assert!(error.contains("app"), "the error must list the real services: {error}");
    }

    /// §7.6: compose forces `restart=no` on one-off containers, so dcd re-applies
    /// what the operator declared. Losing this silently breaks reboot-restart.
    #[test]
    fn the_release_restart_policy_is_read_back_from_compose() {
        assert_eq!(roadrunner().service("app").unwrap().restart_policy().as_deref(), Some("unless-stopped"));
    }

    #[test]
    fn deploy_restart_policy_maps_onto_docker_update() {
        let cases = [
            (r#"{"condition":"any"}"#, "always"),
            (r#"{"condition":"none"}"#, "no"),
            (r#"{"condition":"on-failure"}"#, "on-failure"),
            (r#"{"condition":"on-failure","max_attempts":3}"#, "on-failure:3"),
        ];
        for (policy, expected) in cases {
            let json = format!(r#"{{"services":{{"a":{{"deploy":{{"restart_policy":{policy}}}}}}}}}"#);
            let model = ComposeModel::from_json(json.as_bytes()).unwrap();
            assert_eq!(model.service("a").unwrap().restart_policy().as_deref(), Some(expected));
        }
    }

    #[test]
    fn a_service_declaring_no_policy_gets_no_restart_update() {
        let model = ComposeModel::from_json(br#"{"services":{"a":{"image":"x"}}}"#).unwrap();
        assert_eq!(model.service("a").unwrap().restart_policy(), None);
    }

    /// The health gate: nginx declares one, app does not — so the worked example
    /// legitimately needs the `release.healthcheck` escape hatch (§7.7).
    #[test]
    fn health_gates_are_read_from_the_service_definitions() {
        let model = roadrunner();
        assert!(model.service("nginx").unwrap().has_healthcheck());
        assert!(!model.service("app").unwrap().has_healthcheck());
    }

    #[test]
    fn network_aliases_come_from_compose_not_from_dcd() {
        assert_eq!(roadrunner().service("app").unwrap().network_aliases(), vec!["app-rr".to_string()]);
    }

    /// dcd uploads compose documents and nothing they reference, so `check` has to
    /// be able to name every bind source it will NOT be putting there (§7.1).
    #[test]
    fn bind_sources_are_reported_so_check_can_warn_about_them() {
        let sources = roadrunner().service("app").unwrap().bind_sources();
        assert_eq!(sources, vec!["/srv/acme/.docker/logs/symfony".to_string()]);
    }

    /// Without the profile flags the release service is absent from the model and
    /// validation would reject the operator's own config.
    #[test]
    fn config_argv_carries_the_profiles_and_pins_dotenv_discovery_off() {
        let argv = config_argv("acme", &[std::path::PathBuf::from("docker-compose.prod.yml")], &["dcd-release".to_string()]);
        assert_eq!(
            argv.display(),
            "docker compose -p acme --profile dcd-release --env-file /dev/null -f docker-compose.prod.yml config --format json"
        );
    }
}
