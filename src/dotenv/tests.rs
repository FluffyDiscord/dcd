//! Ported from symfony/dotenv 8.1 `Tests/DotenvTest.php` (spec TC-031): the
//! `getEnvData` / `getEnvDataWithFormatErrors` providers plus the loadEnv chain
//! semantics (deferred resolution, overridden-file references, self-referencing
//! defaults, circular detection), adapted to the map-based API. Deviations under
//! test: `$(command)` errors instead of executing; `$_SERVER`/`putenv`/
//! `.env.local.php` are N/A (process-env model). Chain layering, filters, and
//! reserved keys: TC-032/033/034.

use std::collections::HashMap;

use super::*;

fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn parse_with(data: &str, process: &HashMap<String, String>) -> Vec<(String, String)> {
    parse_document(data, ".env", process)
        .unwrap_or_else(|e| panic!("parse of {data:?} failed: {e}"))
        .into_iter()
        .collect()
}

fn parse_ok(data: &str) -> Vec<(String, String)> {
    parse_with(data, &HashMap::new())
}

fn parse_err(data: &str) -> String {
    let err = parse_document(data, ".env", &HashMap::new())
        .expect_err(&format!("parse of {data:?} should fail"));
    match err {
        DcdError::Config(message) => message,
        other => panic!("expected Config error, got {other:?}"),
    }
}

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn symfony_backslashes() {
    assert_eq!(parse_ok("FOO=foo\\\\bar"), pairs(&[("FOO", "foo\\bar")]));
    assert_eq!(parse_ok("FOO='foo\\\\bar'"), pairs(&[("FOO", "foo\\\\bar")]));
    assert_eq!(parse_ok("FOO=\"foo\\\\bar\""), pairs(&[("FOO", "foo\\bar")]));
}

#[test]
fn symfony_escaped_backslash_before_variable() {
    assert_eq!(
        parse_ok("BAR=bar\nFOO=foo\\\\$BAR"),
        pairs(&[("BAR", "bar"), ("FOO", "foo\\bar")])
    );
    assert_eq!(
        parse_ok("BAR=bar\nFOO='foo\\\\$BAR'"),
        pairs(&[("BAR", "bar"), ("FOO", "foo\\\\$BAR")])
    );
    assert_eq!(
        parse_ok("BAR=bar\nFOO=\"foo\\\\$BAR\""),
        pairs(&[("BAR", "bar"), ("FOO", "foo\\bar")])
    );
    assert_eq!(parse_ok("FOO=foo\\\\\\$BAR"), pairs(&[("FOO", "foo\\$BAR")]));
    assert_eq!(
        parse_ok("FOO='foo\\\\\\$BAR'"),
        pairs(&[("FOO", "foo\\\\\\$BAR")])
    );
    assert_eq!(
        parse_ok("FOO=\"foo\\\\\\$BAR\""),
        pairs(&[("FOO", "foo\\$BAR")])
    );
}

#[test]
fn symfony_spaces() {
    assert_eq!(parse_ok("FOO=bar"), pairs(&[("FOO", "bar")]));
    assert_eq!(parse_ok(" FOO=bar "), pairs(&[("FOO", "bar")]));
    assert_eq!(parse_ok("FOO="), pairs(&[("FOO", "")]));
    assert_eq!(
        parse_ok("FOO=\n\n\nBAR=bar"),
        pairs(&[("FOO", ""), ("BAR", "bar")])
    );
    assert_eq!(parse_ok("FOO=  "), pairs(&[("FOO", "")]));
    assert_eq!(parse_ok("FOO=\nBAR=bar"), pairs(&[("FOO", ""), ("BAR", "bar")]));
}

#[test]
fn blank_lines_of_php_whitespace_are_skipped() {
    assert_eq!(
        parse_ok("FOO=bar\n\u{0C}\nBAR=baz"),
        pairs(&[("FOO", "bar"), ("BAR", "baz")])
    );
    assert_eq!(
        parse_ok("FOO=bar\n\u{0B}\nBAR=baz"),
        pairs(&[("FOO", "bar"), ("BAR", "baz")])
    );
}

#[test]
fn symfony_newlines() {
    assert_eq!(parse_ok("\n\nFOO=bar\r\n\n"), pairs(&[("FOO", "bar")]));
    assert_eq!(
        parse_ok("FOO=bar\r\nBAR=foo"),
        pairs(&[("FOO", "bar"), ("BAR", "foo")])
    );
    assert_eq!(
        parse_ok("FOO=bar\rBAR=foo"),
        pairs(&[("FOO", "bar"), ("BAR", "foo")])
    );
    assert_eq!(
        parse_ok("FOO=bar\nBAR=foo"),
        pairs(&[("FOO", "bar"), ("BAR", "foo")])
    );
}

