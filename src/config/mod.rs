//! Typed configuration: load order is parse → interpolate → select+merge stage →
//! `--set` → identity defaults (project/network/{project}) → deserialize
//! (deny-unknown-fields typo guard) → validate. See spec §5.

mod value_ops;

use std::collections::HashMap;
use std::path::PathBuf;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_yaml::Value;

use crate::error::{DcdError, Result};
use value_ops::{apply_set, default_identity, get_sequence, interpolate, merge_value, set_sequence};

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "two")]
    pub version: u32,
    #[serde(default)]
    pub project: String,
    /// The ssh target, or an `~/.ssh/config` Host alias. Absent ⇒ everything runs
    /// locally, reproducing v1 exactly (spec §2.7).
    #[serde(default)]
    pub ssh: Option<String>,
    #[serde(default = "dot")]
    pub deploy_root: PathBuf,
    /// Used only by `dcd gc --all` as the ownership proof for prunable
    /// repositories; images themselves come from compose (spec §5).
    #[serde(default)]
    pub registry: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
    pub compose: Compose,
    #[serde(default)]
    pub directories: Vec<DirSpec>,
    pub release: Release,
    pub cutover: Cutover,
    /// Policy only. Identity — container name, image, readiness — is derived from
    /// the compose model (ADR-013).
    #[serde(default, deserialize_with = "de_lenient_map")]
    pub services: IndexMap<String, ServicePolicy>,
    #[serde(default)]
    pub workers: Option<Workers>,
    #[serde(default)]
    pub retention: Retention,
    #[serde(default)]
    pub plugins: Vec<PathBuf>,
    #[serde(default, deserialize_with = "de_lenient_map")]
    pub hooks: IndexMap<String, Vec<HookAction>>,
    #[serde(default, skip_deserializing)]
    pub stage: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Compose {
    pub files: Vec<PathBuf>,
    #[serde(default, deserialize_with = "de_lenient_map")]
    pub env: IndexMap<String, String>,
    /// Enabled when RESOLVING the model, never passed to `up` — a service carrying
    /// `profiles:` is otherwise absent from the model (spec §7 preamble).
    #[serde(default = "release_profiles")]
    pub profiles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DirSpec {
    pub path: PathBuf,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
}

/// What compose cannot express about a side container.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServicePolicy {
    #[serde(default)]
    pub recreate: Recreate,
    #[serde(default)]
    pub on_recreate_drain_workers: bool,
    /// Overrides the compose healthcheck as the readiness gate.
    #[serde(default)]
    pub wait: Option<WaitGate>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Recreate {
    #[default]
    OnImageChange,
    Always,
    Never,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WaitGate {
    /// A compose service; defaults to the service being waited on.
    #[serde(default)]
    pub exec_in: Option<String>,
    pub cmd: String,
    #[serde(default = "sixty")]
    pub retries: u32,
    #[serde(default = "one_sec", deserialize_with = "de_secs")]
    #[schemars(schema_with = "duration_schema")]
    pub interval: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Release {
    /// The compose service cut over to. Mutually exclusive with `run` (spec §5.1).
    #[serde(default)]
    pub service: Option<String>,
    /// The no-compose-service fallback, rendered into a one-service compose
    /// document so the engine keeps one creation primitive (spec §5.3).
    #[serde(default)]
    pub run: Option<RunSpec>,
    #[serde(default)]
    pub container_prefix: Option<String>,
    /// The escape hatch: required only when the service declares no compose
    /// `healthcheck:` (spec §7.7).
    #[serde(default)]
    pub healthcheck: Option<Healthcheck>,
    #[serde(default)]
    pub migrate: Option<Migrate>,
    #[serde(default)]
    pub drain: Option<String>,
}

impl Release {
    /// `{project}-{service}` unless the operator pinned it.
    pub fn container_prefix(&self, project: &str) -> String {
        if let Some(prefix) = &self.container_prefix {
            return prefix.clone();
        }
        match &self.service {
            Some(service) => format!("{project}-{service}"),
            None => format!("{project}-release"),
        }
    }

    /// The compose service the release is created from — the rendered fallback
    /// service when `run` is used, so downstream code has exactly one name.
    pub fn service_name(&self, project: &str) -> String {
        match &self.service {
            Some(service) => service.clone(),
            None => format!("{project}-release"),
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunSpec {
    pub image: String,
    #[serde(default)]
    pub network: Option<String>,
    #[serde(default)]
    pub network_alias: Option<String>,
    #[serde(default = "unless_stopped")]
    pub restart: String,
    #[serde(default)]
    pub entrypoint: Vec<String>,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub env_file: Option<PathBuf>,
    #[serde(default, deserialize_with = "de_lenient_map")]
    pub env: IndexMap<String, String>,
    #[serde(default)]
    pub env_include: Vec<String>,
    #[serde(default)]
    pub env_exclude: Vec<String>,
    #[serde(default)]
    pub volumes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Healthcheck {
    /// A compose service name.
    pub exec_in: String,
    pub cmd: String,
    #[serde(default = "sixty")]
    pub retries: u32,
    #[serde(default = "two_sec", deserialize_with = "de_secs")]
    #[schemars(schema_with = "duration_schema")]
    pub interval: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Migrate {
    #[serde(default)]
    pub before: Option<String>,
    #[serde(default)]
    pub after: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Cutover {
    /// The router service; dcd resolves its container name from compose.
    pub service: String,
    pub backend_port: u16,
    #[serde(default = "upstream_default")]
    pub upstream_file: PathBuf,
    #[serde(default = "backend_template")]
    pub template: String,
    #[serde(default = "localhost_backend")]
    pub fallback_backend: String,
    #[serde(default)]
    pub validate: Option<ExecSpec>,
    pub reload: ExecSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecSpec {
    /// A compose service name.
    pub exec_in: String,
    pub cmd: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Workers {
    /// ONE compose service; dcd runs N containers from it.
    pub service: String,
    pub provider: WorkerProvider,
    #[serde(default)]
    pub drain: Option<String>,
    /// Container naming only, NEVER discovery (INV-14).
    #[serde(default = "worker_prefix")]
    pub name_prefix: String,
    #[serde(default = "onehundredtwenty", deserialize_with = "de_secs")]
    #[schemars(schema_with = "duration_schema")]
    pub stop_timeout: u64,
    #[serde(default = "sigterm")]
    pub stop_signal: String,
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Appended after each worker name on the `compose run` argv.
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkerProvider {
    #[serde(default, rename = "static")]
    pub static_names: Option<Vec<String>>,
    #[serde(default)]
    pub command_in_release: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Retention {
    #[serde(default = "one")]
    pub keep_releases: u32,
    #[serde(default = "one")]
    pub keep_managed_images: u32,
    /// Keyed by compose service name (spec §5.4).
    #[serde(default, deserialize_with = "de_lenient_map")]
    pub keep_images: IndexMap<String, u32>,
}

impl Default for Retention {
    fn default() -> Self {
        Retention {
            keep_releases: one(),
            keep_managed_images: one(),
            keep_images: IndexMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
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

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecRef {
    pub service: String,
    pub cmd: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
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

fn two() -> u32 {
    2
}

fn release_profiles() -> Vec<String> {
    vec!["dcd-release".to_string()]
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
fn upstream_default() -> PathBuf {
    PathBuf::from("nginx-upstream.conf")
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

/// `de_secs` takes seconds as an integer OR a duration string, so the derived
/// `u64` schema would red-underline every `interval: 2s` in the field reference.
fn duration_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "description": "seconds as an integer, or a duration string: 250ms, 2s, 5m, 1h",
        "anyOf": [
            { "type": "integer", "minimum": 0 },
            { "type": "string", "pattern": "^[0-9]+(ms|s|m|h)?$" }
        ]
    })
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
) -> Result<Config> {
    let mut doc: Value =
        serde_yaml::from_str(source).map_err(|e| DcdError::Config(format!("parse: {e}")))?;
    reject_removed_keys(&doc)?;
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

    // Conventional env defaults, so a minimal config can omit them (CI usually sets both).
    inject_env_default(&mut base, "registry", env, &["REGISTRY", "CI_REGISTRY_IMAGE"]);
    inject_env_default(&mut base, "deploy_root", env, &["DEPLOY_ROOT"]);

    for assignment in sets {
        apply_set(&mut base, assignment)?;
    }

    // Fill project/network from the deploy_root folder and expand the {project} token (after --set,
    // so an explicit override still wins).
    default_identity(&mut base);

    let mut config: Config =
        serde_yaml::from_value(base).map_err(|e| DcdError::Config(e.to_string()))?;
    config.stage = stage_name;
    validate(&config)?;
    Ok(config)
}

/// Resolve the stage name from a raw (uninterpolated) config — stage names are map
/// keys, which interpolation never touches, so the dotenv chain can be loaded for
/// the right stage before `${VAR}` resolution runs (spec §5.2.1).
pub fn peek_stage(source: &str, requested: Option<&str>) -> Result<String> {
    let mut doc: Value =
        serde_yaml::from_str(source).map_err(|e| DcdError::Config(format!("parse: {e}")))?;
    let stages = doc
        .as_mapping_mut()
        .and_then(|map| map.remove(Value::String("stages".into())));
    select_stage(stages, requested).map(|(name, _)| name)
}

/// A removed key gets a targeted migration error, not the misleading generic
/// "unknown field" (spec §5.1 — the AGENTS.md §7 typo row would misdirect).
fn reject_removed_keys(doc: &Value) -> Result<()> {
    for (path, guidance) in REMOVED_KEYS {
        if removed_key_present(doc, path) {
            return Err(DcdError::Config(format!("{} was removed — {guidance}; see UPGRADE.md", path.join("."))));
        }
    }
    Ok(())
}

/// The JSON Schema for an AUTHORED `dcd.yaml`, which is not the shape of `Config`:
/// `stages:` is stripped before deserialization (§5.1), so a schema derived from
/// the struct alone describes a document nobody writes and rejects every real one.
///
/// `required` is dropped throughout because a stage may legitimately supply any
/// key — a config whose `release:` lives only under `stages.prod` is valid — and
/// `stages` itself becomes the one required key, which is what `select_stage`
/// enforces.
pub fn authoring_schema() -> Result<serde_json::Value> {
    let derived = schemars::schema_for!(Config);
    let mut document = serde_json::to_value(derived)
        .map_err(|e| DcdError::Config(format!("cannot render the schema: {e}")))?;
    drop_required(&mut document);

    let properties = document
        .get("properties")
        .cloned()
        .ok_or_else(|| DcdError::Config("the derived schema has no properties".to_string()))?;
    let stage_override = serde_json::json!({
        "type": "object",
        "description": "deep-merged over the keys above: scalars replace, maps merge, compose.files append",
        "properties": properties,
        "additionalProperties": false,
    });

    let root = document
        .as_object_mut()
        .ok_or_else(|| DcdError::Config("the derived schema is not an object".to_string()))?;
    root.insert("title".to_string(), serde_json::json!("dcd.yaml"));
    root.insert("required".to_string(), serde_json::json!(["stages"]));
    root.get_mut("properties")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| DcdError::Config("the derived schema has no properties".to_string()))?
        .insert(
            "stages".to_string(),
            serde_json::json!({
                "type": "object",
                "description": "named deploy targets; `dcd deploy <stage>` picks one",
                "additionalProperties": stage_override,
            }),
        );
    Ok(document)
}

/// The exact marker `dcd init --from-compose` leaves in value position.
const PLACEHOLDER: &str = "TODO: ";

fn drop_required(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.remove("required");
            for (key, nested) in map.iter_mut() {
                // A union's branches are how a variant is IDENTIFIED, not merged
                // into: stripping their `required` makes every hook action accept
                // any object, so `exec_inn:` would validate clean.
                if key == "anyOf" || key == "oneOf" {
                    continue;
                }
                drop_required(nested);
            }
        }
        serde_json::Value::Array(items) => {
            for nested in items.iter_mut() {
                drop_required(nested);
            }
        }
        _ => {}
    }
}

/// Every v1 key that v2 dropped, with the replacement named. A bare
/// `deny_unknown_fields` "unknown field" would misdirect the reader to the typo
/// row of the AGENTS.md §7 table (spec §5.1).
const REMOVED_KEYS: &[(&[&str], &str)] = &[
    (&["compose", "env_file"], "dcd writes no env file"),
    (&["docker"], "container identity now comes from your compose file; declare policy under `services:`"),
    (&["network"], "networks come from your compose file; dcd no longer creates them"),
    (&["release", "image"], "use `release.service:` (a compose service) or `release.run.image:`"),
    (&["workers", "template"], "worker containers are declared as ONE compose service; set `workers.service:`"),
    (&["workers", "compose_file"], "dcd no longer renders a workers compose file"),
    (&["workers", "name_filter"], "renamed to `workers.name_prefix` (naming only — discovery is by service label)"),
];

/// A removed key counts whether it sits in the base document or in any stage.
fn removed_key_present(doc: &Value, path: &[&str]) -> bool {
    let lookup = |root: &Value| -> bool {
        let mut cursor = root;
        for key in path {
            match cursor.get(*key) {
                Some(next) => cursor = next,
                None => return false,
            }
        }
        true
    };
    if lookup(doc) {
        return true;
    }
    doc.get("stages")
        .and_then(Value::as_mapping)
        .is_some_and(|stages| stages.values().any(lookup))
}

/// Set a top-level key from the first env var present, only if the config did not already
/// provide it — `--set` still wins (it is applied after).
fn inject_env_default(base: &mut Value, key: &str, env: &HashMap<String, String>, vars: &[&str]) {
    let Some(map) = base.as_mapping_mut() else { return };
    if map.contains_key(Value::String(key.to_string())) {
        return;
    }
    for var in vars {
        if let Some(value) = env.get(*var) {
            map.insert(Value::String(key.to_string()), Value::String(value.clone()));
            return;
        }
    }
}

/// Rebuild a typed config from a value produced by a plugin mutating `ctx.cfg` (the live
/// Lua table read back as YAML). The `stage` key is engine-owned, so it is stripped and
/// reapplied rather than trusted from Lua; the result is fully validated.
pub fn from_lua_value(mut value: Value, stage: &str) -> Result<Config> {
    if let Some(map) = value.as_mapping_mut() {
        map.remove(Value::String("stage".to_string())); // engine-owned, not plugin-settable
    }
    let mut config: Config = serde_yaml::from_value(value).map_err(|e| DcdError::Config(e.to_string()))?;
    config.stage = stage.to_string();
    validate(&config)?;
    Ok(config)
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

/// Checks that can only be made once the compose model is resolved. Static
/// validation runs at load; this runs as soon as the model is available, so a
/// bad reference fails before anything is touched rather than twenty steps later.
pub fn validate_with_model(config: &Config, model: &crate::compose::ComposeModel) -> Result<()> {
    let release_service = config.release.service_name(&config.project);
    if config.release.service.is_some() {
        model.require_service(&release_service, "release.service")?;
    }
    model.require_service(&config.cutover.service, "cutover.service")?;
    if let Some(workers) = &config.workers {
        model.require_service(&workers.service, "workers.service")?;
    }
    for (name, _) in &config.services {
        model.require_service(name, "services")?;
    }

    validate_health_gates(config, model)?;

    let mut targets = vec![(&config.cutover.reload.exec_in, "cutover.reload")];
    if let Some(validate) = &config.cutover.validate {
        targets.push((&validate.exec_in, "cutover.validate"));
    }
    if let Some(probe) = &config.release.healthcheck {
        targets.push((&probe.exec_in, "release.healthcheck"));
    }
    for (service, owner) in targets {
        model.require_service(service, owner)?;
    }
    Ok(())
}

/// Every container dcd waits on must declare a health gate, and this is an error
/// rather than a warning: `compose up --wait` returns as soon as a container is
/// RUNNING when its service has no `healthcheck:`, so a missing gate silently
/// turns readiness into "started" — which is exactly the not-yet-ready-database
/// race that ordered recreate and `on_recreate_drain_workers` exist to prevent.
fn validate_health_gates(config: &Config, model: &crate::compose::ComposeModel) -> Result<()> {
    for (name, policy) in &config.services {
        if policy.wait.is_some() {
            continue;
        }
        let declared = model.service(name).is_some_and(|service| service.has_healthcheck());
        if !declared {
            return Err(DcdError::Config(format!(
                "service '{name}' has no health gate: give it a `healthcheck:` in your compose file, or a \
                 `services.{name}.wait` probe. Without one `compose up --wait` returns as soon as the \
                 container starts, so dcd would cut over to a container that is not ready yet"
            )));
        }
    }

    let release_service = config.release.service_name(&config.project);
    let release_declares = model
        .service(&release_service)
        .is_some_and(|service| service.has_healthcheck());
    if !release_declares && config.release.healthcheck.is_none() {
        return Err(DcdError::Config(format!(
            "release service '{release_service}' has no health gate: give it a `healthcheck:` in your compose \
             file, or set `release.healthcheck`. The cutover has nothing to wait on otherwise"
        )));
    }
    Ok(())
}

fn validate(config: &Config) -> Result<()> {
    if config.version != 2 {
        return Err(DcdError::Config(format!(
            "unsupported version {}; expected 2",
            config.version
        )));
    }

    validate_release_shape(config)?;
    validate_health_gate(config)?;
    validate_worker_rules(config)?;
    validate_directories(config)?;
    validate_keep_images(config)?;
    validate_env_rules(config)?;
    validate_no_placeholders_left(config)?;
    validate_cutover_backend(config)?;

    Ok(())
}

/// Port 0 is not a port. It is what `dcd init --from-compose` writes when the
/// release publishes none, and the `TODO:` above it is a comment the placeholder
/// guard cannot see — so without this, a scaffold validates clean and then points
/// the router at `<container>:0`.
fn validate_cutover_backend(config: &Config) -> Result<()> {
    if config.cutover.backend_port == 0 {
        return Err(DcdError::Config(
            "cutover.backend_port is unset — name the port the router should send traffic to".to_string(),
        ));
    }
    Ok(())
}

/// `dcd init --from-compose` fills in what it can infer and leaves `PLACEHOLDER`
/// on what it cannot. Catching those here is what turns "the scaffold still needs
/// you" into an error naming the field, instead of a deploy that ssh's to a host
/// literally called `TODO: user@host`. The prefix is matched exactly, so an
/// operator's own `TODO_FLUSH=1 bin/console app:drain` is not a placeholder.
fn validate_no_placeholders_left(config: &Config) -> Result<()> {
    let document = serde_yaml::to_value(config)
        .map_err(|e| DcdError::Config(format!("cannot inspect the config: {e}")))?;
    let mut unfilled = Vec::new();
    collect_placeholders(&document, String::new(), &mut unfilled);
    if unfilled.is_empty() {
        return Ok(());
    }
    Err(DcdError::Config(format!(
        "still scaffolded — fill in: {}",
        unfilled.join(", ")
    )))
}

fn collect_placeholders(value: &serde_yaml::Value, path: String, unfilled: &mut Vec<String>) {
    match value {
        serde_yaml::Value::String(text) if text.starts_with(PLACEHOLDER) => unfilled.push(path),
        serde_yaml::Value::Mapping(map) => {
            for (key, nested) in map {
                let name = key.as_str().unwrap_or("?");
                let nested_path = if path.is_empty() {
                    name.to_string()
                } else {
                    format!("{path}.{name}")
                };
                collect_placeholders(nested, nested_path, unfilled);
            }
        }
        serde_yaml::Value::Sequence(items) => {
            for (index, nested) in items.iter().enumerate() {
                collect_placeholders(nested, format!("{path}[{index}]"), unfilled);
            }
        }
        _ => {}
    }
}

/// The release comes from a compose service or from `run`, never both: two
/// creation paths would be two chances to drift from the red-black invariants.
fn validate_release_shape(config: &Config) -> Result<()> {
    match (&config.release.service, &config.release.run) {
        (Some(_), Some(_)) => Err(DcdError::Config(
            "release: declare service: (compose-owned) or run: (dcd-owned), not both".to_string(),
        )),
        (None, None) => Err(DcdError::Config(
            "release needs service: (a compose service) or run: (the no-compose fallback)".to_string(),
        )),
        _ => Ok(()),
    }
}

/// The escape-hatch probe runs from another container, so it must target the black
/// by NAME. `--use-aliases` gives red and black the same aliases, so an alias would
/// resolve to red and pass falsely.
fn validate_health_gate(config: &Config) -> Result<()> {
    let Some(probe) = &config.release.healthcheck else {
        return Ok(());
    };
    if !probe.cmd.contains("{container}") {
        return Err(DcdError::Config(
            "release.healthcheck.cmd must reference {container}: red and black share the service's \
             network aliases, so an alias resolves to red and the probe passes falsely"
                .to_string(),
        ));
    }
    Ok(())
}

/// Worker discovery is by compose service label (INV-14). Sharing the service with
/// the release would make worker drain `docker stop` the container serving traffic,
/// and an overlapping name prefix makes the reapers sweep each other's containers.
fn validate_worker_rules(config: &Config) -> Result<()> {
    let Some(workers) = &config.workers else {
        return Ok(());
    };

    if workers.provider.static_names.is_none() && workers.provider.command_in_release.is_none() {
        return Err(DcdError::Config(
            "workers.provider needs `static` or `command_in_release`".to_string(),
        ));
    }

    if let Some(release_service) = &config.release.service {
        if &workers.service == release_service {
            return Err(DcdError::Config(format!(
                "workers.service '{}' must not equal release.service: worker discovery is by service label,                  so sharing it would make worker drain stop the release container mid-traffic (INV-14)",
                workers.service
            )));
        }
    }

    let container_prefix = config.release.container_prefix(&config.project);
    if workers.name_prefix.starts_with(&container_prefix) || container_prefix.starts_with(&workers.name_prefix) {
        return Err(DcdError::Config(format!(
            "workers.name_prefix '{}' overlaps release container prefix '{container_prefix}':              `docker ps --filter name=` is an unanchored match, so the reapers would sweep each other",
            workers.name_prefix
        )));
    }

    Ok(())
}

/// `directories[].path` is interpolated into a root-equivalent `chown` inside a
/// container that mounts `deploy_root`, so an escaping path is a privilege bug.
fn validate_directories(config: &Config) -> Result<()> {
    for directory in &config.directories {
        if directory.path.is_absolute() {
            return Err(DcdError::Config(format!(
                "directories path '{}' must be relative to deploy_root",
                directory.path.display()
            )));
        }
        if directory.path.components().any(|c| c == std::path::Component::ParentDir) {
            return Err(DcdError::Config(format!(
                "directories path '{}' must not contain '..'",
                directory.path.display()
            )));
        }
    }
    Ok(())
}

/// `retention.keep_images` overrides `keep_managed_images` for one image dcd manages.
/// The release image is rejected because `keep_releases` is the knob that bounds it —
/// two counts over one image would only be a way to disagree with yourself.
fn validate_keep_images(config: &Config) -> Result<()> {
    if config.retention.keep_releases == 0 {
        return Err(DcdError::Config(
            "retention.keep_releases must be at least 1: at 0 the rollback target is evicted and its image \
             removed, so `dcd rollback` depends entirely on the tag still being pullable (INV-6)"
                .to_string(),
        ));
    }

    let release_service = config.release.service_name(&config.project);
    for service in config.retention.keep_images.keys() {
        if service == &release_service {
            return Err(DcdError::Config(format!(
                "retention.keep_images cannot set '{service}': it is release.service, bound by retention.keep_releases"
            )));
        }
    }
    Ok(())
}

fn validate_env_rules(config: &Config) -> Result<()> {
    use crate::dotenv::{filter_container_keys, invalid_env_key, reserved_key_reason, ReservedAllowance};

    let guard = |env: &IndexMap<String, String>, owner: &str, allowance: ReservedAllowance| -> Result<()> {
        for key in env.keys() {
            if invalid_env_key(key) {
                return Err(DcdError::Config(format!(
                    "invalid env key `{key}` in {owner} (letters, digits, and underscore only, not starting with a digit)"
                )));
            }
            if let Some(reason) = reserved_key_reason(key, allowance) {
                return Err(DcdError::Config(format!("{key} in {owner} is reserved ({reason})")));
            }
        }
        Ok(())
    };
    guard(&config.compose.env, "compose.env", ReservedAllowance::ComposeVars)?;

    // Worker containers declare their env on the compose service (spec §7.12), so
    // the only dcd-owned env map left is the `run` fallback's.
    if let Some(run) = &config.release.run {
        guard(&run.env, "release.run.env", ReservedAllowance::ProxyVars)?;
        let empty = std::collections::BTreeMap::new();
        filter_container_keys(&empty, &run.env_include, &run.env_exclude)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
