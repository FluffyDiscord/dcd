//! Transforms on the raw YAML tree, run in order: interpolate `${VAR}`, merge the
//! selected stage over the base, apply `--set` overrides — all before typed
//! deserialization, so each rule (spec §5.1) is unit-tested in isolation here.

use std::collections::HashMap;

use serde_yaml::Value;

use crate::error::{DcdError, Result};

/// Replace every `${VAR}` / `${VAR:-default}` in every string in the tree.
pub fn interpolate(value: &mut Value, env: &HashMap<String, String>) -> Result<()> {
    match value {
        Value::String(s) => *s = interpolate_str(s, env)?,
        Value::Sequence(items) => {
            for item in items {
                interpolate(item, env)?;
            }
        }
        Value::Mapping(map) => {
            for (_key, val) in map.iter_mut() {
                interpolate(val, env)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn interpolate_str(input: &str, env: &HashMap<String, String>) -> Result<String> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| DcdError::Config(format!("unterminated `${{` in `{input}`")))?;
        let token = &after[..end];
        let (name, default) = match token.split_once(":-") {
            Some((name, default)) => (name.trim(), Some(default)),
            None => (token.trim(), None),
        };
        let resolved = env
            .get(name)
            .cloned()
            .or_else(|| default.map(str::to_string))
            .ok_or_else(|| DcdError::Config(format!("${{{name}}} is not set")))?;
        out.push_str(&resolved);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Deep-merge `overlay` onto `base`: maps merge key-by-key; scalars and sequences
/// from the overlay replace the base (the documented lists-replace rule). The one
/// exception (`compose.files` appends) is handled by the caller before this runs.
pub fn merge_value(base: &mut Value, overlay: Value) {
    match overlay {
        Value::Mapping(overlay_map) => {
            let Value::Mapping(base_map) = base else {
                *base = Value::Mapping(overlay_map);
                return;
            };
            for (key, overlay_val) in overlay_map {
                match base_map.get_mut(&key) {
                    Some(base_val) => merge_value(base_val, overlay_val),
                    None => {
                        base_map.insert(key, overlay_val);
                    }
                }
            }
        }
        other => *base = other,
    }
}

/// Read a nested sequence at a dotted path (used for the `compose.files` append).
pub fn get_sequence(root: &Value, path: &[&str]) -> Option<Vec<Value>> {
    let mut cur = root;
    for key in path {
        cur = cur.get(*key)?;
    }
    cur.as_sequence().cloned()
}

/// Set a nested sequence at a dotted path, creating intermediate maps as needed.
pub fn set_sequence(root: &mut Value, path: &[&str], items: Vec<Value>) {
    let mut cur = root;
    for key in &path[..path.len() - 1] {
        if cur.get(*key).is_none() {
            if let Value::Mapping(map) = cur {
                map.insert(Value::String((*key).to_string()), Value::Mapping(Default::default()));
            }
        }
        cur = cur.get_mut(*key).expect("intermediate map just ensured");
    }
    if let Value::Mapping(map) = cur {
        map.insert(Value::String(path[path.len() - 1].to_string()), Value::Sequence(items));
    }
}

enum Seg {
    Key(String),
    Index(usize),
}

/// Apply one `--set dotted.path[i]=value` override onto an existing scalar.
pub fn apply_set(root: &mut Value, assignment: &str) -> Result<()> {
    let (path, raw) = assignment
        .split_once('=')
        .ok_or_else(|| DcdError::Config(format!("--set `{assignment}` must be path=value")))?;
    let segments = parse_path(path)?;
    let scalar = parse_scalar(raw);
    set_existing_scalar(root, &segments, path, scalar)
}

fn set_existing_scalar(cur: &mut Value, segments: &[Seg], path: &str, scalar: Value) -> Result<()> {
    let (head, tail) = segments.split_first().expect("non-empty path");
    let slot = navigate(cur, head, path)?;
    if tail.is_empty() {
        if slot.is_mapping() || slot.is_sequence() {
            return Err(DcdError::Config(format!("--set {path}: target is not a scalar")));
        }
        *slot = scalar;
        Ok(())
    } else {
        set_existing_scalar(slot, tail, path, scalar)
    }
}

fn navigate<'a>(cur: &'a mut Value, seg: &Seg, path: &str) -> Result<&'a mut Value> {
    match seg {
        Seg::Key(key) => cur
            .as_mapping_mut()
            .and_then(|map| map.get_mut(Value::String(key.clone())))
            .ok_or_else(|| DcdError::Config(format!("--set {path}: key `{key}` does not exist"))),
        Seg::Index(index) => cur
            .as_sequence_mut()
            .and_then(|seq| seq.get_mut(*index))
            .ok_or_else(|| DcdError::Config(format!("--set {path}: index [{index}] out of range"))),
    }
}

fn parse_path(path: &str) -> Result<Vec<Seg>> {
    let mut segments = Vec::new();
    for part in path.split('.') {
        let (key, indices) = match part.split_once('[') {
            Some((key, rest)) => (key, Some(rest)),
            None => (part, None),
        };
        if !key.is_empty() {
            segments.push(Seg::Key(key.to_string()));
        }
        if let Some(indices) = indices {
            for chunk in indices.split('[') {
                let digits = chunk.trim_end_matches(']');
                let index = digits
                    .parse::<usize>()
                    .map_err(|_| DcdError::Config(format!("--set {path}: bad index `{digits}`")))?;
                segments.push(Seg::Index(index));
            }
        }
    }
    if segments.is_empty() {
        return Err(DcdError::Config(format!("--set: empty path in `{path}`")));
    }
    Ok(segments)
}

fn parse_scalar(raw: &str) -> Value {
    serde_yaml::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// Derive identity so a lean, multi-stage config can omit it: `project` defaults to the
/// `deploy_root` folder name and `compose.env.COMPOSE_PROJECT_NAME` to the project; then the
/// `{project}` token is expanded everywhere. Because `deploy_root` differs per stage, every
/// container namespaces per stage with no repetition (e.g. prod vs beta on one host). An
/// explicit value always wins — this only fills what is absent.
///
/// Networks are NOT derived: under ADR-013 they are declared in the compose file, and a
/// `<project>_default` guess would inspect a network nothing uses.
pub fn default_identity(base: &mut Value) {
    let project = resolve_project(base);
    if let Value::Mapping(map) = base {
        map.insert(Value::String("project".into()), Value::String(project.clone()));
    }
    inject_compose_project_name(base, &project);
    expand_token(base, "{project}", &project);
}

fn resolve_project(base: &Value) -> String {
    if let Some(explicit) = base.get("project").and_then(Value::as_str) {
        if !explicit.is_empty() {
            return explicit.to_string();
        }
    }
    let deploy_root = base.get("deploy_root").and_then(Value::as_str).unwrap_or(".");
    folder_name(deploy_root)
}

fn folder_name(path: &str) -> String {
    let named = std::path::Path::new(path.trim_end_matches('/'))
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty() && *n != ".");
    match named {
        Some(name) => name.to_string(),
        None => std::env::current_dir()
            .ok()
            .and_then(|cwd| cwd.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "app".to_string()),
    }
}

fn inject_compose_project_name(base: &mut Value, project: &str) {
    let Some(root) = base.as_mapping_mut() else { return };
    let Some(compose) = ensure_map(root, "compose") else { return };
    let Some(env) = ensure_map(compose, "env") else { return };
    if !env.contains_key(Value::String("COMPOSE_PROJECT_NAME".into())) {
        env.insert(
            Value::String("COMPOSE_PROJECT_NAME".into()),
            Value::String(project.to_string()),
        );
    }
}

/// Ensure `parent[key]` is a mapping, creating it if missing, and return it.
fn ensure_map<'a>(parent: &'a mut serde_yaml::Mapping, key: &str) -> Option<&'a mut serde_yaml::Mapping> {
    let slot = Value::String(key.to_string());
    if !parent.contains_key(slot.clone()) {
        parent.insert(slot.clone(), Value::Mapping(Default::default()));
    }
    parent.get_mut(slot).and_then(Value::as_mapping_mut)
}