#[test]
fn symfony_quotes() {
    assert_eq!(parse_ok("FOO=\"bar\"\n"), pairs(&[("FOO", "bar")]));
    assert_eq!(parse_ok("FOO=\"bar'foo\"\n"), pairs(&[("FOO", "bar'foo")]));
    assert_eq!(parse_ok("FOO='bar'\n"), pairs(&[("FOO", "bar")]));
    assert_eq!(parse_ok("FOO='bar\"foo'\n"), pairs(&[("FOO", "bar\"foo")]));
    assert_eq!(parse_ok("FOO=\"bar\\\"foo\"\n"), pairs(&[("FOO", "bar\"foo")]));
    assert_eq!(parse_ok("FOO=\"bar\\nfoo\""), pairs(&[("FOO", "bar\nfoo")]));
    assert_eq!(parse_ok("FOO=\"bar\\rfoo\""), pairs(&[("FOO", "bar\rfoo")]));
    assert_eq!(parse_ok("FOO='bar\\nfoo'"), pairs(&[("FOO", "bar\\nfoo")]));
    assert_eq!(parse_ok("FOO='bar\\rfoo'"), pairs(&[("FOO", "bar\\rfoo")]));
    assert_eq!(parse_ok("FOO='bar\nfoo'"), pairs(&[("FOO", "bar\nfoo")]));
    assert_eq!(parse_ok("FOO=\" FOO \""), pairs(&[("FOO", " FOO ")]));
    assert_eq!(parse_ok("FOO=\"  \""), pairs(&[("FOO", "  ")]));
    assert_eq!(parse_ok("PATH=\"c:\\\\\""), pairs(&[("PATH", "c:\\")]));
    assert_eq!(parse_ok("FOO=\"bar\nfoo\""), pairs(&[("FOO", "bar\nfoo")]));
    assert_eq!(parse_ok("FOO=BAR\\\""), pairs(&[("FOO", "BAR\"")]));
    assert_eq!(parse_ok("FOO=BAR\\'BAZ"), pairs(&[("FOO", "BAR'BAZ")]));
    assert_eq!(parse_ok("FOO=\\\"BAR"), pairs(&[("FOO", "\"BAR")]));
}

#[test]
fn symfony_concatenated_values() {
    assert_eq!(parse_ok("FOO='bar''foo'\n"), pairs(&[("FOO", "barfoo")]));
    assert_eq!(parse_ok("FOO='bar '' baz'"), pairs(&[("FOO", "bar  baz")]));
    assert_eq!(
        parse_ok("FOO=bar\nBAR='baz'\"$FOO\""),
        pairs(&[("FOO", "bar"), ("BAR", "bazbar")])
    );
    assert_eq!(parse_ok("FOO='bar '\\'' baz'"), pairs(&[("FOO", "bar ' baz")]));
}

#[test]
fn symfony_comments() {
    assert_eq!(parse_ok("#FOO=bar\nBAR=foo"), pairs(&[("BAR", "foo")]));
    assert_eq!(parse_ok("#FOO=bar # Comment\nBAR=foo"), pairs(&[("BAR", "foo")]));
    assert_eq!(parse_ok("FOO='bar foo' # Comment"), pairs(&[("FOO", "bar foo")]));
    assert_eq!(parse_ok("FOO='bar#foo' # Comment"), pairs(&[("FOO", "bar#foo")]));
    assert_eq!(
        parse_ok("# Comment\r\nFOO=bar\n# Comment\nBAR=foo"),
        pairs(&[("FOO", "bar"), ("BAR", "foo")])
    );
    assert_eq!(
        parse_ok("FOO=bar # Another comment\nBAR=foo"),
        pairs(&[("FOO", "bar"), ("BAR", "foo")])
    );
    assert_eq!(
        parse_ok("FOO=\n\n# comment\nBAR=bar"),
        pairs(&[("FOO", ""), ("BAR", "bar")])
    );
    assert_eq!(parse_ok("FOO=NOT#COMMENT"), pairs(&[("FOO", "NOT#COMMENT")]));
    assert_eq!(parse_ok("FOO=  # Comment"), pairs(&[("FOO", "")]));
}

#[test]
fn symfony_typeless_values() {
    assert_eq!(parse_ok("FOO=0"), pairs(&[("FOO", "0")]));
    assert_eq!(parse_ok("FOO=false"), pairs(&[("FOO", "false")]));
    assert_eq!(parse_ok("FOO=null"), pairs(&[("FOO", "null")]));
}

#[test]
fn symfony_export() {
    assert_eq!(parse_ok("export FOO=bar"), pairs(&[("FOO", "bar")]));
    assert_eq!(parse_ok("  export   FOO=bar"), pairs(&[("FOO", "bar")]));
    assert_eq!(parse_ok("export=\"export as key\""), pairs(&[("export", "export as key")]));
    assert_eq!(
        parse_ok("export   SHELL_LOVER=1"),
        pairs(&[("SHELL_LOVER", "1")])
    );
}

