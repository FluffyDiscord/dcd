use super::*;

fn env() -> HashMap<String, String> {
    [
        ("REGISTRY", "reg.example.com/app"),
        ("APP_TAG", "app-123"),
        ("DB_TAG", "db-abc"),
        ("DEPLOY_ROOT", "/opt/app"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

fn sample() -> &'static str {
    r#"
version: 2
project: demo
deploy_root: ${DEPLOY_ROOT}
registry: ${REGISTRY}
ssh: deploy@demo.host
compose:
  files: [base.yml]
  env:
    REGISTRY: ${REGISTRY}
release:
  service: app
  container_prefix: demo-app
  healthcheck: { exec_in: nginx, cmd: 'curl -sf http://{container}:2114/health' }
  migrate: { before: 'migrate before', after: 'migrate after' }
cutover:
  service: nginx
  backend_port: 8080
  reload: { exec_in: nginx, cmd: 'nginx -s reload' }
services:
  postgres:
    on_recreate_drain_workers: true
    wait: { cmd: 'pg_isready', retries: 30, interval: 1s }
  nginx:
    recreate: never
workers:
  service: worker
  provider: { command_in_release: 'list-transports' }
retention: { keep_releases: 3 }
stages:
  beta:
    host: beta.host
    compose: { files: [beta.yml], env: { APP_ENV: beta } }
    retention: { keep_releases: 2 }
  prod:
    host: prod.host
    compose: { env: { APP_ENV: prod } }
    retention: { keep_releases: 5 }
"#
}

#[test]
fn loads_and_merges_selected_stage() {
    let c = load(sample(), Some("prod"), &[], &env()).unwrap();
    assert_eq!(c.stage, "prod");
    assert_eq!(c.project, "demo");
    assert_eq!(c.deploy_root, PathBuf::from("/opt/app"));
    assert_eq!(c.registry.as_deref(), Some("reg.example.com/app"));
    assert_eq!(c.ssh.as_deref(), Some("deploy@demo.host"));
    assert_eq!(c.host.as_deref(), Some("prod.host"));
    assert_eq!(c.retention.keep_releases, 5); // stage override
    assert_eq!(c.compose.env["APP_ENV"], "prod"); // stage env merged in
    assert_eq!(c.compose.env["REGISTRY"], "reg.example.com/app"); // base env kept
    // service order preserved (postgres before nginx)
    let names: Vec<&str> = c.services.keys().map(String::as_str).collect();
    assert_eq!(names, vec!["postgres", "nginx"]);
}

#[test]
fn compose_files_append_on_stage() {
    let beta = load(sample(), Some("beta"), &[], &env()).unwrap();
    assert_eq!(beta.compose.files, vec![PathBuf::from("base.yml"), PathBuf::from("beta.yml")]);
    let prod = load(sample(), Some("prod"), &[], &env()).unwrap();
    assert_eq!(prod.compose.files, vec![PathBuf::from("base.yml")]); // prod adds none
}

fn with_keep_images(entry: &str) -> String {
    sample().replace(
        "retention: { keep_releases: 3 }",
        &format!("retention: {{ keep_releases: 3, keep_images: {{ {entry} }} }}"),
    )
}

/// Two paths reach a default: no `retention:` block at all (struct Default) and a
/// block with the key omitted (the serde field default). A test that exercises only
/// the first passes even if a field default is wrong, so both are asserted here.
#[test]
fn retention_defaults_to_one_release_and_one_managed_image() {
    let bare = sample()
        .replace("retention: { keep_releases: 3 }\n", "")
        .replace("    retention: { keep_releases: 5 }\n", "");
    let c = load(&bare, Some("prod"), &[], &env()).unwrap();
    assert_eq!(c.retention.keep_releases, 1);
    assert_eq!(c.retention.keep_managed_images, 1);

    let only_images = bare.replace("stages:", "retention: { keep_managed_images: 4 }\nstages:");
    let c = load(&only_images, Some("prod"), &[], &env()).unwrap();
    assert_eq!(c.retention.keep_releases, 1);
    assert_eq!(c.retention.keep_managed_images, 4);

    let only_releases = bare.replace("stages:", "retention: { keep_releases: 4 }\nstages:");
    let c = load(&only_releases, Some("prod"), &[], &env()).unwrap();
    assert_eq!(c.retention.keep_releases, 4);
    assert_eq!(c.retention.keep_managed_images, 1);
}

#[test]
fn keep_releases_zero_is_rejected_so_a_rollback_target_always_survives() {
    let bare = sample()
        .replace("retention: { keep_releases: 3 }", "retention: { keep_releases: 0 }")
        .replace("    retention: { keep_releases: 5 }\n", "");
    let err = load(&bare, Some("prod"), &[], &env()).unwrap_err();
    assert!(err.to_string().contains("keep_releases"), "{err}");
}

#[test]
fn keep_images_overrides_a_managed_service_image() {
    let c = load(&with_keep_images("database: 1"), Some("prod"), &[], &env()).unwrap();
    assert_eq!(c.retention.keep_images["database"], 1);
    assert_eq!(c.retention.keep_managed_images, 1); // untouched default for the rest
}

#[test]
fn keep_images_rejects_the_release_service_so_the_rollback_target_survives() {
    let err = load(&with_keep_images("app: 0"), Some("prod"), &[], &env()).unwrap_err();
    assert!(err.to_string().contains("it is release.service"), "{err}");
}

#[test]
fn set_override_applies_after_interpolation() {
    let c = load(sample(), Some("prod"), &["retention.keep_releases=9".to_string()], &env()).unwrap();
    assert_eq!(c.retention.keep_releases, 9);
}

#[test]
fn registry_and_deploy_root_default_from_env() {
    // A config that omits both still resolves them from conventional env vars.
    let minimal = r#"
version: 2
project: demo
compose:
  files: [base.yml]
release:
  service: app
  container_prefix: demo-app
  healthcheck: { exec_in: nginx, cmd: 'curl {container}' }
cutover: { service: nginx, backend_port: 80, reload: { exec_in: nginx, cmd: 'r' } }
"#;
    let extra: HashMap<String, String> = [("REGISTRY", "reg.io/app"), ("DEPLOY_ROOT", "/srv/x")]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let c = load(minimal, None, &[], &extra).unwrap();
    assert_eq!(c.registry.as_deref(), Some("reg.io/app"));
    assert_eq!(c.deploy_root, PathBuf::from("/srv/x"));
}

#[test]
fn missing_var_is_config_error() {
    let mut e = env();
    e.remove("REGISTRY");
    let err = load(sample(), Some("prod"), &[], &e).unwrap_err();
    assert_eq!(err.exit_code(), 2);
    assert!(err.to_string().contains("REGISTRY"));
}

#[test]
fn unknown_key_is_rejected() {
    let bad = sample().replace("project: demo", "project: demo\nservces: oops");
    let err = load(&bad, Some("prod"), &[], &env()).unwrap_err();
    assert_eq!(err.exit_code(), 2);
}

/// Every v1 key names its v2 replacement instead of surfacing as "unknown field",
/// which would send the reader to the typo row of the AGENTS.md table.
#[test]
fn every_removed_v1_key_gets_a_targeted_migration_error() {
    let top_level = [
        ("docker:\n  images: { app: x }\n", "container identity now comes from your compose file"),
        ("network: demo_net\n", "networks come from your compose file"),
    ];
    for (snippet, expected) in top_level {
        let bad = sample().replace("project: demo\n", &format!("project: demo\n{snippet}"));
        let err = load(&bad, Some("prod"), &[], &env()).unwrap_err().to_string();
        assert!(err.contains(expected), "for {snippet:?} got: {err}");
        assert!(err.contains("UPGRADE.md"), "for {snippet:?} got: {err}");
    }

    let under_workers = [
        ("  template: { image: app }\n", "ONE compose service"),
        ("  compose_file: workers.yml\n", "no longer renders a workers compose file"),
        ("  name_filter: 'w-'\n", "renamed to `workers.name_prefix`"),
    ];
    for (snippet, expected) in under_workers {
        let bad = sample().replace("  service: worker\n", &format!("  service: worker\n{snippet}"));
        let err = load(&bad, Some("prod"), &[], &env()).unwrap_err().to_string();
        assert!(err.contains(expected), "for {snippet:?} got: {err}");
        assert!(err.contains("UPGRADE.md"), "for {snippet:?} got: {err}");
    }

    // A removed key hiding in a stage must be caught too, not only in the base.
    let in_stage = sample().replace("  prod:\n", "  prod:\n    network: demo_net\n");
    let err = load(&in_stage, Some("prod"), &[], &env()).unwrap_err().to_string();
    assert!(err.contains("networks come from your compose file"), "got: {err}");
}

/// INV-14: sharing a service with the release would make worker drain
/// `docker stop` the container serving traffic.
#[test]
fn workers_may_not_share_the_release_service() {
    let bad = sample().replace("  service: worker\n", "  service: app\n");
    let err = load(&bad, Some("prod"), &[], &env()).unwrap_err().to_string();
    assert!(err.contains("must not equal release.service"), "got: {err}");
    assert!(err.contains("INV-14"), "got: {err}");
}

#[test]
fn release_needs_exactly_one_of_service_or_run() {
    let both = sample().replace("  service: app\n", "  service: app\n  run: { image: x }\n");
    let err = load(&both, Some("prod"), &[], &env()).unwrap_err().to_string();
    assert!(err.contains("not both"), "got: {err}");

    let neither = sample().replace("  service: app\n", "");
    let err = load(&neither, Some("prod"), &[], &env()).unwrap_err().to_string();
    assert!(err.contains("release needs service:"), "got: {err}");
}

/// The chown runs as root inside a container mounting deploy_root, so an
/// escaping path is a privilege bug, not a typo.
#[test]
fn directory_paths_may_not_escape_deploy_root() {
    for path in ["../../etc", "/etc"] {
        let bad = sample().replace("project: demo\n", &format!("project: demo\ndirectories:\n  - {{ path: {path} }}\n"));
        let err = load(&bad, Some("prod"), &[], &env()).unwrap_err().to_string();
        assert!(err.contains("directories path"), "for {path} got: {err}");
    }
}

/// ssh takes the destination positionally, so a leading `-` makes it an option:
/// `-oProxyCommand=` then runs on the deploying machine, not the target.
#[test]
fn an_ssh_target_that_looks_like_an_option_is_rejected() {
    for target in ["-oProxyCommand=touch /tmp/pwned", "-F/tmp/evil"] {
        let bad = sample().replace("ssh: deploy@demo.host\n", &format!("ssh: '{target}'\n"));
        let err = load(&bad, Some("prod"), &[], &env()).unwrap_err().to_string();
        assert!(err.contains("starts with '-'"), "for {target} got: {err}");
    }

    let good = load(sample(), Some("prod"), &[], &env()).unwrap();
    assert_eq!(good.ssh.as_deref(), Some("deploy@demo.host"));
}

#[test]
fn healthcheck_without_container_placeholder_is_rejected() {
    let bad = sample().replace("http://{container}:2114/health", "http://app-rr:2114/health");
    let err = load(&bad, Some("prod"), &[], &env()).unwrap_err();
    assert!(err.to_string().contains("{container}"));
}

/// An overlapping prefix makes `docker ps --filter name=` (an unanchored match)
/// sweep the other set's containers.
#[test]
fn worker_and_release_name_prefixes_may_not_overlap() {
    let bad = sample().replace("  service: worker\n", "  service: worker\n  name_prefix: demo-app-w\n");
    let err = load(&bad, Some("prod"), &[], &env()).unwrap_err().to_string();
    assert!(err.contains("overlaps release container prefix"), "got: {err}");
}

#[test]
fn ambiguous_and_missing_stage_errors() {
    let ambiguous = load(sample(), None, &[], &env()).unwrap_err();
    assert!(ambiguous.to_string().contains("multiple stages"));
    let missing = load(sample(), Some("staging"), &[], &env()).unwrap_err();
    assert!(missing.to_string().contains("not found"));
}

#[test]
fn durations_parse_suffixes() {
    let c = load(sample(), Some("prod"), &[], &env()).unwrap();
    let pg = &c.services["postgres"];
    assert_eq!(pg.wait.as_ref().unwrap().interval, 1);
    assert_eq!(c.workers.as_ref().unwrap().stop_timeout, 120); // default 120s
}

#[test]
fn removed_compose_env_file_gets_targeted_error() {
    let top_level = sample().replace("compose:\n", "compose:\n  env_file: compose.env\n");
    let err = load(&top_level, Some("prod"), &[], &env()).unwrap_err();
    assert!(err.to_string().contains("compose.env_file was removed"), "got: {err}");
    assert!(err.to_string().contains("UPGRADE.md"));

    let in_stage = sample().replace(
        "stages:\n",
        "stages:\n  legacy:\n    compose: { env_file: old.env }\n",
    );
    let err = load(&in_stage, Some("prod"), &[], &env()).unwrap_err();
    assert!(err.to_string().contains("compose.env_file was removed"), "got: {err}");
}

#[test]
fn env_map_keys_are_charset_validated_and_reserved_guarded() {
    let bad_key = sample().replace("env:\n", "env:\n    'BAD KEY': x\n");
    let err = load(&bad_key, Some("prod"), &[], &env()).unwrap_err();
    assert!(err.to_string().contains("invalid env key `BAD KEY`"), "got: {err}");

    let hijack = sample().replace("env:\n", "env:\n    DOCKER_HOST: tcp://evil:2375\n");
    let err = load(&hijack, Some("prod"), &[], &env()).unwrap_err();
    assert!(err.to_string().contains("DOCKER_HOST"), "got: {err}");
    assert!(err.to_string().contains("reserved"), "got: {err}");

    // COMPOSE_* stays legitimate in compose.env
    let compose_ok = sample().replace("env:\n", "env:\n    COMPOSE_IGNORE_ORPHANS: 'true'\n");
    assert!(load(&compose_ok, Some("prod"), &[], &env()).is_ok());
}

fn model_json(app_health: bool, pg_health: bool) -> crate::compose::ComposeModel {
    let health = r#","healthcheck":{"test":["CMD","true"]}"#;
    let json = format!(
        r#"{{"services":{{
            "app":{{"image":"a"{}}},
            "worker":{{"image":"a"}},
            "postgres":{{"image":"p","container_name":"demo-postgres"{}}},
            "nginx":{{"container_name":"demo-nginx","healthcheck":{{"test":["CMD","true"]}}}}
        }}}}"#,
        if app_health { health } else { "" },
        if pg_health { health } else { "" },
    );
    crate::compose::ComposeModel::from_json(json.as_bytes()).unwrap()
}