fn expand_token(value: &mut Value, token: &str, replacement: &str) {
    match value {
        Value::String(s) if s.contains(token) => *s = s.replace(token, replacement),
        Value::Sequence(items) => items.iter_mut().for_each(|v| expand_token(v, token, replacement)),
        Value::Mapping(map) => map.iter_mut().for_each(|(_, v)| expand_token(v, token, replacement)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn yaml(s: &str) -> Value {
        serde_yaml::from_str(s).unwrap()
    }

    #[test]
    fn default_identity_fills_from_deploy_root_and_expands_token() {
        let mut v = yaml(
            "deploy_root: /var/www/beta-app\ncompose:\n  files: [c.yml]\nrelease:\n  container_prefix: '{project}-app'\n  healthcheck: { exec_in: '{project}-router', cmd: x }",
        );
        default_identity(&mut v);
        assert_eq!(v.get("project").and_then(Value::as_str), Some("beta-app"));
        assert_eq!(
            v.get("release").and_then(|r| r.get("container_prefix")).and_then(Value::as_str),
            Some("beta-app-app")
        );
        assert_eq!(
            v.get("release").and_then(|r| r.get("healthcheck")).and_then(|h| h.get("exec_in")).and_then(Value::as_str),
            Some("beta-app-router")
        );
        assert_eq!(
            v.get("compose").and_then(|c| c.get("env")).and_then(|e| e.get("COMPOSE_PROJECT_NAME")).and_then(Value::as_str),
            Some("beta-app")
        );
    }

    #[test]
    fn default_identity_never_overrides_explicit_values() {
        let mut v = yaml("project: custom\ndeploy_root: /var/www/app\ncompose:\n  env:\n    COMPOSE_PROJECT_NAME: keep");
        default_identity(&mut v);
        assert_eq!(v.get("project").and_then(Value::as_str), Some("custom"));
        assert_eq!(
            v.get("compose").and_then(|c| c.get("env")).and_then(|e| e.get("COMPOSE_PROJECT_NAME")).and_then(Value::as_str),
            Some("keep")
        );
    }

    #[test]
    fn interpolates_var_default_and_errors_on_missing() {
        let e = env(&[("X", "xv")]);
        let mut v = yaml("a: ${X}\nb: ${Y:-fallback}\nc: pre-${X}-post");
        interpolate(&mut v, &e).unwrap();
        assert_eq!(v["a"], Value::String("xv".into()));
        assert_eq!(v["b"], Value::String("fallback".into()));
        assert_eq!(v["c"], Value::String("pre-xv-post".into()));

        let mut missing = yaml("a: ${ZZZ}");
        let err = interpolate(&mut missing, &e).unwrap_err();
        assert!(err.to_string().contains("ZZZ"));
    }

    #[test]
    fn interpolation_keeps_unicode_and_literals() {
        let mut v = yaml("a: 'čen ${X} 🌷'");
        interpolate(&mut v, &env(&[("X", "Z")])).unwrap();
        assert_eq!(v["a"], Value::String("čen Z 🌷".into()));
    }

    #[test]
    fn merge_maps_deep_scalars_and_lists_replace() {
        let mut base = yaml("env: {A: 1, B: 2}\nlist: [1, 2]\nscalar: old");
        let overlay = yaml("env: {B: 9, C: 3}\nlist: [9]\nscalar: new");
        merge_value(&mut base, overlay);
        assert_eq!(base["env"]["A"], yaml("1"));
        assert_eq!(base["env"]["B"], yaml("9"));
        assert_eq!(base["env"]["C"], yaml("3"));
        assert_eq!(base["list"], yaml("[9]"));
        assert_eq!(base["scalar"], Value::String("new".into()));
    }

    #[test]
    fn compose_files_append_helper() {
        let base = yaml("compose: {files: [a.yml]}");
        let mut merged = yaml("compose: {files: [a.yml]}");
        let mut combined = get_sequence(&base, &["compose", "files"]).unwrap();
        combined.extend(get_sequence(&yaml("compose: {files: [b.yml]}"), &["compose", "files"]).unwrap());
        set_sequence(&mut merged, &["compose", "files"], combined);
        assert_eq!(merged["compose"]["files"], yaml("[a.yml, b.yml]"));
    }

    #[test]
    fn set_existing_scalar_only() {
        let mut v = yaml("compose: {env: {APP_ENV: dev}}\nfiles: [a, b]");
        apply_set(&mut v, "compose.env.APP_ENV=prod").unwrap();
        assert_eq!(v["compose"]["env"]["APP_ENV"], Value::String("prod".into()));

        apply_set(&mut v, "files[1]=z").unwrap();
        assert_eq!(v["files"][1], Value::String("z".into()));

        let new_key = apply_set(&mut v, "compose.env.NEW=x").unwrap_err();
        assert!(new_key.to_string().contains("does not exist"));

        let into_map = apply_set(&mut v, "compose=x").unwrap_err();
        assert!(into_map.to_string().contains("not a scalar"));
    }

    #[test]
    fn set_parses_scalar_types() {
        let mut v = yaml("a: 1\nb: text");
        apply_set(&mut v, "a=42").unwrap();
        apply_set(&mut v, "b=hello world").unwrap();
        assert_eq!(v["a"], yaml("42"));
        assert_eq!(v["b"], Value::String("hello world".into()));
    }
}
