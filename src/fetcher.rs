use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use reqwest::{
    Client, StatusCode,
    header::{ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED},
};
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

use crate::{
    cache::{Cache, CacheMetadata, CachedSource},
    config::{Config, SourceConfig},
    error::FetchError,
};

const MAX_CONCURRENT_FETCHES: usize = 8;
const MAX_ATTEMPTS: usize = 4;
const USER_AGENT: &str = concat!("cdbgen-rs/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone)]
pub struct FetchedSource {
    pub body: Vec<u8>,
    pub from_cache: bool,
    pub stale: bool,
}

#[derive(Debug, Clone)]
pub enum FetchOutcome {
    Available(FetchedSource),
    Unavailable { error: FetchError },
}

pub async fn fetch_sources(
    config: &Config,
    selected_sources: &BTreeSet<String>,
    cache: &Cache,
    force_refresh: bool,
    dry_run: bool,
) -> BTreeMap<String, FetchOutcome> {
    let client = build_client();
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_FETCHES));
    let mut tasks = Vec::new();

    for source_id in selected_sources {
        let source = config.sources[source_id].clone();
        let cache = cache.clone();
        let client = client.clone();
        let semaphore = Arc::clone(&semaphore);

        tasks.push(tokio::spawn(async move {
            let permit = semaphore.acquire_owned().await;
            let outcome = match permit {
                Ok(_permit) => fetch_one(&client, &source, &cache, force_refresh, dry_run).await,
                Err(err) => Err(FetchError::Unavailable {
                    source_id: source.id.clone(),
                    message: err.to_string(),
                }),
            };

            let source_id = source.id.clone();
            let outcome = match outcome {
                Ok(source) => FetchOutcome::Available(source),
                Err(error) => FetchOutcome::Unavailable { error },
            };
            (source_id, outcome)
        }));
    }

    let mut outcomes = BTreeMap::new();
    for task in tasks {
        match task.await {
            Ok((source_id, outcome)) => {
                outcomes.insert(source_id, outcome);
            }
            Err(err) => {
                let source_id = format!("task-{}", outcomes.len());
                outcomes.insert(
                    source_id.clone(),
                    FetchOutcome::Unavailable {
                        error: FetchError::Unavailable {
                            source_id,
                            message: err.to_string(),
                        },
                    },
                );
            }
        }
    }

    outcomes
}

fn build_client() -> Client {
    Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .expect("reqwest client configuration is valid")
}

async fn fetch_one(
    client: &Client,
    source: &SourceConfig,
    cache: &Cache,
    force_refresh: bool,
    dry_run: bool,
) -> Result<FetchedSource, FetchError> {
    let url = source.url.as_str();
    let cached = if force_refresh {
        None
    } else {
        cache.load(&source.id, url)
    };

    if force_refresh {
        debug!(source = %source.id, "force-refresh bypasses cache validators");
    }

    let mut last_error = None;
    for attempt in 1..=MAX_ATTEMPTS {
        info!(
            source = %source.id,
            url = %url,
            attempt,
            "fetch start"
        );

        match attempt_fetch(client, source, cached.as_ref(), force_refresh).await {
            Ok(FetchResult::Fresh { body, metadata }) => {
                info!(
                    source = %source.id,
                    bytes = body.len(),
                    "fetch success"
                );
                if !dry_run && let Err(err) = cache.store(&source.id, &metadata, &body) {
                    warn!(
                        source = %source.id,
                        cache = %cache.root().display(),
                        error = %err,
                        "failed to update cache"
                    );
                }
                return Ok(FetchedSource {
                    body,
                    from_cache: false,
                    stale: false,
                });
            }
            Ok(FetchResult::NotModified) => {
                if let Some(cached) = cached {
                    info!(source = %source.id, "fetch not modified; using cache");
                    return Ok(FetchedSource {
                        body: cached.body,
                        from_cache: true,
                        stale: false,
                    });
                }
                last_error = Some("server returned 304 without usable cache".to_string());
            }
            Err(err) => {
                warn!(
                    source = %source.id,
                    attempt,
                    error = %err,
                    "fetch attempt failed"
                );
                last_error = Some(err);
            }
        }

        if attempt < MAX_ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(100 * attempt as u64)).await;
        }
    }

    if !force_refresh && let Some(cached) = cached {
        warn!(source = %source.id, "fetch failed; using stale cache");
        return Ok(FetchedSource {
            body: cached.body,
            from_cache: true,
            stale: true,
        });
    }

    Err(FetchError::Unavailable {
        source_id: source.id.clone(),
        message: last_error.unwrap_or_else(|| "unknown fetch error".to_string()),
    })
}

enum FetchResult {
    Fresh {
        body: Vec<u8>,
        metadata: CacheMetadata,
    },
    NotModified,
}