/// A managed service with no health gate is an ERROR, not a warning: `compose up
/// --wait` returns as soon as the container is RUNNING when the service declares
/// no `healthcheck:`, so dcd would cut over to a database that is not ready.
#[test]
fn a_managed_service_without_a_health_gate_is_rejected() {
    let no_probe = sample().replace("    wait: { cmd: 'pg_isready', retries: 30, interval: 1s }\n", "");
    let c = load(&no_probe, Some("prod"), &[], &env()).unwrap();
    assert!(c.services["postgres"].wait.is_none());

    let err = validate_with_model(&c, &model_json(true, false)).unwrap_err().to_string();
    assert!(err.contains("service 'postgres' has no health gate"), "got: {err}");
    assert!(err.contains("not ready yet"), "the error must say why it matters: {err}");

    // a compose healthcheck satisfies it...
    validate_with_model(&c, &model_json(true, true)).unwrap();
}

/// ...and so does an explicit `wait:` probe, which is the override for services
/// that cannot declare one.
#[test]
fn an_explicit_wait_probe_satisfies_the_health_gate() {
    // postgres declares no compose healthcheck in this model, but the sample gives
    // it a `wait:` probe — which is exactly the sanctioned override.
    let c = load(sample(), Some("prod"), &[], &env()).unwrap();
    assert!(c.services["postgres"].wait.is_some());
    validate_with_model(&c, &model_json(true, false)).unwrap();

    let with_probe = sample().replace(
        "  nginx:\n    recreate: never\n",
        "  nginx:\n    recreate: never\n    wait: { cmd: 'test -f /var/run/nginx.pid' }\n",
    );
    let c = load(&with_probe, Some("prod"), &[], &env()).unwrap();
    assert!(c.services["nginx"].wait.is_some());
}