#[test]
fn symfony_variable_expansion() {
    assert_eq!(
        parse_ok("FOO=BAR\nBAR=$FOO"),
        pairs(&[("FOO", "BAR"), ("BAR", "BAR")])
    );
    assert_eq!(
        parse_ok("FOO=BAR\nBAR=\"$FOO\""),
        pairs(&[("FOO", "BAR"), ("BAR", "BAR")])
    );
    assert_eq!(
        parse_ok("FOO=BAR\nBAR='$FOO'"),
        pairs(&[("FOO", "BAR"), ("BAR", "$FOO")])
    );
    assert_eq!(
        parse_ok("FOO_BAR9=BAR\nBAR=$FOO_BAR9"),
        pairs(&[("FOO_BAR9", "BAR"), ("BAR", "BAR")])
    );
    assert_eq!(
        parse_ok("FOO=BAR\nBAR=${FOO}Z"),
        pairs(&[("FOO", "BAR"), ("BAR", "BARZ")])
    );
    assert_eq!(
        parse_ok("FOO=BAR\nBAR=$FOO}"),
        pairs(&[("FOO", "BAR"), ("BAR", "BAR}")])
    );
    assert_eq!(
        parse_ok("FOO=BAR\nBAR=\\$FOO"),
        pairs(&[("FOO", "BAR"), ("BAR", "$FOO")])
    );
    assert_eq!(parse_ok("FOO=\" \\$ \""), pairs(&[("FOO", " $ ")]));
    assert_eq!(parse_ok("FOO=\" $ \""), pairs(&[("FOO", " $ ")]));
    assert_eq!(parse_ok("FOO=$NOTDEFINED"), pairs(&[("FOO", "")]));
    assert_eq!(
        parse_ok("FOO=foo\nFOOBAR=${FOO}${BAR}"),
        pairs(&[("FOO", "foo"), ("FOOBAR", "foo")])
    );
}

#[test]
fn symfony_expansion_from_process_env() {
    let process = env(&[("LOCAL", "local"), ("REMOTE", "remote"), ("SERVERVAR", "servervar")]);
    assert_eq!(parse_with("BAR=$LOCAL", &process), pairs(&[("BAR", "local")]));
    assert_eq!(parse_with("BAR=$REMOTE", &process), pairs(&[("BAR", "remote")]));
    assert_eq!(
        parse_with("BAR=$SERVERVAR", &process),
        pairs(&[("BAR", "servervar")])
    );
}

#[test]
fn symfony_default_values() {
    assert_eq!(
        parse_ok("FOO=BAR\nBAR=${FOO:-TEST}"),
        pairs(&[("FOO", "BAR"), ("BAR", "BAR")])
    );
    assert_eq!(
        parse_ok("FOO=BAR\nBAR=${NOTDEFINED:-TEST}"),
        pairs(&[("FOO", "BAR"), ("BAR", "TEST")])
    );
    assert_eq!(
        parse_ok("FOO=\nBAR=${FOO:-TEST}"),
        pairs(&[("FOO", ""), ("BAR", "TEST")])
    );
    assert_eq!(
        parse_ok("FOO=\nBAR=$FOO:-TEST}"),
        pairs(&[("FOO", ""), ("BAR", "TEST}")])
    );
    assert_eq!(
        parse_ok("FOO=BAR\nBAR=${FOO:=TEST}"),
        pairs(&[("FOO", "BAR"), ("BAR", "BAR")])
    );
    assert_eq!(
        parse_ok("FOO=BAR\nBAR=${NOTDEFINED:=TEST}"),
        pairs(&[("FOO", "BAR"), ("NOTDEFINED", "TEST"), ("BAR", "TEST")])
    );
    assert_eq!(
        parse_ok("FOO=\nBAR=${FOO:=TEST}"),
        pairs(&[("FOO", "TEST"), ("BAR", "TEST")])
    );
    assert_eq!(
        parse_ok("FOO=\nBAR=$FOO:=TEST}"),
        pairs(&[("FOO", "TEST"), ("BAR", "TEST}")])
    );
    assert_eq!(
        parse_ok("FOO=BAR\nBAR=${FOO:-}"),
        pairs(&[("FOO", "BAR"), ("BAR", "BAR")])
    );
    assert_eq!(
        parse_ok("FOO=BAR\nBAR=${NOTDEFINED:-}"),
        pairs(&[("FOO", "BAR"), ("BAR", "")])
    );
    assert_eq!(parse_ok("FOO=\nBAR=${FOO:-}"), pairs(&[("FOO", ""), ("BAR", "")]));
    assert_eq!(parse_ok("FOO=\nBAR=$FOO:-}"), pairs(&[("FOO", ""), ("BAR", "}")]));
    assert_eq!(
        parse_ok("FOO=BAR\nBAR=${FOO:=}"),
        pairs(&[("FOO", "BAR"), ("BAR", "BAR")])
    );
    assert_eq!(
        parse_ok("FOO=BAR\nBAR=${NOTDEFINED:=}"),
        pairs(&[("FOO", "BAR"), ("NOTDEFINED", ""), ("BAR", "")])
    );
    assert_eq!(parse_ok("FOO=\nBAR=${FOO:=}"), pairs(&[("FOO", ""), ("BAR", "")]));
    assert_eq!(parse_ok("FOO=\nBAR=$FOO:=}"), pairs(&[("FOO", ""), ("BAR", "}")]));
}

