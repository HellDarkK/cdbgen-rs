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
    outputs: Option<BTreeMap<String, RawOutputPaths>>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawOutputPaths {
    One(String),
    Many(Vec<String>),
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
        for (group, raw_paths) in raw_outputs {
            let source_ids = parse_output_group(&group);
            if source_ids.is_empty() {
                return Err(ConfigError::EmptyOutputGroup { group });
            }
            for source_id in &source_ids {
                if !sources.contains_key(source_id) {
                    return Err(ConfigError::UnknownOutputSource {
                        group: group.clone(),
                        source_id: source_id.clone(),
                    });
                }
            }

            let paths = raw_paths.into_paths();
            if paths.is_empty() {
                return Err(ConfigError::EmptyOutputPath { group });
            }

            for path in paths {
                if path.trim().is_empty() {
                    return Err(ConfigError::EmptyOutputPath {
                        group: group.clone(),
                    });
                }

                outputs.push(OutputConfig {
                    group: group.clone(),
                    source_ids: source_ids.clone(),
                    path: PathBuf::from(path),
                });
            }
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

impl RawOutputPaths {
    fn into_paths(self) -> Vec<String> {
        match self {
            Self::One(path) => parse_quoted_path_list(&path).unwrap_or_else(|| vec![path]),
            Self::Many(paths) => paths,
        }
    }
}

fn parse_quoted_path_list(value: &str) -> Option<Vec<String>> {
    let parts = value.split(',').collect::<Vec<_>>();
    if parts.len() < 2 {
        return None;
    }

    let mut paths = Vec::with_capacity(parts.len());
    for part in parts {
        let trimmed = part.trim();
        let mut chars = trimmed.chars();
        let quote = chars.next()?;
        if quote != '\'' && quote != '"' {
            return None;
        }
        if trimmed.len() < quote.len_utf8() * 2 {
            return None;
        }
        if !trimmed.ends_with(quote) {
            return None;
        }
        paths.push(trimmed[quote.len_utf8()..trimmed.len() - quote.len_utf8()].to_owned());
    }

    Some(paths)
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
    fn expands_output_path_array() {
        let cfg = parse(
            r#"
            [sources]
            a = "https://example.com/a"

            [outputs]
            a = ["/tmp/a.cdb", "/tmp/b.cdb"]
            "#,
        )
        .unwrap();

        assert_eq!(cfg.outputs.len(), 2);
        assert_eq!(cfg.outputs[0].path, PathBuf::from("/tmp/a.cdb"));
        assert_eq!(cfg.outputs[1].path, PathBuf::from("/tmp/b.cdb"));
        assert_eq!(cfg.outputs[0].source_ids, vec!["a"]);
        assert_eq!(cfg.outputs[1].source_ids, vec!["a"]);
    }

    #[test]
    fn expands_quoted_comma_output_path_string() {
        let cfg = parse(
            r#"
            [sources]
            a = "https://example.com/a"

            [outputs]
            a = "'/tmp/a.cdb', '/tmp/b.cdb'"
            "#,
        )
        .unwrap();

        assert_eq!(cfg.outputs.len(), 2);
        assert_eq!(cfg.outputs[0].path, PathBuf::from("/tmp/a.cdb"));
        assert_eq!(cfg.outputs[1].path, PathBuf::from("/tmp/b.cdb"));
    }

    #[test]
    fn rejects_empty_output_path_array() {
        let err = parse(
            r#"
            [sources]
            a = "https://example.com/a"

            [outputs]
            a = []
            "#,
        )
        .unwrap_err();

        assert!(matches!(err, ConfigError::EmptyOutputPath { .. }));
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

    #[test]
    fn example_config_is_valid() {
        let cfg = Config::from_toml_str(
            include_str!("../config.example.toml"),
            Path::new("config.example.toml"),
        )
        .unwrap();

        assert_eq!(cfg.outputs.len(), 3);
        assert_eq!(
            cfg.sources.keys().cloned().collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "adblock_example".to_string(),
                "hosts_example".to_string(),
                "rpz_example".to_string(),
                "unbound_example".to_string(),
            ])
        );
        assert_example_output(
            &cfg,
            "/var/lib/cdbgen/default.cdb",
            &["hosts_example", "adblock_example"],
        );
        assert_example_output(
            &cfg,
            "/var/lib/cdbgen/combined.cdb",
            &["hosts_example", "unbound_example", "rpz_example"],
        );
        assert_example_output(
            &cfg,
            "/srv/www/blocklists/combined.cdb",
            &["hosts_example", "unbound_example", "rpz_example"],
        );
    }

    fn assert_example_output(cfg: &Config, path: &str, source_ids: &[&str]) {
        let output = cfg
            .outputs
            .iter()
            .find(|output| output.path == Path::new(path))
            .unwrap_or_else(|| panic!("missing output {path}"));
        assert_eq!(
            output.source_ids,
            source_ids
                .iter()
                .map(|source_id| (*source_id).to_string())
                .collect::<Vec<_>>()
        );
    }
}
