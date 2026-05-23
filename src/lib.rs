pub mod cache;
pub mod cdb_writer;
pub mod cli;
pub mod config;
pub mod error;
pub mod fetcher;
pub mod logging;
pub mod normalize;
pub mod outputs;
pub mod parser;

use std::collections::{BTreeMap, BTreeSet};

use cli::Cli;
use config::Config;
use error::AppError;
use fetcher::{FetchOutcome, fetch_sources};
use outputs::{OutputPlan, build_output_domains, write_outputs};
use parser::{ParsedSource, parse_sources};

pub const EXIT_SUCCESS: i32 = 0;
pub const EXIT_GENERIC: i32 = 1;
pub const EXIT_CONFIG: i32 = 2;
pub const EXIT_FETCH: i32 = 3;
pub const EXIT_OUTPUT: i32 = 4;

pub async fn run(cli: Cli) -> Result<i32, AppError> {
    run_with_cache(cli, cache::Cache::default_root()).await
}

pub async fn run_with_cache(cli: Cli, cache: cache::Cache) -> Result<i32, AppError> {
    let config = Config::load(&cli.config)?;
    tracing::info!(
        config = %cli.config.display(),
        sources = config.sources.len(),
        outputs = config.outputs.len(),
        dry_run = cli.dry_run,
        force_refresh = cli.force_refresh,
        "configuration loaded"
    );

    let selected_sources = config.selected_source_ids();
    let fetches = fetch_sources(
        &config,
        &selected_sources,
        &cache,
        cli.force_refresh,
        cli.dry_run,
    )
    .await;

    let mut available = BTreeMap::new();
    let mut unavailable = BTreeSet::new();
    for (source_id, outcome) in fetches {
        match outcome {
            FetchOutcome::Available(data) => {
                available.insert(source_id, data);
            }
            FetchOutcome::Unavailable { error } => {
                tracing::error!(source = %source_id, error = %error, "source unavailable");
                unavailable.insert(source_id);
            }
        }
    }

    let parsed = parse_sources(available).await?;
    log_parse_summary(&parsed);

    let plans = build_output_domains(&config.outputs, &parsed, &unavailable);

    let skipped = plans
        .iter()
        .filter(|plan| matches!(plan, OutputPlan::Skipped { .. }))
        .count();
    let empty = plans
        .iter()
        .filter(|plan| matches!(plan, OutputPlan::Empty { .. }))
        .count();
    let ready = plans
        .iter()
        .filter(|plan| matches!(plan, OutputPlan::Ready { .. }))
        .count();

    tracing::info!(
        ready_outputs = ready,
        skipped_outputs = skipped,
        empty_outputs = empty,
        "output plan built"
    );

    if cli.dry_run {
        for plan in &plans {
            match plan {
                OutputPlan::Ready {
                    output, domains, ..
                } => tracing::info!(
                    output = %output.path.display(),
                    domains = domains.len(),
                    "dry-run would write output"
                ),
                OutputPlan::Skipped {
                    output,
                    unavailable_sources,
                } => tracing::warn!(
                    output = %output.path.display(),
                    unavailable = ?unavailable_sources,
                    "dry-run would skip output"
                ),
                OutputPlan::Empty { output } => tracing::error!(
                    output = %output.path.display(),
                    "dry-run output would be empty"
                ),
            }
        }
        return Ok(final_exit_code(skipped > 0, empty > 0));
    }

    let write_summary = write_outputs(&plans)?;
    Ok(final_exit_code(
        skipped > 0,
        empty > 0 || write_summary.empty_outputs > 0,
    ))
}

fn log_parse_summary(parsed: &BTreeMap<String, ParsedSource>) {
    for (source, parsed) in parsed {
        tracing::info!(
            source = %source,
            lines = parsed.stats.total_lines,
            blocks = parsed.blocks.len(),
            allows = parsed.allows.len(),
            rejected = parsed.stats.rejected_lines,
            "source parsed"
        );
    }
}

fn final_exit_code(had_fetch_skips: bool, had_empty_outputs: bool) -> i32 {
    if had_empty_outputs {
        EXIT_GENERIC
    } else if had_fetch_skips {
        EXIT_FETCH
    } else {
        EXIT_SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::Path};
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    #[test]
    fn final_exit_code_prefers_empty_guard_over_fetch_skip() {
        assert_eq!(final_exit_code(false, false), EXIT_SUCCESS);
        assert_eq!(final_exit_code(true, false), EXIT_FETCH);
        assert_eq!(final_exit_code(false, true), EXIT_GENERIC);
        assert_eq!(final_exit_code(true, true), EXIT_GENERIC);
    }

    #[tokio::test]
    async fn partial_fetch_failure_writes_unaffected_outputs_and_exits_3() {
        let server = MockServer::start().await;
        for index in 0..7 {
            let body = format!("s{index}.example\n");
            Mock::given(method("GET"))
                .and(wiremock::matchers::path(format!("/s{index}")))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(&server)
                .await;
        }

        let dir = tempfile::tempdir().unwrap();
        let good_a = dir.path().join("good-a.cdb");
        let good_b = dir.path().join("good-b.cdb");
        let bad_a = dir.path().join("bad-a.cdb");
        let bad_b = dir.path().join("bad-b.cdb");
        let config_path = dir.path().join("config.toml");
        fs::write(
            &config_path,
            config_for_partial_failure(&server, &good_a, &good_b, &bad_a, &bad_b),
        )
        .unwrap();

        let code = run_with_cache(
            Cli {
                config: config_path,
                dry_run: false,
                verbose: false,
                force_refresh: false,
            },
            cache::Cache::new(dir.path().join("cache")),
        )
        .await
        .unwrap();

        assert_eq!(code, EXIT_FETCH);
        assert!(good_a.exists());
        assert!(good_b.exists());
        assert!(!bad_a.exists());
        assert!(!bad_b.exists());

        let db = cdb::CDB::open(&good_a).unwrap();
        assert_eq!(db.get(b"s0.example").unwrap().unwrap(), b"");
        assert_eq!(db.get(b"s1.example").unwrap().unwrap(), b"");
    }

    fn config_for_partial_failure(
        server: &MockServer,
        good_a: &Path,
        good_b: &Path,
        bad_a: &Path,
        bad_b: &Path,
    ) -> String {
        let mut config = String::from("[sources]\n");
        for index in 0..10 {
            config.push_str(&format!("s{index} = \"{}/s{index}\"\n", server.uri()));
        }
        config.push_str("\n[outputs]\n");
        config.push_str(&format!("\"s0, s1\" = \"{}\"\n", good_a.display()));
        config.push_str(&format!(
            "\"s2, s3, s4, s5, s6\" = \"{}\"\n",
            good_b.display()
        ));
        config.push_str(&format!("\"s7, s8\" = \"{}\"\n", bad_a.display()));
        config.push_str(&format!("\"s9\" = \"{}\"\n", bad_b.display()));
        config
    }
}