#[test]
fn symfony_variable_without_parenthesis_substituted_before_separators() {
    assert_eq!(
        parse_ok("KEY1=test_user\nKEY1_1=test_user_with_separator\nKEY=\">$KEY1_1<>$KEY1}<>$KEY1{<\""),
        pairs(&[
            ("KEY1", "test_user"),
            ("KEY1_1", "test_user_with_separator"),
            ("KEY", ">test_user_with_separator<>test_user}<>test_user{<"),
        ])
    );
}

#[test]
fn symfony_underscores() {
    assert_eq!(parse_ok("_FOO=BAR"), pairs(&[("_FOO", "BAR")]));
    assert_eq!(parse_ok("_FOO_BAR=FOOBAR"), pairs(&[("_FOO_BAR", "FOOBAR")]));
    assert_eq!(parse_ok("__FOO_BAR=FOOBAR"), pairs(&[("__FOO_BAR", "FOOBAR")]));
}

#[test]
fn symfony_env_value_wins_over_file_definition_in_expansion() {
    let process = env(&[("APP_ENV", "prod")]);
    assert_eq!(
        parse_with("APP_ENV=dev\nTEST1=foo1_${APP_ENV}", &process),
        pairs(&[("APP_ENV", "dev"), ("TEST1", "foo1_prod")])
    );
    let process = env(&[("Foo", "Bar")]);
    assert_eq!(parse_with("Foo=${Foo}", &process), pairs(&[("Foo", "Bar")]));
}

#[test]
fn symfony_format_errors() {
    let cases: Vec<(&str, String)> = vec![
        ("FOO=BAR BAZ", "A value containing spaces must be surrounded by quotes in \".env\" at line 1.\n...FOO=BAR BAZ...\n             ^ line 1 offset 11".to_string()),
        ("FOO BAR=BAR", "Whitespace characters are not supported after the variable name in \".env\" at line 1.\n...FOO BAR=BAR...\n     ^ line 1 offset 3".to_string()),
        ("FOO", "Missing = in the environment variable declaration in \".env\" at line 1.\n...FOO...\n     ^ line 1 offset 3".to_string()),
        ("FOO=\"foo", "Missing quote to end the value in \".env\" at line 1.\n...FOO=\"foo...\n          ^ line 1 offset 8".to_string()),
        ("FOO='foo", "Missing quote to end the value in \".env\" at line 1.\n...FOO='foo...\n          ^ line 1 offset 8".to_string()),
        ("FOO=\"foo\nBAR=\"bar\"", "Missing quote to end the value in \".env\" at line 1.\n...FOO=\"foo\\nBAR=\"bar\"...\n                     ^ line 1 offset 18".to_string()),
        ("FOO='foo\n", "Missing quote to end the value in \".env\" at line 1.\n...FOO='foo\\n...\n            ^ line 1 offset 9".to_string()),
        ("export FOO", "Unable to unset an environment variable in \".env\" at line 1.\n...export FOO...\n            ^ line 1 offset 10".to_string()),
        ("FOO=${FOO", "Unclosed braces on variable expansion in \".env\" at line 1.\n...FOO=${FOO...\n           ^ line 1 offset 9".to_string()),
        ("FOO= BAR", "Whitespace are not supported before the value in \".env\" at line 1.\n...FOO= BAR...\n      ^ line 1 offset 4".to_string()),
        ("Стасян", "Invalid character in variable name in \".env\" at line 1.\n...Стасян...\n  ^ line 1 offset 0".to_string()),
        ("FOO!", "Missing = in the environment variable declaration in \".env\" at line 1.\n...FOO!...\n     ^ line 1 offset 3".to_string()),
        ("FOO=$(echo foo", "Missing closing parenthesis. in \".env\" at line 1.\n...FOO=$(echo foo...\n                ^ line 1 offset 14".to_string()),
        ("FOO=$(echo foo\n", "Missing closing parenthesis. in \".env\" at line 1.\n...FOO=$(echo foo\\n...\n                ^ line 1 offset 14".to_string()),
        ("FOO=\nBAR=${FOO:-\\'a{a}a}", "Unsupported character \"'\" found in the default value of variable \"$FOO\". in \".env\" at line 2.\n...\\nBAR=${FOO:-\\'a{a}a}...\n                       ^ line 2 offset 24".to_string()),
        ("FOO=\nBAR=${FOO:-a$a}", "Unsupported character \"$\" found in the default value of variable \"$FOO\". in \".env\" at line 2.\n...FOO=\\nBAR=${FOO:-a$a}...\n                       ^ line 2 offset 20".to_string()),
        ("FOO=\nBAR=${FOO:-a\"a}", "Missing quote to end the value in \".env\" at line 2.\n...FOO=\\nBAR=${FOO:-a\"a}...\n                       ^ line 2 offset 20".to_string()),
        ("_=FOO", "Invalid character in variable name in \".env\" at line 1.\n..._=FOO...\n  ^ line 1 offset 0".to_string()),
    ];
    for (data, expected) in cases {
        assert_eq!(parse_err(data), expected, "input: {data:?}");
    }
}

