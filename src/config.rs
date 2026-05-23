use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use url::Url;

use crate::error::ConfigError;

#[derive(Debug, Deserialize)]
struct RawConfig {
    sources: Option<BTreeMap<String, String>>,
    outputs: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub sources: BTreeMap<String, SourceConfig>,
    pub outputs: Vec<OutputConfig>,
}

#[derive(Debug, Clone)]
pub struct SourceConfig {
    pub id: String,
    pub url: Url,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputConfig {
    pub group: String,
    pub source_ids: Vec<String>,
    pub path: PathBuf,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml_str(&text, path)
    }

    pub fn from_toml_str(text: &str, path: &Path) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(text).map_err(|source| ConfigError::Toml {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_raw(raw)
    }

    fn from_raw(raw: RawConfig) -> Result<Self, ConfigError> {
        let raw_sources = raw.sources.ok_or(ConfigError::EmptySources)?;
        let raw_outputs = raw.outputs.ok_or(ConfigError::EmptyOutputs)?;

        if raw_sources.is_empty() {
            return Err(ConfigError::EmptySources);
        }
        if raw_outputs.is_empty() {
            return Err(ConfigError::EmptyOutputs);
        }

        let mut sources = BTreeMap::new();
        for (id, url) in raw_sources {
            if !is_valid_source_id(&id) {
                return Err(ConfigError::InvalidSourceId(id));
            }

            let parsed = Url::parse(&url).map_err(|_| ConfigError::InvalidUrl {
                source_id: id.clone(),
                url: url.clone(),
            })?;

            match parsed.scheme() {
                "http" | "https" => {}
                scheme => {
                    return Err(ConfigError::UnsupportedScheme {
                        source_id: id.clone(),
                        scheme: scheme.to_owned(),
                    });
                }
            }

            sources.insert(id.clone(), SourceConfig { id, url: parsed });
        }

        let mut outputs = Vec::new();
        for (group, path) in raw_outputs {
            let source_ids = parse_output_group(&group);
            if source_ids.is_empty() {
                return Err(ConfigError::EmptyOutputGroup { group });
            }
            if path.trim().is_empty() {
                return Err(ConfigError::EmptyOutputPath { group });
            }
            for source_id in &source_ids {
                if !sources.contains_key(source_id) {
                    return Err(ConfigError::UnknownOutputSource {
                        group,
                        source_id: source_id.clone(),
                    });
                }
            }

            outputs.push(OutputConfig {
                group,
                source_ids,
                path: PathBuf::from(path),
            });
        }

        Ok(Self { sources, outputs })
    }

    pub fn selected_source_ids(&self) -> BTreeSet<String> {
        self.outputs
            .iter()
            .flat_map(|output| output.source_ids.iter().cloned())
            .collect()
    }
}

fn parse_output_group(group: &str) -> Vec<String> {
    group
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn is_valid_source_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config, ConfigError> {
        Config::from_toml_str(text, Path::new("test.toml"))
    }

    #[test]
    fn parses_valid_config() {
        let cfg = parse(
            r#"
            [sources]
            "adblock-a" = "https://example.com/a"
            "adblock_b" = "http://example.com/b"

            [outputs]
            "adblock-a, adblock_b" = "/tmp/general.cdb"
            "#,
        )
        .unwrap();

        assert_eq!(cfg.sources.len(), 2);
        assert_eq!(cfg.outputs[0].source_ids, vec!["adblock-a", "adblock_b"]);
        assert_eq!(cfg.selected_source_ids().len(), 2);
    }

    #[test]
    fn rejects_invalid_source_id() {
        let err = parse(
            r#"
            [sources]
            "bad id" = "https://example.com/a"
            [outputs]
            "bad id" = "/tmp/a.cdb"
            "#,
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::InvalidSourceId(_)));
    }

    #[test]
    fn rejects_unknown_output_source() {
        let err = parse(
            r#"
            [sources]
            "a" = "https://example.com/a"
            [outputs]
            "a, b" = "/tmp/a.cdb"
            "#,
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::UnknownOutputSource { .. }));
    }

    #[test]
    fn rejects_unsupported_scheme() {
        let err = parse(
            r#"
            [sources]
            "a" = "ftp://example.com/a"
            [outputs]
            "a" = "/tmp/a.cdb"
            "#,
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::UnsupportedScheme { .. }));
    }
}