async fn attempt_fetch(
    client: &Client,
    source: &SourceConfig,
    cached: Option<&CachedSource>,
    force_refresh: bool,
) -> Result<FetchResult, String> {
    let mut request = client.get(source.url.clone());

    if !force_refresh && let Some(cached) = cached {
        if let Some(etag) = &cached.metadata.etag {
            request = request.header(IF_NONE_MATCH, etag);
        }
        if let Some(last_modified) = &cached.metadata.last_modified {
            request = request.header(IF_MODIFIED_SINCE, last_modified);
        }
    }

    let response = request.send().await.map_err(|err| err.to_string())?;
    if response.status() == StatusCode::NOT_MODIFIED {
        return Ok(FetchResult::NotModified);
    }
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }

    let etag = header_to_string(response.headers().get(ETAG));
    let last_modified = header_to_string(response.headers().get(LAST_MODIFIED));
    let body = response
        .bytes()
        .await
        .map_err(|err| format!("failed to read body: {err}"))?
        .to_vec();

    Ok(FetchResult::Fresh {
        metadata: CacheMetadata {
            url: source.url.to_string(),
            etag,
            last_modified,
        },
        body,
    })
}

fn header_to_string(value: Option<&reqwest::header::HeaderValue>) -> Option<String> {
    value
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;
    use std::path::Path;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };

    #[tokio::test]
    async fn fetches_and_reuses_304_cache() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/list"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"abc\"")
                    .set_body_bytes("example.com"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let config = Config::from_toml_str(
            &format!(
                r#"
                [sources]
                a = "{}/list"
                [outputs]
                a = "/tmp/a.cdb"
                "#,
                server.uri()
            ),
            Path::new("test.toml"),
        )
        .unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(cache_dir.path());
        let selected = config.selected_source_ids();

        let first = fetch_sources(&config, &selected, &cache, false, false).await;
        assert!(matches!(first["a"], FetchOutcome::Available(_)));

        let server = MockServer::start().await;
        let config = Config::from_toml_str(
            &format!(
                r#"
                [sources]
                a = "{}/list"
                [outputs]
                a = "/tmp/a.cdb"
                "#,
                server.uri()
            ),
            Path::new("test.toml"),
        )
        .unwrap();

        cache
            .store(
                "a",
                &CacheMetadata {
                    url: format!("{}/list", server.uri()),
                    etag: Some("\"abc\"".into()),
                    last_modified: None,
                },
                b"example.com",
            )
            .unwrap();

        Mock::given(method("GET"))
            .and(path("/list"))
            .and(header("if-none-match", "\"abc\""))
            .respond_with(ResponseTemplate::new(304))
            .expect(1)
            .mount(&server)
            .await;

        let second = fetch_sources(&config, &selected, &cache, false, false).await;
        match &second["a"] {
            FetchOutcome::Available(source) => {
                assert_eq!(source.body, b"example.com");
                assert!(source.from_cache);
                assert!(!source.stale);
            }
            FetchOutcome::Unavailable { error } => panic!("{error}"),
        }
    }

    #[tokio::test]
    async fn uses_stale_cache_after_failed_retries() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/list"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let config = Config::from_toml_str(
            &format!(
                r#"
                [sources]
                a = "{}/list"
                [outputs]
                a = "/tmp/a.cdb"
                "#,
                server.uri()
            ),
            Path::new("test.toml"),
        )
        .unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(cache_dir.path());
        cache
            .store(
                "a",
                &CacheMetadata {
                    url: format!("{}/list", server.uri()),
                    etag: None,
                    last_modified: None,
                },
                b"cached.example",
            )
            .unwrap();

        let result =
            fetch_sources(&config, &config.selected_source_ids(), &cache, false, false).await;
        match &result["a"] {
            FetchOutcome::Available(source) => {
                assert_eq!(source.body, b"cached.example");
                assert!(source.from_cache);
                assert!(source.stale);
            }
            FetchOutcome::Unavailable { error } => panic!("{error}"),
        }
    }

    #[tokio::test]
    async fn decodes_gzip_brotli_zstd_and_deflate() {
        let cases = [
            ("gzip", gzip(b"gzip.example")),
            ("br", brotli_bytes(b"brotli.example")),
            (
                "zstd",
                zstd::stream::encode_all(&b"zstd.example"[..], 0).unwrap(),
            ),
            ("deflate", deflate(b"deflate.example")),
        ];

        for (encoding, body) in cases {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/list"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("content-encoding", encoding)
                        .set_body_bytes(body),
                )
                .mount(&server)
                .await;

            let config = Config::from_toml_str(
                &format!(
                    r#"
                    [sources]
                    a = "{}/list"
                    [outputs]
                    a = "/tmp/a.cdb"
                    "#,
                    server.uri()
                ),
                Path::new("test.toml"),
            )
            .unwrap();

            let cache_dir = tempfile::tempdir().unwrap();
            let result = fetch_sources(
                &config,
                &config.selected_source_ids(),
                &Cache::new(cache_dir.path()),
                false,
                false,
            )
            .await;
            match &result["a"] {
                FetchOutcome::Available(source) => {
                    assert!(
                        std::str::from_utf8(&source.body)
                            .unwrap()
                            .ends_with(".example"),
                        "{encoding}"
                    );
                }
                FetchOutcome::Unavailable { error } => panic!("{encoding}: {error}"),
            }
        }
    }

    fn gzip(input: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(input).unwrap();
        encoder.finish().unwrap()
    }

    fn deflate(input: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(input).unwrap();
        encoder.finish().unwrap()
    }

    fn brotli_bytes(input: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        {
            let mut compressor = brotli::CompressorWriter::new(&mut output, 4096, 5, 22);
            compressor.write_all(input).unwrap();
        }
        output
    }
}