#[test]
fn command_expansion_is_refused_not_executed() {
    let message = parse_err("FOO=$(echo foo)");
    assert!(
        message.starts_with("command expansion is not supported; remove $(...)"),
        "got: {message}"
    );
    let message = parse_err("FOO=FOO$((1+2))BAR");
    assert!(
        message.starts_with("command expansion is not supported; remove $(...)"),
        "got: {message}"
    );
    assert_eq!(
        parse_ok("FOO=\\$(echo foo)"),
        pairs(&[("FOO", "$(echo foo)")])
    );
}

#[test]
fn command_expansion_across_a_quoted_newline_is_refused_too() {
    let message = parse_err("FOO=\"$(echo\nhi)\"");
    assert!(
        message.starts_with("command expansion is not supported; remove $(...)"),
        "got: {message}"
    );

    let layers = docs(&[(".env", "FOO=\"$(echo\nhi)\"")]);
    let err = resolve_documents(&layers, &HashMap::new(), Vec::new()).unwrap_err();
    assert!(
        err.to_string().contains("command expansion is not supported"),
        "got: {err}"
    );
}

#[test]
fn truncated_command_expression_errors_instead_of_looping() {
    for data in ["FOO=$(", "FOO=$(a(", "FOO=$(a)b$(", "FOO=x\nBAR=$("] {
        let message = parse_err(data);
        assert!(
            message.starts_with("Missing closing parenthesis."),
            "input {data:?} got: {message}"
        );
    }
}

#[test]
fn deep_paren_nesting_errors_instead_of_overflowing_the_stack() {
    let depth = 100_000;
    let data = format!("FOO=$({}{}", "(".repeat(depth), ")".repeat(depth + 1));
    let message = parse_err(&data);
    assert!(message.starts_with("Missing closing parenthesis."), "got: {message}");
}

#[test]
fn multibyte_content_inside_escaped_command_survives() {
    assert_eq!(
        parse_ok("FOO=\\$(echo žlutý)"),
        pairs(&[("FOO", "$(echo žlutý)")])
    );
}

#[test]
fn invalid_env_key_charset() {
    for bad in ["A B", "A=B", "A\nB", "1AB", "", "A-B"] {
        assert!(invalid_env_key(bad), "{bad:?} must be invalid");
    }
    for good in ["A", "_A", "__A", "TZ", "DATABASE_URL", "a1_b"] {
        assert!(!invalid_env_key(good), "{good:?} must be valid");
    }
}

#[test]
fn bom_is_refused() {
    let message = parse_err("\u{FEFF}FOO=bar");
    assert!(
        message.starts_with("Loading files starting with a byte-order-mark (BOM) is not supported."),
        "got: {message}"
    );
}

#[test]
fn nul_bytes_are_refused() {
    let message = parse_err("FOO=b\0ar");
    assert!(
        message.starts_with("Loading files containing NUL bytes is not supported."),
        "got: {message}"
    );

    let layers = docs(&[(".env", "FOO=b\0ar")]);
    let err = resolve_documents(&layers, &HashMap::new(), Vec::new()).unwrap_err();
    assert!(
        err.to_string().contains("Loading files containing NUL bytes is not supported."),
        "got: {err}"
    );
}

// ---------- chain layering (spec §5.2.1/§5.2.2, TC-032) ----------

fn docs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter().map(|(l, d)| (l.to_string(), d.to_string())).collect()
}

#[test]
fn chain_later_layer_wins_and_cross_layer_references_resolve() {
    let layers = docs(&[
        (".env", "DB_PASSWORD=secret\nAPP_ENV=dev"),
        (".env.prod", "APP_ENV=prod\nDATABASE_URL=postgres://app:${DB_PASSWORD}@db/app"),
    ]);
    let resolved = resolve_documents(&layers, &HashMap::new(), Vec::new()).unwrap();
    assert_eq!(resolved.container_env["APP_ENV"], "prod");
    assert_eq!(
        resolved.container_env["DATABASE_URL"],
        "postgres://app:secret@db/app"
    );
}

#[test]
fn chain_process_env_wins_over_every_layer() {
    let layers = docs(&[(".env", "APP_SECRET=file"), (".env.prod.local", "APP_SECRET=local")]);
    let process = env(&[("APP_SECRET", "real"), ("CI_ONLY", "yes")]);
    let resolved = resolve_documents(&layers, &process, Vec::new()).unwrap();
    assert_eq!(resolved.container_env["APP_SECRET"], "real");
    assert_eq!(resolved.shadowed_keys, vec!["APP_SECRET".to_string()]);
    assert!(
        !resolved.container_env.contains_key("CI_ONLY"),
        "process-only keys never reach containers"
    );
    assert_eq!(resolved.interpolation_env["CI_ONLY"], "yes");
    assert_eq!(resolved.interpolation_env["APP_SECRET"], "real");
}