/// The release needs a gate too, or the cutover has nothing to wait on.
#[test]
fn a_release_without_any_health_gate_is_rejected() {
    let no_escape_hatch = sample().replace(
        "  healthcheck: { exec_in: nginx, cmd: 'curl -sf http://{container}:2114/health' }\n",
        "",
    );
    let c = load(&no_escape_hatch, Some("prod"), &[], &env()).unwrap();

    let err = validate_with_model(&c, &model_json(false, true)).unwrap_err().to_string();
    assert!(err.contains("release service 'app' has no health gate"), "got: {err}");

    // either the compose healthcheck or the escape hatch is enough
    validate_with_model(&c, &model_json(true, true)).unwrap();
    let with_hatch = load(sample(), Some("prod"), &[], &env()).unwrap();
    validate_with_model(&with_hatch, &model_json(false, true)).unwrap();
}

/// A reference to a service the compose file does not define fails here, naming
/// the services that do exist — not twenty steps later as a timeout.
#[test]
fn a_service_reference_missing_from_compose_is_rejected() {
    let bad = sample().replace("  service: nginx\n", "  service: ghost\n");
    let c = load(&bad, Some("prod"), &[], &env()).unwrap();
    let err = validate_with_model(&c, &model_json(true, true)).unwrap_err().to_string();
    assert!(err.contains("cutover.service 'ghost' is not a service"), "got: {err}");
    assert!(err.contains("nginx"), "the error must list what does exist: {err}");
}

