use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct CachedSource {
    pub body: Vec<u8>,
    pub metadata: CacheMetadata,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct CacheMetadata {
    pub url: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

impl Cache {
    pub fn default_root() -> Self {
        Self::new("/var/cache/cdbgen-rs")
    }

    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { root: path.into() }
    }

    pub fn load(&self, source_id: &str, url: &str) -> Option<CachedSource> {
        let metadata_path = self.metadata_path(source_id);
        let body_path = self.body_path(source_id);
        let metadata_text = fs::read_to_string(metadata_path).ok()?;
        let metadata: CacheMetadata = toml::from_str(&metadata_text).ok()?;
        if metadata.url != url {
            return None;
        }
        let body = fs::read(body_path).ok()?;
        Some(CachedSource { body, metadata })
    }

    pub fn store(
        &self,
        source_id: &str,
        metadata: &CacheMetadata,
        body: &[u8],
    ) -> std::io::Result<()> {
        fs::create_dir_all(&self.root)?;
        let body_path = self.body_path(source_id);
        let metadata_path = self.metadata_path(source_id);
        let tmp_body = self.tmp_path(source_id, "body");
        let tmp_meta = self.tmp_path(source_id, "toml");

        fs::write(&tmp_body, body)?;
        fs::write(&tmp_meta, toml::to_string(metadata).unwrap_or_default())?;
        fs::rename(&tmp_body, body_path)?;
        fs::rename(&tmp_meta, metadata_path)?;
        Ok(())
    }

    fn body_path(&self, source_id: &str) -> PathBuf {
        self.root.join(format!("{source_id}.body"))
    }

    fn metadata_path(&self, source_id: &str) -> PathBuf {
        self.root.join(format!("{source_id}.toml"))
    }

    fn tmp_path(&self, source_id: &str, suffix: &str) -> PathBuf {
        let pid = std::process::id();
        self.root.join(format!("{source_id}.{pid}.{suffix}.tmp"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_loads_matching_url() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path());
        let metadata = CacheMetadata {
            url: "https://example.com/a".into(),
            etag: Some("\"abc\"".into()),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".into()),
        };

        cache.store("a", &metadata, b"body").unwrap();
        let loaded = cache.load("a", "https://example.com/a").unwrap();

        assert_eq!(loaded.body, b"body");
        assert_eq!(loaded.metadata.etag.as_deref(), Some("\"abc\""));
        assert!(cache.load("a", "https://example.com/b").is_none());
    }
}
