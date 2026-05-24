use std::fs;

use assert_cmd::prelude::*;
use cdbgen_rs::cdb_writer::domain_to_wire;
use predicates::prelude::*;
use std::process::Command;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

#[test]
fn invalid_config_exits_2() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("bad.toml");
    fs::write(&config, "not = [valid").unwrap();

    Command::cargo_bin("cdbgen-rs")
        .unwrap()
        .arg("--config")
        .arg(config)
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("config error"));
}

#[tokio::test]
async fn dry_run_fetches_and_does_not_write_output() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wiremock::matchers::path("/list"))
        .respond_with(ResponseTemplate::new(200).set_body_string("example.com\n"))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.cdb");
    let config = dir.path().join("config.toml");
    fs::write(
        &config,
        format!(
            r#"
            [sources]
            a = "{}/list"
            [outputs]
            a = "{}"
            "#,
            server.uri(),
            output.display()
        ),
    )
    .unwrap();

    Command::cargo_bin("cdbgen-rs")
        .unwrap()
        .arg("--config")
        .arg(config)
        .arg("--dry-run")
        .assert()
        .success();

    assert!(!output.exists());
}

#[tokio::test]
async fn normal_run_writes_wire_format_keys_by_default() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wiremock::matchers::path("/list"))
        .respond_with(ResponseTemplate::new(200).set_body_string("example.com\n"))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.cdb");
    let config = dir.path().join("config.toml");
    fs::write(
        &config,
        format!(
            r#"
            [sources]
            a = "{}/list"
            [outputs]
            a = "{}"
            "#,
            server.uri(),
            output.display()
        ),
    )
    .unwrap();

    Command::cargo_bin("cdbgen-rs")
        .unwrap()
        .arg("--config")
        .arg(config)
        .assert()
        .success();

    let db = cdb::CDB::open(&output).unwrap();
    assert_eq!(
        db.get(&domain_to_wire("example.com").unwrap())
            .unwrap()
            .unwrap(),
        b""
    );
    assert!(db.get(b"example.com").is_none());
}

#[test]
fn output_flag_is_deferred() {
    Command::cargo_bin("cdbgen-rs")
        .unwrap()
        .arg("--output")
        .arg("general")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}
