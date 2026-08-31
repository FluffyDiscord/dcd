use super::*;

fn write(directory: &Path, body: &str) -> std::path::PathBuf {
    let path = directory.join("docker-compose.prod.yml");
    std::fs::write(&path, body).unwrap();
    path
}

fn scratch(tag: &str) -> std::path::PathBuf {
    let directory = std::env::temp_dir().join(format!("dcd-scaffold-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    directory
}

const TYPICAL: &str = r#"
services:
  app:
    image: registry.example.com/team/app:${APP_TAG}
    profiles: ["dcd-release"]
    ports:
      - "8080"
  nginx:
    image: nginx:1.27-alpine
    healthcheck:
      test: ["CMD", "true"]
  postgres:
    image: postgres:16
    healthcheck:
      test: ["CMD", "pg_isready"]
"#;

/// The guesses the spec asks for: the release behind the `dcd-release` profile,
/// the router by image, the port from the release's own `ports:`.
#[test]
fn a_typical_compose_file_fills_in_every_field_that_can_be_inferred() {
    let directory = scratch("typical");
    let scaffold = Scaffold::from_compose(&write(&directory, TYPICAL), &directory.join("dcd.yaml")).unwrap();
    let rendered = scaffold.render();

    assert!(rendered.contains("version: 2"), "{rendered}");
    assert!(rendered.contains("  service: app\n"), "{rendered}");
    assert!(rendered.contains("  service: nginx\n"), "{rendered}");
    assert!(rendered.contains("backend_port: 8080"), "{rendered}");
    assert!(rendered.contains("- docker-compose.prod.yml"), "{rendered}");
    assert!(rendered.contains("  postgres:\n    recreate: on-image-change"), "{rendered}");
    let _ = std::fs::remove_dir_all(&directory);
}

/// An unresolved `${APP_TAG}` must not stop the scaffold: needing a working
/// environment and a running daemon to produce a starter config would defeat it.
#[test]
fn an_unresolved_interpolation_does_not_stop_the_scaffold() {
    let directory = scratch("interp");
    let scaffold = Scaffold::from_compose(&write(&directory, TYPICAL), &directory.join("dcd.yaml")).unwrap();
    assert!(scaffold.render().contains("service: app"));
    let _ = std::fs::remove_dir_all(&directory);
}

/// The health gate is mandatory, so a service without one has to be called out
/// where the operator will see it — in the file AND on stdout.
#[test]
fn a_service_without_a_healthcheck_is_flagged_in_the_config_and_in_the_notes() {
    let directory = scratch("ungated");
    let scaffold = Scaffold::from_compose(&write(
        &directory,
        r#"
services:
  app:
    image: app:1
    ports: ["9000:9000"]
  nginx:
    image: nginx:alpine
  redis:
    image: redis:7
"#,
    ), &directory.join("dcd.yaml"))
    .unwrap();

    let rendered = scaffold.render();
    assert!(rendered.contains("declares no `healthcheck:`"), "{rendered}");
    assert!(rendered.contains("backend_port: 9000"), "the container side of the mapping wins: {rendered}");

    let notes = scaffold.notes().join("\n");
    assert!(notes.contains("release.service: app"), "{notes}");
    assert!(notes.contains("cutover.service: nginx"), "{notes}");
    assert!(notes.contains("app") && notes.contains("nginx") && notes.contains("redis"), "{notes}");
    let _ = std::fs::remove_dir_all(&directory);
}

/// Nothing may be guessed silently: with no conventional name, no profile and two
/// port-publishing candidates, both fields stay TODO.
#[test]
fn an_ambiguous_compose_file_leaves_todo_rather_than_guessing() {
    let directory = scratch("ambiguous");
    let scaffold = Scaffold::from_compose(&write(
        &directory,
        r#"
services:
  alpha:
    image: alpha:1
    ports: ["1000:1000"]
  beta:
    image: beta:1
    ports: ["2000:2000"]
"#,
    ), &directory.join("dcd.yaml"))
    .unwrap();

    let rendered = scaffold.render();
    assert!(rendered.contains(r#"service: "TODO: the compose service to deploy red-black""#), "{rendered}");
    assert!(rendered.contains(r#"service: "TODO: the compose service that routes traffic""#), "{rendered}");

    let notes = scaffold.notes().join("\n");
    assert!(notes.contains("could not be guessed"), "{notes}");
    let _ = std::fs::remove_dir_all(&directory);
}

/// Long-form port syntax is as common as the string form in real compose files.
#[test]
fn a_long_form_port_mapping_is_read_for_the_backend_port() {
    let directory = scratch("longport");
    let scaffold = Scaffold::from_compose(&write(
        &directory,
        r#"
services:
  app:
    image: app:1
    ports:
      - target: 8000
        published: 80
  caddy:
    image: caddy:2
"#,
    ), &directory.join("dcd.yaml"))
    .unwrap();
    assert!(scaffold.render().contains("backend_port: 8000"), "{}", scaffold.render());
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn a_file_that_is_not_a_compose_file_is_a_config_error() {
    let directory = scratch("garbage");
    let path = directory.join("nope.yml");
    std::fs::write(&path, "services: []\n").unwrap();
    assert!(Scaffold::from_compose(&path, &directory.join("dcd.yaml")).is_err());

    let empty = directory.join("empty.yml");
    std::fs::write(&empty, "services: {}\n").unwrap();
    let error = Scaffold::from_compose(&empty, &directory.join("dcd.yaml")).unwrap_err().to_string();
    assert!(error.contains("declares no services"), "{error}");
    let _ = std::fs::remove_dir_all(&directory);
}

/// The scaffold's whole value is being a file you can open and edit — an
/// unquoted `TODO: ...` in value position makes it not YAML at all, and `dcd
/// check` then reports a parse error instead of the fields still to fill in.
#[test]
fn everything_the_scaffold_renders_is_valid_yaml() {
    let directory = scratch("yaml");
    let cases = [
        TYPICAL,
        "services:\n  alpha:\n    image: alpha:1\n    ports: [\"1:1\"]\n  beta:\n    image: beta:1\n    ports: [\"2:2\"]\n",
        "services:\n  lonely:\n    image: lonely:1\n",
    ];
    for (index, body) in cases.iter().enumerate() {
        let path = directory.join(format!("compose-{index}.yml"));
        std::fs::write(&path, body).unwrap();
        let rendered = Scaffold::from_compose(&path, &directory.join("dcd.yaml")).unwrap().render();
        serde_norway::from_str::<serde_norway::Value>(&rendered)
            .unwrap_or_else(|e| panic!("case {index} is not YAML: {e}\n{rendered}"));
    }
    let _ = std::fs::remove_dir_all(&directory);
}

/// A router built in CI is `${REGISTRY}:${ROUTER_TAG}` — the image names nothing,
/// so the service name has to carry the guess.
#[test]
fn a_router_is_recognised_by_its_service_name_when_its_image_is_a_variable() {
    let directory = scratch("varrouter");
    let scaffold = Scaffold::from_compose(&write(
        &directory,
        r#"
services:
  app:
    image: ${REGISTRY}:${APP_TAG}
    ports: ["8080"]
  router:
    image: ${REGISTRY}:${ROUTER_TAG:-router-latest}
"#,
    ), &directory.join("dcd.yaml"))
    .unwrap();
    let rendered = scaffold.render();
    assert!(rendered.contains("  service: router
"), "{rendered}");
    assert!(!rendered.contains("  router:
    recreate"), "the router is not a side container: {rendered}");
    let _ = std::fs::remove_dir_all(&directory);
}

/// The loop the two features close together: scaffold a config, and `dcd check`
/// names exactly the fields still to fill rather than accepting them.
#[test]
fn check_names_the_fields_the_scaffold_left_unfilled() {
    let directory = scratch("checkloop");
    let rendered = Scaffold::from_compose(&write(&directory, TYPICAL), &directory.join("dcd.yaml")).unwrap().render();

    let error = crate::config::load(&rendered, Some("prod"), &[], &std::collections::HashMap::new())
        .unwrap_err()
        .to_string();
    assert!(error.contains("still scaffolded"), "{error}");
    assert!(error.contains("ssh"), "{error}");
    assert!(error.contains("deploy_root"), "{error}");
    assert!(error.contains("cutover.reload.cmd"), "{error}");
    let _ = std::fs::remove_dir_all(&directory);
}

/// The guard matches the exact marker the scaffold writes. A command that merely
/// BEGINS with those four letters is not a placeholder — this value is chosen so
/// the test fails if the predicate is loosened back to `"TODO"`.
#[test]
fn a_filled_in_config_is_not_mistaken_for_a_scaffold() {
    let source = r#"
version: 2
project: demo
deploy_root: /srv/demo
compose:
  files: [compose.yml]
release:
  service: app
  healthcheck: { exec_in: nginx, cmd: 'curl -sf http://{container}/health' }
  drain: 'TODO_FLUSH=1 php bin/console app:drain'
cutover:
  service: nginx
  backend_port: 8080
  reload: { exec_in: nginx, cmd: 'nginx -s reload' }
stages: { prod: {} }
"#;
    let cfg = crate::config::load(source, Some("prod"), &[], &std::collections::HashMap::new()).unwrap();
    assert_eq!(cfg.release.service.as_deref(), Some("app"));
}

/// `compose.files` is resolved relative to the CONFIG, not to wherever the compose
/// file was found. Emitting only the file name silently dropped `infra/` and made
/// the generated config unresolvable.
#[test]
fn a_compose_file_in_a_subdirectory_keeps_its_path() {
    let directory = scratch("subdir");
    let nested = directory.join("infra");
    std::fs::create_dir_all(&nested).unwrap();
    let compose = nested.join("docker-compose.yml");
    std::fs::write(&compose, TYPICAL).unwrap();

    let scaffold = Scaffold::from_compose(&compose, &directory.join("dcd.yaml")).unwrap();
    let rendered = scaffold.render();
    assert!(rendered.contains("- infra/docker-compose.yml"), "{rendered}");
    let _ = std::fs::remove_dir_all(&directory);
}

/// `project:` names the deploy, so it comes from where the CONFIG lands — not
/// from the compose file's directory, which may be `infra/`.
#[test]
fn the_project_name_comes_from_the_config_directory() {
    let directory = scratch("projectname");
    let nested = directory.join("infra");
    std::fs::create_dir_all(&nested).unwrap();
    let compose = nested.join("docker-compose.yml");
    std::fs::write(&compose, TYPICAL).unwrap();

    let rendered = Scaffold::from_compose(&compose, &directory.join("dcd.yaml")).unwrap().render();
    assert!(!rendered.contains("project: infra"), "{rendered}");
    let expected = directory.file_name().unwrap().to_string_lossy().to_lowercase();
    assert!(rendered.contains(&format!("project: {expected}")), "{rendered}");
    let _ = std::fs::remove_dir_all(&directory);
}

/// The commonest invocation of all: both files in the working directory, the
/// compose path with no directory component at all. Asserted on the path logic
/// alone — `set_current_dir` is process-global and would race the other tests.
#[test]
fn a_bare_compose_filename_is_emitted_unchanged() {
    let directory = scratch("bare");
    let compose = directory.join("docker-compose.yml");
    std::fs::write(&compose, TYPICAL).unwrap();

    let rendered = Scaffold::from_compose(&compose, &directory.join("dcd.yaml")).unwrap().render();
    assert!(rendered.contains("- docker-compose.yml"), "{rendered}");

    assert_eq!(
        Scaffold::compose_reference(Path::new("docker-compose.yml"), Path::new("dcd.yaml")),
        "docker-compose.yml"
    );
    let _ = std::fs::remove_dir_all(&directory);
}

/// A compose file whose release publishes no ports cannot yield a backend port.
/// The scaffold must still emit valid YAML, and `dcd check` must refuse it rather
/// than let the router be pointed at port 0.
#[test]
fn a_scaffold_with_no_inferable_port_is_refused_by_check() {
    let directory = scratch("noport");
    let rendered = Scaffold::from_compose(
        &write(
            &directory,
            r#"
services:
  app:
    image: app:1
    profiles: ["dcd-release"]
  nginx:
    image: nginx:alpine
"#,
        ),
        &directory.join("dcd.yaml"),
    )
    .unwrap()
    .render();

    serde_norway::from_str::<serde_norway::Value>(&rendered).expect("still valid YAML");

    let error = crate::config::load(&rendered, Some("prod"), &[], &std::collections::HashMap::new())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("still scaffolded") || error.contains("cutover.backend_port is unset"),
        "{error}"
    );
    let _ = std::fs::remove_dir_all(&directory);
}

/// A compose file one level ABOVE the config is an ordinary layout. Baking the
/// absolute path in would make the committed config resolve on one machine only.
#[test]
fn a_compose_file_above_the_config_is_addressed_relatively() {
    let directory = scratch("above");
    let nested = directory.join("deploy");
    std::fs::create_dir_all(&nested).unwrap();
    let compose = directory.join("docker-compose.yml");
    std::fs::write(&compose, TYPICAL).unwrap();

    let rendered = Scaffold::from_compose(&compose, &nested.join("dcd.yaml")).unwrap().render();
    assert!(rendered.contains("- ../docker-compose.yml"), "{rendered}");
    assert!(!rendered.contains(&directory.display().to_string()), "no absolute path: {rendered}");
    let _ = std::fs::remove_dir_all(&directory);
}

/// A filename carrying a YAML metacharacter must not reparse as a different list.
#[test]
fn a_compose_filename_with_a_metacharacter_survives_the_round_trip() {
    let directory = scratch("meta");
    let compose = directory.join("in,fra#1.yml");
    std::fs::write(&compose, TYPICAL).unwrap();

    let rendered = Scaffold::from_compose(&compose, &directory.join("dcd.yaml")).unwrap().render();
    let parsed: serde_norway::Value = serde_norway::from_str(&rendered).expect("valid YAML");
    let files = parsed["compose"]["files"].as_sequence().expect("a files list");
    assert_eq!(files.len(), 1, "the name must not split: {rendered}");
    assert_eq!(files[0].as_str(), Some("in,fra#1.yml"), "{rendered}");
    let _ = std::fs::remove_dir_all(&directory);
}
