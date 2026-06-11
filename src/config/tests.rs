use super::*;

fn env() -> HashMap<String, String> {
    [
        ("REGISTRY", "reg.example.com/app"),
        ("APP_TAG", "app-123"),
        ("DB_TAG", "db-abc"),
        ("DEPLOY_ROOT", "/opt/app"),
        ("MAXMIND_LICENSE_KEY", "s3cr3t-key"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

fn sample() -> &'static str {
    r#"
version: 1
project: demo
deploy_root: ${DEPLOY_ROOT}
network: demo_net
registry: ${REGISTRY}
images:
  app: ${APP_TAG}
  database: ${DB_TAG}
compose:
  files: [base.yml]
  env:
    REGISTRY: ${REGISTRY}
    MAXMIND_LICENSE_KEY: ${MAXMIND_LICENSE_KEY}
services:
  postgres:
    image: database
    container: demo-postgres
    on_recreate_drain_workers: true
    wait: { exec_in: demo-postgres, cmd: 'pg_isready', retries: 30, interval: 1s }
  nginx:
    image: ~
    container: demo-nginx
    recreate: never
release:
  image: app
  container_prefix: demo-app
  healthcheck: { exec_in: demo-nginx, cmd: 'curl -sf http://{container}:2114/health' }
  migrate: { before: 'migrate before', after: 'migrate after' }
cutover:
  backend_port: 8080
  reload: { exec_in: demo-nginx, cmd: 'nginx -s reload' }
workers:
  compose_file: workers.yml
  provider: { command_in_release: 'list-transports' }
  template:
    image: app
    entrypoint: ['php', 'consume']
    command: ['{name}']
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
    let loaded = load(sample(), Some("prod"), &[], &env()).unwrap();
    let c = loaded.config;
    assert_eq!(c.stage, "prod");
    assert_eq!(c.project, "demo");
    assert_eq!(c.deploy_root, PathBuf::from("/opt/app"));
    assert_eq!(c.registry.as_deref(), Some("reg.example.com/app"));
    assert_eq!(c.images["app"], "app-123");
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
    let beta = load(sample(), Some("beta"), &[], &env()).unwrap().config;
    assert_eq!(beta.compose.files, vec![PathBuf::from("base.yml"), PathBuf::from("beta.yml")]);
    let prod = load(sample(), Some("prod"), &[], &env()).unwrap().config;
    assert_eq!(prod.compose.files, vec![PathBuf::from("base.yml")]); // prod adds none
}

#[test]
fn set_override_applies_after_interpolation() {
    let c = load(sample(), Some("prod"), &["retention.keep_releases=9".to_string()], &env())
        .unwrap()
        .config;
    assert_eq!(c.retention.keep_releases, 9);
}

#[test]
fn missing_var_is_config_error() {
    let mut e = env();
    e.remove("APP_TAG");
    let err = load(sample(), Some("prod"), &[], &e).unwrap_err();
    assert_eq!(err.exit_code(), 2);
    assert!(err.to_string().contains("APP_TAG"));
}

#[test]
fn unknown_key_is_rejected() {
    let bad = sample().replace("project: demo", "project: demo\nservces: oops");
    let err = load(&bad, Some("prod"), &[], &env()).unwrap_err();
    assert_eq!(err.exit_code(), 2);
}

#[test]
fn unknown_image_ref_is_rejected() {
    let bad = sample().replace("image: app\n  container_prefix", "image: ghost\n  container_prefix");
    let err = load(&bad, Some("prod"), &[], &env()).unwrap_err();
    assert!(err.to_string().contains("ghost"));
}

#[test]
fn healthcheck_without_container_placeholder_is_rejected() {
    let bad = sample().replace("http://{container}:2114/health", "http://app-rr:2114/health");
    let err = load(&bad, Some("prod"), &[], &env()).unwrap_err();
    assert!(err.to_string().contains("{container}"));
}

#[test]
fn exec_in_unknown_container_is_rejected() {
    let bad = sample().replace("exec_in: demo-nginx, cmd: 'nginx -s reload'", "exec_in: ghost-c, cmd: 'nginx -s reload'");
    let err = load(&bad, Some("prod"), &[], &env()).unwrap_err();
    assert!(err.to_string().contains("ghost-c"));
}

#[test]
fn ambiguous_and_missing_stage_errors() {
    let ambiguous = load(sample(), None, &[], &env()).unwrap_err();
    assert!(ambiguous.to_string().contains("multiple stages"));
    let missing = load(sample(), Some("staging"), &[], &env()).unwrap_err();
    assert!(missing.to_string().contains("not found"));
}

#[test]
fn redactor_collects_secret_values_by_key() {
    let loaded = load(sample(), Some("prod"), &[], &env()).unwrap();
    let masked = loaded.redactor.apply("token=s3cr3t-key in command");
    assert_eq!(masked, "token=*** in command");
}

#[test]
fn durations_parse_suffixes() {
    let c = load(sample(), Some("prod"), &[], &env()).unwrap().config;
    let pg = &c.services["postgres"];
    assert_eq!(pg.wait.as_ref().unwrap().interval, 1);
    assert_eq!(c.workers.as_ref().unwrap().stop_timeout, 120); // default 120s
}