#[test]
fn chain_empty_value_is_a_manifest_key_process_env_supplies_it() {
    let layers = docs(&[(".env", "DATABASE_URL=")]);
    let process = env(&[("DATABASE_URL", "postgres://real")]);
    let resolved = resolve_documents(&layers, &process, Vec::new()).unwrap();
    assert_eq!(resolved.container_env["DATABASE_URL"], "postgres://real");

    let resolved = resolve_documents(&layers, &HashMap::new(), Vec::new()).unwrap();
    assert_eq!(resolved.container_env["DATABASE_URL"], "");
}

#[test]
fn chain_file_discovery_layers_and_local_stage_skip() {
    let dir = std::env::temp_dir().join(format!("dcd-dotenv-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(".env"), "FOO=base\nONLY_BASE=1").unwrap();
    std::fs::write(dir.join(".env.local"), "FOO=local").unwrap();
    std::fs::write(dir.join(".env.prod"), "FOO=prod").unwrap();
    std::fs::write(dir.join(".env.prod.local"), "FOO=prodlocal").unwrap();

    let base = dir.join(".env");
    let resolved = resolve(&base, "prod", false, None, &HashMap::new()).unwrap();
    assert_eq!(resolved.container_env["FOO"], "prodlocal");
    assert_eq!(resolved.container_env["ONLY_BASE"], "1");
    assert_eq!(resolved.layers.len(), 4);

    let resolved = resolve(&base, "local", false, None, &HashMap::new()).unwrap();
    assert_eq!(resolved.container_env["FOO"], "local");

    let resolved = resolve(&base, "", false, None, &HashMap::new()).unwrap();
    assert_eq!(resolved.container_env["FOO"], "local");

    let stdin_doc = "FOO=stdin";
    let resolved = resolve(&base, "prod", false, Some(stdin_doc), &HashMap::new()).unwrap();
    assert_eq!(resolved.container_env["FOO"], "stdin");
    assert_eq!(resolved.layers.last().unwrap().label, "<stdin>");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn chain_dist_fallback_and_missing_dir() {
    let dir = std::env::temp_dir().join(format!("dcd-dotenv-dist-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(".env.dist"), "FOO=dist").unwrap();
    let base = dir.join(".env");
    let resolved = resolve(&base, "", false, None, &HashMap::new()).unwrap();
    assert_eq!(resolved.container_env["FOO"], "dist");

    std::fs::write(dir.join(".env"), "FOO=real").unwrap();
    let resolved = resolve(&base, "", false, None, &HashMap::new()).unwrap();
    assert_eq!(resolved.container_env["FOO"], "real");
    std::fs::remove_dir_all(&dir).unwrap();

    let missing = dir.join("nope").join(".env");
    let err = resolve(&missing, "", false, None, &HashMap::new()).unwrap_err();
    assert!(err.to_string().contains("does not exist"), "got: {err}");
}

#[test]
fn chain_unreadable_file_is_a_loud_error() {
    let dir = std::env::temp_dir().join(format!("dcd-dotenv-bad-{}", std::process::id()));
    std::fs::create_dir_all(dir.join(".env.prod")).unwrap();
    std::fs::write(dir.join(".env"), "FOO=1").unwrap();
    let base = dir.join(".env");
    let err = resolve(&base, "prod", false, None, &HashMap::new()).unwrap_err();
    assert!(err.to_string().contains("is a directory"), "got: {err}");

    std::fs::remove_dir_all(dir.join(".env.prod")).unwrap();
    std::fs::write(dir.join(".env.prod"), [0xFF, 0xFE, 0x00]).unwrap();
    let err = resolve(&base, "prod", false, None, &HashMap::new()).unwrap_err();
    assert!(err.to_string().contains("not valid UTF-8"), "got: {err}");
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------- deferred resolution (symfony/dotenv 8.1 loadEnv semantics) ----------

#[test]
fn chain_forward_reference_resolves_across_layers() {
    let layers = docs(&[
        (".env", "HOST=localhost\nURL=http://${HOST}"),
        (".env.local", "HOST=production"),
    ]);
    let resolved = resolve_documents(&layers, &HashMap::new(), Vec::new()).unwrap();
    assert_eq!(resolved.container_env["HOST"], "production");
    assert_eq!(resolved.container_env["URL"], "http://production");
}

#[test]
fn chain_later_override_rewrites_earlier_reference() {
    let layers = docs(&[
        (".env", "REDIS_HOST=localhost\nLOCK_DSN=redis://${REDIS_HOST}"),
        (".env.local", "REDIS_HOST=aaa"),
    ]);
    let resolved = resolve_documents(&layers, &HashMap::new(), Vec::new()).unwrap();
    assert_eq!(resolved.container_env["REDIS_HOST"], "aaa");
    assert_eq!(resolved.container_env["LOCK_DSN"], "redis://aaa");
}

#[test]
fn chain_double_quoted_backslashes_with_variables_resolve_once() {
    let layers = docs(&[
        (".env", "HOST=localhost\nDSN=\"path\\\\${HOST}\""),
        (".env.local", "HOST=override"),
    ]);
    let resolved = resolve_documents(&layers, &HashMap::new(), Vec::new()).unwrap();
    assert_eq!(resolved.container_env["DSN"], "path\\override");

    let layers = docs(&[
        (".env", "HOST=localhost\nDSN=\"path\\\\\\\\:${HOST}\""),
        (".env.local", "HOST=override"),
    ]);
    let resolved = resolve_documents(&layers, &HashMap::new(), Vec::new()).unwrap();
    assert_eq!(resolved.container_env["DSN"], "path\\\\:override");
}

#[test]
fn chain_single_quoted_dollar_stays_literal() {
    let layers = docs(&[(".env", "BAR=hello\nFOO='$BAR'"), (".env.local", "BAR=world")]);
    let resolved = resolve_documents(&layers, &HashMap::new(), Vec::new()).unwrap();
    assert_eq!(resolved.container_env["FOO"], "$BAR");
    assert_eq!(resolved.container_env["BAR"], "world");
}

#[test]
fn chain_escaped_dollars_survive_deferred_resolution() {
    let bcrypt = "$2y$10$AAAAAAAAAAAAAAAAAAAAAAAAAA.BBBBBBBBBBBBBBBBBBBBBB";
    let layers = docs(&[
        (".env", "FOO=\"\\$2y\\$10\\$AAAAAAAAAAAAAAAAAAAAAAAAAA.BBBBBBBBBBBBBBBBBBBBBB\""),
        (".env.local", "BAR=dummy"),
    ]);
    let resolved = resolve_documents(&layers, &HashMap::new(), Vec::new()).unwrap();
    assert_eq!(resolved.container_env["FOO"], bcrypt);

    let layers = docs(&[
        (".env", "FOO=\\$2y\\$10\\$AAAAAAAAAAAAAAAAAAAAAAAAAA.BBBBBBBBBBBBBBBBBBBBBB"),
        (".env.local", "BAR=dummy"),
    ]);
    let resolved = resolve_documents(&layers, &HashMap::new(), Vec::new()).unwrap();
    assert_eq!(resolved.container_env["FOO"], bcrypt);
}

#[test]
fn chain_double_backslash_without_dollar_unescapes_once() {
    let layers = docs(&[
        (".env", "DATABASE_URL=sqlsrv://user:pass@localhost\\\\SQLEXPRESS/db"),
        (".env.local", "BAR=dummy"),
    ]);
    let resolved = resolve_documents(&layers, &HashMap::new(), Vec::new()).unwrap();
    assert_eq!(
        resolved.container_env["DATABASE_URL"],
        "sqlsrv://user:pass@localhost\\SQLEXPRESS/db"
    );
}

#[test]
fn chain_circular_references_error_instead_of_looping() {
    let layers = docs(&[(".env", "A=${B}x"), (".env.local", "B=${A}y")]);
    let err = resolve_documents(&layers, &HashMap::new(), Vec::new()).unwrap_err();
    assert_eq!(
        err.to_string(),
        "config: Too many levels of variable indirection in env vars: A, B."
    );
}

#[test]
fn chain_external_values_are_preserved_verbatim_never_executed() {
    let externals = [
        "secret$word",
        "value$(id)",
        "a\\b",
        "a\\\\b",
        "a\\\\\\b",
        "a\\\\\\\\b",
        "a\\\\",
        "\\\\a",
        "a\\\\$b",
    ];
    for external in externals {
        let process = env(&[("EXT_VAR", external)]);
        let layers = docs(&[(".env", "REF_VAR=pre${EXT_VAR}post")]);
        let resolved = resolve_documents(&layers, &process, Vec::new()).unwrap();
        assert_eq!(
            resolved.container_env["REF_VAR"],
            format!("pre{external}post"),
            "external {external:?} must round-trip"
        );
    }
}

#[test]
fn chain_self_referencing_default_applies_or_yields_to_process_env() {
    let layers = docs(&[(".env", "EXT_VAR=${EXT_VAR:-default}")]);
    let resolved = resolve_documents(&layers, &HashMap::new(), Vec::new()).unwrap();
    assert_eq!(resolved.container_env["EXT_VAR"], "default");

    let process = env(&[("EXT_VAR", "external")]);
    let resolved = resolve_documents(&layers, &process, Vec::new()).unwrap();
    assert_eq!(resolved.container_env["EXT_VAR"], "external");

    let layers = docs(&[(".env", "MY_VAR=\"${MY_VAR:=fallback}\"")]);
    let resolved = resolve_documents(&layers, &HashMap::new(), Vec::new()).unwrap();
    assert_eq!(resolved.container_env["MY_VAR"], "fallback");
}

#[test]
fn chain_self_reference_appends_to_the_previous_layer_value() {
    let layers = docs(&[
        (".env", "MY_VAR=original"),
        (".env.local", "MY_VAR=\"${MY_VAR}_suffix\""),
    ]);
    let resolved = resolve_documents(&layers, &HashMap::new(), Vec::new()).unwrap();
    assert_eq!(resolved.container_env["MY_VAR"], "original_suffix");
}

#[test]
fn chain_unquoted_space_with_variable_does_not_error() {
    let layers = docs(&[
        (".env", "PREFIX=hello\nLABEL=${PREFIX} world"),
        (".env.local", "PREFIX=overridden"),
    ]);
    let resolved = resolve_documents(&layers, &HashMap::new(), Vec::new()).unwrap();
    assert_eq!(resolved.container_env["LABEL"], "overridden world");
}

#[test]
fn chain_command_expansion_errors_at_resolution_with_the_key_named() {
    let layers = docs(&[(".env", "RESOLVED=$(echo hi)")]);
    let err = resolve_documents(&layers, &HashMap::new(), Vec::new()).unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("command expansion is not supported"),
        "got: {message}"
    );
    assert!(message.contains("RESOLVED"), "got: {message}");
}

#[test]
fn chain_base_override_rebases_the_whole_chain() {
    let dir = std::env::temp_dir().join(format!("dcd-dotenv-base-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(".env"), "FOO=app-owned").unwrap();
    std::fs::write(dir.join(".env.deploy"), "FOO=base\nONLY_BASE=1").unwrap();
    std::fs::write(dir.join(".env.deploy.local"), "FOO=local").unwrap();
    std::fs::write(dir.join(".env.deploy.prod"), "FOO=prod").unwrap();

    let base = dir.join(".env.deploy");
    let resolved = resolve(&base, "prod", true, None, &HashMap::new()).unwrap();
    assert_eq!(resolved.container_env["FOO"], "prod");
    assert_eq!(resolved.container_env["ONLY_BASE"], "1");
    assert!(
        resolved.layers.iter().all(|l| l.label != dir.join(".env").display().to_string()),
        "the app's own .env must not be read"
    );

    let missing = dir.join(".env.missing");
    let err = resolve(&missing, "prod", true, None, &HashMap::new()).unwrap_err();
    assert!(err.to_string().contains("does not exist"), "got: {err}");

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------- reserved keys (spec §5.2.3, TC-034) ----------

#[test]
fn reserved_keys_in_a_chain_layer_error_with_the_file() {
    for key in ["DOCKER_HOST", "COMPOSE_FILE", "PATH", "HOME", "LD_PRELOAD", "http_proxy"] {
        let layers = docs(&[(".env.prod", &format!("{key}=x"))]);
        let err = resolve_documents(&layers, &HashMap::new(), Vec::new()).unwrap_err();
        let message = err.to_string();
        assert!(message.contains(key), "missing key in: {message}");
        assert!(message.contains(".env.prod"), "missing file in: {message}");
        assert!(message.contains("release.run.env"), "missing escape hatch in: {message}");
    }
}

#[test]
fn reserved_allowances_for_explicit_maps() {
    assert!(reserved_key_reason("COMPOSE_PROJECT_NAME", ReservedAllowance::ComposeVars).is_none());
    assert!(reserved_key_reason("COMPOSE_PROJECT_NAME", ReservedAllowance::None).is_some());
    assert!(reserved_key_reason("HTTP_PROXY", ReservedAllowance::ProxyVars).is_none());
    assert!(reserved_key_reason("HTTP_PROXY", ReservedAllowance::ComposeVars).is_some());
    assert!(reserved_key_reason("DOCKER_HOST", ReservedAllowance::ProxyVars).is_some());
    assert!(reserved_key_reason("TZ", ReservedAllowance::None).is_none());
}

// ---------- filters (spec §5.2.4, TC-033) ----------

fn container(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[test]
fn filters_full_match_exclude_wins_sorted() {
    let envs = container(&[("MAILER_DSN", "a"), ("MAILER", "b"), ("DATABASE_URL", "c")]);
    let all = filter_container_keys(&envs, &[], &[]).unwrap();
    assert_eq!(all, vec!["DATABASE_URL", "MAILER", "MAILER_DSN"]);

    let unanchored = filter_container_keys(&envs, &["MAILER".into()], &[]).unwrap();
    assert_eq!(unanchored, vec!["MAILER"], "no substring match");

    let prefixed = filter_container_keys(&envs, &["MAILER.*".into()], &[]).unwrap();
    assert_eq!(prefixed, vec!["MAILER", "MAILER_DSN"]);

    let exclude_wins =
        filter_container_keys(&envs, &["MAILER.*".into()], &["MAILER_DSN".into()]).unwrap();
    assert_eq!(exclude_wins, vec!["MAILER"]);

    let err = filter_container_keys(&envs, &["(".into()], &[]).unwrap_err();
    assert!(err.to_string().contains("env_include"), "got: {err}");
}

#[test]
fn delivered_keys_unions_chain_and_explicit_map_sorted_dedup() {
    let envs = container(&[("B_CHAIN", "1"), ("SHARED", "2")]);
    let explicit = ["A_EXPLICIT".to_string(), "SHARED".to_string()];
    let keys = delivered_keys(&envs, &[], &[], explicit.iter()).unwrap();
    assert_eq!(keys, vec!["A_EXPLICIT", "B_CHAIN", "SHARED"]);
}