/// The shipped worked example must satisfy its own rules — including the
/// mandatory health gates — against compose's REAL resolved output.
#[test]
fn the_worked_example_passes_model_validation() {
    let src = include_str!("../../docs/examples/roadrunner_app/dcd.yaml");
    let e: HashMap<String, String> = [
        ("DEPLOY_SSH", "deploy@prod.example.internal"),
        ("DEPLOY_ROOT", "/srv/acme"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let cfg = load(src, Some("prod"), &[], &e).unwrap();
    let model =
        crate::compose::ComposeModel::from_json(include_bytes!("../testdata/roadrunner-model.json")).unwrap();
    validate_with_model(&cfg, &model).unwrap_or_else(|err| panic!("the shipped example must validate: {err}"));
}

/// The full field reference must itself be a valid config: it is what every
/// reader copies from.
#[test]
fn the_all_in_one_reference_config_is_valid() {
    let src = include_str!("../../docs/examples/all_in_one/dcd.yaml");
    let cfg = load(src, Some("prod"), &[], &HashMap::new()).unwrap();
    assert_eq!(cfg.project, "allinone");
    assert_eq!(cfg.release.service.as_deref(), Some("app"));
    assert_eq!(cfg.cutover.service, "web");
    assert_eq!(cfg.workers.as_ref().unwrap().service, "worker");
    assert_eq!(cfg.compose.profiles, vec!["dcd-release".to_string()]);
    // and the beta stage merges cleanly on top
    let beta = load(src, Some("beta"), &[], &HashMap::new()).unwrap();
    assert_eq!(beta.retention.keep_releases, 3);
    assert_eq!(beta.compose.files.len(), 2);
}

/// Every shipped example must load and validate — they are what readers copy.
/// The fpm example exercises the other shape: two stages, a stage-only service,
/// and a release whose gate is the escape hatch rather than a compose healthcheck.
#[test]
fn the_fpm_example_is_valid_in_both_stages() {
    let src = include_str!("../../docs/examples/fpm_app/dcd.yaml");
    let e: HashMap<String, String> = [("DEPLOY_SSH", "deploy@fpm.example"), ("DEPLOY_ROOT", "/srv/webapp")]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

    let prod = load(src, Some("prod"), &[], &e).unwrap();
    assert_eq!(prod.project, "webapp");
    assert_eq!(prod.release.container_prefix(&prod.project), "webapp-app");
    assert!(prod.release.healthcheck.is_some(), "the fpm app probes from the router");
    assert!(!prod.services.contains_key("mailpit"), "mailpit is beta-only");

    let beta = load(src, Some("beta"), &[], &e).unwrap();
    assert_eq!(beta.project, "beta-webapp");
    assert_eq!(beta.release.container_prefix(&beta.project), "beta-webapp-app");
    assert!(beta.services.contains_key("mailpit"), "the stage merges its own service in");
    assert_eq!(beta.compose.files.len(), 2, "stage compose files APPEND");
}

/// C3's regression guard. `dcd schema` shipped describing the MERGED config: no
/// `stages` key, `additionalProperties: false`, and `required` on keys a stage may
/// legitimately supply — so it rejected every config in the repo. This walks the
/// shipped examples through the emitted schema the way an editor would.
#[test]
fn the_emitted_schema_describes_every_shipped_example() {
    let schema = crate::config::authoring_schema().unwrap();
    let properties = schema["properties"].as_object().expect("root properties");
    let stage_properties = schema["properties"]["stages"]["additionalProperties"]["properties"]
        .as_object()
        .expect("stage override properties");

    assert_eq!(schema["required"], serde_json::json!(["stages"]), "stages is the one key every config must have");

    for example in [
        "docs/examples/all_in_one/dcd.yaml",
        "docs/examples/roadrunner_app/dcd.yaml",
        "docs/examples/fpm_app/dcd.yaml",
    ] {
        let source = std::fs::read_to_string(example).unwrap_or_else(|e| panic!("{example}: {e}"));
        let document: serde_norway::Mapping = serde_norway::from_str(&source).unwrap_or_else(|e| panic!("{example}: {e}"));

        for key in document.keys() {
            let key = key.as_str().expect("a scalar key");
            assert!(properties.contains_key(key), "{example}: the schema does not describe `{key}`");
        }

        let stages = document.get(serde_norway::Value::String("stages".into()));
        let stages = stages.and_then(serde_norway::Value::as_mapping).unwrap_or_else(|| panic!("{example}: no stages"));
        for (name, body) in stages {
            let Some(body) = body.as_mapping() else { continue };
            for key in body.keys() {
                let key = key.as_str().expect("a scalar key");
                assert!(
                    stage_properties.contains_key(key),
                    "{example}: stage {name:?} sets `{key}`, which the override schema does not describe"
                );
            }
        }
    }
}

/// A duration is written `2s` as often as `2`, and the field reference uses the
/// string form throughout. A schema deriving the bare `u64` red-underlines it.
#[test]
fn the_emitted_schema_accepts_both_duration_forms() {
    let schema = crate::config::authoring_schema().unwrap();
    let interval = &schema["$defs"]["Healthcheck"]["properties"]["interval"];
    let forms = interval["anyOf"].as_array().expect("a duration accepts two forms");
    assert!(forms.iter().any(|form| form["type"] == "integer"), "{interval}");
    assert!(forms.iter().any(|form| form["type"] == "string"), "{interval}");
}

/// Relaxing `required` is right for the deep-merged objects and wrong inside a
/// union: a hook action is IDENTIFIED by its key, so stripping it there would let
/// `exec_inn:` validate as a valid action.
#[test]
fn the_emitted_schema_still_identifies_hook_action_variants() {
    let schema = crate::config::authoring_schema().unwrap();
    let action = &schema["$defs"]["HookAction"];
    let branches = action["anyOf"].as_array().expect("hook actions are a union");
    let identified = branches
        .iter()
        .filter(|branch| branch.get("required").is_some())
        .count();
    assert!(identified > 0, "no branch names its key, so any object validates: {action}");
}

/// Port 0 is what the scaffold writes when the release publishes none. Asserted on
/// a config with nothing else wrong, so the placeholder guard cannot mask it.
#[test]
fn an_unset_cutover_backend_port_is_refused() {
    let source = r#"
version: 2
project: demo
deploy_root: /srv/demo
compose:
  files: [compose.yml]
release:
  service: app
  healthcheck: { exec_in: nginx, cmd: 'curl -sf http://{container}/health' }
cutover:
  service: nginx
  backend_port: 0
  reload: { exec_in: nginx, cmd: 'nginx -s reload' }
stages: { prod: {} }
"#;
    let error = crate::config::load(source, Some("prod"), &[], &std::collections::HashMap::new())
        .unwrap_err()
        .to_string();
    assert!(error.contains("cutover.backend_port is unset"), "{error}");
}

/// `check` claims "every referenced service exists", and it now means it. These
/// three classes used to pass validation and fail at the step that ran them — for
/// an `after_cutover` hook, past the point of no return, as exit 4.
#[test]
fn every_service_reference_is_validated_not_just_the_cutover_path() {
    let base = sample();

    let bad_wait = base.replace(
        "    wait: { cmd: 'pg_isready', retries: 30, interval: 1s }",
        "    wait: { exec_in: no-such-service, cmd: 'pg_isready' }",
    );
    let c = load(&bad_wait, Some("prod"), &[], &env()).unwrap();
    let err = validate_with_model(&c, &model_json(true, true)).unwrap_err().to_string();
    assert!(err.contains("services.postgres.wait.exec_in 'no-such-service'"), "got: {err}");

    let bad_hook = format!("{base}\nhooks:\n  after_cutover:\n    - exec_in: {{ service: ghost, cmd: 'true' }}\n");
    let c = load(&bad_hook, Some("prod"), &[], &env()).unwrap();
    let err = validate_with_model(&c, &model_json(true, true)).unwrap_err().to_string();
    assert!(err.contains("hooks.after_cutover.exec_in 'ghost'"), "got: {err}");

    let bad_keep = base.replace(
        "retention: { keep_releases: 3 }",
        "retention: { keep_releases: 3, keep_images: { ghost-image: 2 } }",
    );
    let c = load(&bad_keep, Some("prod"), &[], &env()).unwrap();
    let err = validate_with_model(&c, &model_json(true, true)).unwrap_err().to_string();
    assert!(err.contains("retention.keep_images 'ghost-image'"), "got: {err}");
}
