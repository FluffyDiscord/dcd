//! Typed configuration: load order is parse → interpolate → select+merge stage →
//! `--set` → deserialize (deny-unknown-fields typo guard) → validate. See spec §5.

mod value_ops;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_yaml::Value;

use crate::error::{DcdError, Result};
use crate::redact::{is_secret_key, Redactor};
use value_ops::{apply_set, get_sequence, interpolate, merge_value, set_sequence};

#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    pub redactor: Redactor,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "one")]
    pub version: u32,
    pub project: String,
    #[serde(default = "dot")]
    pub deploy_root: PathBuf,
    pub network: String,
    #[serde(default)]
    pub registry: Option<String>,
    #[serde(default, deserialize_with = "de_lenient_map")]
    pub images: IndexMap<String, String>,
    pub compose: Compose,
    #[serde(default)]
    pub redact: Vec<String>,
    #[serde(default)]
    pub preflight: Preflight,
    #[serde(default, deserialize_with = "de_lenient_map")]
    pub services: IndexMap<String, Service>,
    pub release: Release,
    pub cutover: Cutover,
    #[serde(default)]
    pub workers: Option<Workers>,
    #[serde(default)]
    pub retention: Retention,
    #[serde(default)]
    pub plugins: Vec<PathBuf>,
    #[serde(default, deserialize_with = "de_lenient_map")]
    pub hooks: IndexMap<String, Vec<HookAction>>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default, skip_deserializing)]
    pub stage: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compose {
    #[serde(default)]
    pub files: Vec<PathBuf>,
    #[serde(default = "compose_env_file")]
    pub env_file: PathBuf,
    #[serde(default, deserialize_with = "de_lenient_map")]
    pub env: IndexMap<String, String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preflight {
    #[serde(default)]
    pub directories: Vec<DirSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirSpec {
    pub path: PathBuf,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    #[serde(default)]
    pub image: Option<String>,
    pub container: String,
    #[serde(default)]
    pub recreate: Recreate,
    #[serde(default)]
    pub on_recreate_drain_workers: bool,
    #[serde(default)]
    pub wait: Option<WaitGate>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Recreate {
    #[default]
    OnImageChange,
    Always,
    Never,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitGate {
    pub exec_in: String,
    pub cmd: String,
    #[serde(default = "sixty")]
    pub retries: u32,
    #[serde(default = "one_sec", deserialize_with = "de_secs")]
    pub interval: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub image: String,
    pub container_prefix: String,
    #[serde(default)]
    pub run: RunSpec,
    pub healthcheck: Healthcheck,
    #[serde(default)]
    pub migrate: Option<Migrate>,
    #[serde(default)]
    pub drain: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunSpec {
    #[serde(default)]
    pub network_alias: Option<String>,
    #[serde(default = "unless_stopped")]
    pub restart: String,
    #[serde(default, deserialize_with = "de_lenient_map")]
    pub env: IndexMap<String, String>,
    #[serde(default)]
    pub volumes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Healthcheck {
    pub exec_in: String,
    pub cmd: String,
    #[serde(default = "sixty")]
    pub retries: u32,
    #[serde(default = "two_sec", deserialize_with = "de_secs")]
    pub interval: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Migrate {
    #[serde(default)]
    pub before: Option<String>,
    #[serde(default)]
    pub after: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cutover {
    #[serde(default = "upstream_default")]
    pub upstream_file: PathBuf,
    #[serde(default = "backend_template")]
    pub template: String,
    pub backend_port: u16,
    #[serde(default = "localhost_backend")]
    pub fallback_backend: String,
    #[serde(default)]
    pub validate: Option<ExecSpec>,
    pub reload: ExecSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecSpec {
    pub exec_in: String,
    pub cmd: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workers {
    #[serde(default = "worker_prefix")]
    pub name_filter: String,
    #[serde(default)]
    pub drain: Option<String>,
    #[serde(default = "onehundredtwenty", deserialize_with = "de_secs")]
    pub stop_timeout: u64,
    #[serde(default = "workers_compose_file")]
    pub compose_file: PathBuf,
    pub provider: WorkerProvider,
    pub template: WorkerTemplate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerProvider {
    #[serde(default, rename = "static")]
    pub static_names: Option<Vec<String>>,
    #[serde(default)]
    pub command_in_release: Option<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerTemplate {
    pub image: String,
    pub entrypoint: Vec<String>,
    pub command: Vec<String>,
    #[serde(default = "sigterm")]
    pub stop_signal: String,
    #[serde(default = "onehundredtwenty", deserialize_with = "de_secs")]
    pub stop_grace_period: u64,
    #[serde(default = "unless_stopped")]
    pub restart: String,
    #[serde(default, deserialize_with = "de_lenient_map")]
    pub env: IndexMap<String, String>,
    #[serde(default)]
    pub volumes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Retention {
    #[serde(default = "three")]
    pub keep_releases: u32,
    #[serde(default = "two")]
    pub keep_managed_images: u32,
}

impl Default for Retention {
    fn default() -> Self {
        Retention {
            keep_releases: 3,
            keep_managed_images: 2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum HookAction {
    Run(String),
    ExecIn { exec_in: ExecRef },
    ExecInRelease { exec_in_release: String },
    Docker { docker: Vec<String> },
    Compose { compose: Vec<String> },
    CpFromRelease { cp_from_release: CpSpec },
    CpToRelease { cp_to_release: CpSpec },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecRef {
    pub service: String,
    pub cmd: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CpSpec {
    pub from: String,
    pub to: String,
}

impl WorkerProvider {
    pub fn static_list(&self) -> Option<&Vec<String>> {
        self.static_names.as_ref()
    }
}

fn one() -> u32 {
    1
}
fn three() -> u32 {
    3
}
fn two() -> u32 {
    2
}
fn sixty() -> u32 {
    60
}
fn one_sec() -> u64 {
    1
}
fn two_sec() -> u64 {
    2
}
fn onehundredtwenty() -> u64 {
    120
}
fn dot() -> PathBuf {
    PathBuf::from(".")
}
fn compose_env_file() -> PathBuf {
    PathBuf::from("compose.env")
}
fn upstream_default() -> PathBuf {
    PathBuf::from("nginx-upstream.conf")
}
fn workers_compose_file() -> PathBuf {
    PathBuf::from("docker-compose.workers.yml")
}
fn backend_template() -> String {
    "set $backend \"{backend}\";".to_string()
}
fn localhost_backend() -> String {
    "127.0.0.1:8080".to_string()
}
fn unless_stopped() -> String {
    "unless-stopped".to_string()
}
fn worker_prefix() -> String {
    "worker-".to_string()
}
fn sigterm() -> String {
    "SIGTERM".to_string()
}

fn de_secs<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct SecsVisitor;
    impl serde::de::Visitor<'_> for SecsVisitor {
        type Value = u64;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a duration like \"120s\" or a number of seconds")
        }
        fn visit_u64<E: serde::de::Error>(self, value: u64) -> std::result::Result<u64, E> {
            Ok(value)
        }
        fn visit_i64<E: serde::de::Error>(self, value: i64) -> std::result::Result<u64, E> {
            Ok(value.max(0) as u64)
        }
        fn visit_str<E: serde::de::Error>(self, value: &str) -> std::result::Result<u64, E> {
            parse_secs(value).map_err(E::custom)
        }
    }
    deserializer.deserialize_any(SecsVisitor)
}

fn parse_secs(raw: &str) -> std::result::Result<u64, String> {
    let s = raw.trim();
    for (suffix, mult) in [("ms", 0u64), ("s", 1), ("m", 60), ("h", 3600)] {
        if let Some(num) = s.strip_suffix(suffix) {
            if mult == 0 {
                return Ok(1); // sub-second readiness polls round up to 1s
            }
            return num
                .trim()
                .parse::<u64>()
                .map(|v| v * mult)
                .map_err(|_| format!("bad duration `{raw}`"));
        }
    }
    s.parse::<u64>().map_err(|_| format!("bad duration `{raw}`"))
}

/// Deserialize an `IndexMap`, tolerating the empty *sequence* mlua emits for an empty
/// Lua table. Plain YAML maps and a YAML empty map `{}` deserialize as usual; only an
/// empty `[]` is coerced to an empty map (a non-empty sequence where a map is expected
/// is still an error). Needed because direct `ctx.cfg` mutation round-trips through Lua.
fn de_lenient_map<'de, D, V>(deserializer: D) -> std::result::Result<IndexMap<String, V>, D::Error>
where
    D: serde::Deserializer<'de>,
    V: serde::Deserialize<'de>,
{
    struct MapOrEmptySeq<V>(std::marker::PhantomData<V>);
    impl<'de, V: serde::Deserialize<'de>> serde::de::Visitor<'de> for MapOrEmptySeq<V> {
        type Value = IndexMap<String, V>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a map (or an empty sequence)")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(self, mut access: A) -> std::result::Result<Self::Value, A::Error> {
            let mut out = IndexMap::new();
            while let Some((key, value)) = access.next_entry::<String, V>()? {
                out.insert(key, value);
            }
            Ok(out)
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut access: A) -> std::result::Result<Self::Value, A::Error> {
            if access.next_element::<serde::de::IgnoredAny>()?.is_some() {
                return Err(serde::de::Error::custom("expected a map, found a non-empty sequence"));
            }
            Ok(IndexMap::new())
        }
    }
    deserializer.deserialize_any(MapOrEmptySeq(std::marker::PhantomData))
}

pub fn load(
    source: &str,
    requested_stage: Option<&str>,
    sets: &[String],
    env: &HashMap<String, String>,
) -> Result<Loaded> {
    let mut doc: Value =
        serde_yaml::from_str(source).map_err(|e| DcdError::Config(format!("parse: {e}")))?;
    interpolate(&mut doc, env)?;

    let stages = doc
        .as_mapping_mut()
        .and_then(|map| map.remove(Value::String("stages".into())));
    let (stage_name, stage_value) = select_stage(stages, requested_stage)?;

    let mut base = doc;
    let base_files = get_sequence(&base, &["compose", "files"]);
    let stage_files = get_sequence(&stage_value, &["compose", "files"]);
    merge_value(&mut base, stage_value);
    if let Some(stage_files) = stage_files {
        let mut combined = base_files.unwrap_or_default();
        combined.extend(stage_files);
        set_sequence(&mut base, &["compose", "files"], combined);
    }

    for assignment in sets {
        apply_set(&mut base, assignment)?;
    }

    let mut config: Config =
        serde_yaml::from_value(base).map_err(|e| DcdError::Config(e.to_string()))?;
    config.stage = stage_name;
    validate(&config)?;
    let redactor = build_redactor(&config);
    Ok(Loaded { config, redactor })
}

/// Rebuild a typed config from a value produced by a plugin mutating `ctx.cfg` (the live
/// Lua table read back as YAML). The `stage` key is engine-owned, so it is stripped and
/// reapplied rather than trusted from Lua; the result is fully validated.
pub fn from_lua_value(mut value: Value, stage: &str) -> Result<Loaded> {
    if let Some(map) = value.as_mapping_mut() {
        map.remove(Value::String("stage".to_string())); // engine-owned, not plugin-settable
    }
    let mut config: Config = serde_yaml::from_value(value).map_err(|e| DcdError::Config(e.to_string()))?;
    config.stage = stage.to_string();
    validate(&config)?;
    let redactor = build_redactor(&config);
    Ok(Loaded { config, redactor })
}

fn select_stage(stages: Option<Value>, requested: Option<&str>) -> Result<(String, Value)> {
    let empty = Value::Mapping(Default::default());
    let Some(Value::Mapping(map)) = stages else {
        return match requested {
            Some(name) => Err(DcdError::Config(format!(
                "stage '{name}' requested but config defines no stages"
            ))),
            None => Ok((String::new(), empty)),
        };
    };
    let names: Vec<String> = map
        .keys()
        .filter_map(|k| k.as_str().map(String::from))
        .collect();
    match requested {
        Some(name) => map
            .get(Value::String(name.to_string()))
            .cloned()
            .map(|v| (name.to_string(), v))
            .ok_or_else(|| {
                DcdError::Config(format!("stage '{name}' not found; known: {}", names.join(", ")))
            }),
        None => match names.as_slice() {
            [] => Ok((String::new(), empty)),
            [only] => Ok((only.clone(), map.get(Value::String(only.clone())).cloned().unwrap())),
            _ => Err(DcdError::Config(format!(
                "multiple stages; pass one of: {}",
                names.join(", ")
            ))),
        },
    }
}

fn validate(config: &Config) -> Result<()> {
    if config.version != 1 {
        return Err(DcdError::Config(format!(
            "unsupported version {}; expected 1",
            config.version
        )));
    }

    let images: HashSet<&str> = config.images.keys().map(String::as_str).collect();
    let require_image = |logical: &str, owner: &str| -> Result<()> {
        if images.contains(logical) {
            Ok(())
        } else {
            Err(DcdError::Config(format!(
                "{owner} references image '{logical}' which is not declared in images:"
            )))
        }
    };
    require_image(&config.release.image, "release.image")?;
    for (name, service) in &config.services {
        if let Some(logical) = &service.image {
            require_image(logical, &format!("services.{name}.image"))?;
        }
    }
    if let Some(workers) = &config.workers {
        require_image(&workers.template.image, "workers.template.image")?;
    }

    let containers: HashSet<&str> = config
        .services
        .values()
        .map(|s| s.container.as_str())
        .collect();
    let require_container = |target: &str, owner: &str| -> Result<()> {
        if containers.contains(target) {
            Ok(())
        } else {
            Err(DcdError::Config(format!(
                "{owner} execs in '{target}' which is not a declared service container"
            )))
        }
    };
    require_container(&config.release.healthcheck.exec_in, "release.healthcheck")?;
    require_container(&config.cutover.reload.exec_in, "cutover.reload")?;
    if let Some(validate) = &config.cutover.validate {
        require_container(&validate.exec_in, "cutover.validate")?;
    }
    for (name, service) in &config.services {
        if let Some(wait) = &service.wait {
            require_container(&wait.exec_in, &format!("services.{name}.wait"))?;
        }
    }

    if !config.release.healthcheck.cmd.contains("{container}") {
        return Err(DcdError::Config(
            "release.healthcheck.cmd must reference {container} (never the shared network_alias)"
                .to_string(),
        ));
    }

    if let Some(workers) = &config.workers {
        if workers.provider.static_list().is_none() && workers.provider.command_in_release.is_none()
        {
            return Err(DcdError::Config(
                "workers.provider needs `static` or `command_in_release`".to_string(),
            ));
        }
    }

    Ok(())
}

fn build_redactor(config: &Config) -> Redactor {
    let redact_keys: HashSet<&str> = config.redact.iter().map(String::as_str).collect();
    let mut values = Vec::new();
    let mut scan = |env: &IndexMap<String, String>| {
        for (key, value) in env {
            if is_secret_key(key) || redact_keys.contains(key.as_str()) {
                values.push(value.clone());
            }
        }
    };
    scan(&config.compose.env);
    scan(&config.release.run.env);
    if let Some(workers) = &config.workers {
        scan(&workers.template.env);
    }
    Redactor::new(values)
}

#[cfg(test)]
mod tests;
