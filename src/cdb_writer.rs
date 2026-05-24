use std::{
    borrow::Cow,
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use cdb::CDBMake;

use crate::{config::OutputKeyFormat, error::OutputError};

pub fn write_cdb_atomic(
    path: &Path,
    domains: &BTreeSet<String>,
    key_format: OutputKeyFormat,
) -> Result<(), OutputError> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| OutputError::CreateParent {
        path: parent.to_path_buf(),
        source,
    })?;

    let tmp_path = temp_output_path(path);
    let result = write_cdb_to_temp(&tmp_path, path, domains, key_format).and_then(|_| {
        fs::rename(&tmp_path, path).map_err(|source| OutputError::Rename {
            path: path.to_path_buf(),
            source,
        })?;
        fsync_dir(parent)?;
        Ok(())
    });

    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }

    result
}

fn write_cdb_to_temp(
    tmp_path: &Path,
    final_path: &Path,
    domains: &BTreeSet<String>,
    key_format: OutputKeyFormat,
) -> Result<(), OutputError> {
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(tmp_path)
        .map_err(|source| OutputError::CreateTemp {
            path: tmp_path.to_path_buf(),
            source,
        })?;

    let mut writer = CDBMake::new(file).map_err(|source| OutputError::CreateTemp {
        path: tmp_path.to_path_buf(),
        source,
    })?;
    for domain in domains {
        let key = cdb_key(domain, key_format).map_err(|_| OutputError::InvalidWireKey {
            path: final_path.to_path_buf(),
            domain: domain.clone(),
        })?;
        writer
            .add(&key, b"")
            .map_err(|source| OutputError::WriteRecord {
                path: final_path.to_path_buf(),
                source,
            })?;
    }

    writer.finish().map_err(|source| OutputError::Finalize {
        path: final_path.to_path_buf(),
        source,
    })?;

    File::open(tmp_path)
        .and_then(|file| file.sync_all())
        .map_err(|source| OutputError::Fsync {
            path: tmp_path.to_path_buf(),
            source,
        })?;

    Ok(())
}

fn cdb_key(domain: &str, key_format: OutputKeyFormat) -> Result<Cow<'_, [u8]>, WireKeyError> {
    match key_format {
        OutputKeyFormat::Wire => domain_to_wire(domain).map(Cow::Owned),
        OutputKeyFormat::Plaintext => Ok(Cow::Borrowed(domain.as_bytes())),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireKeyError;

pub fn domain_to_wire(domain: &str) -> Result<Vec<u8>, WireKeyError> {
    if domain.is_empty() || domain.len() > 253 || !domain.is_ascii() {
        return Err(WireKeyError);
    }

    let mut wire = Vec::with_capacity(domain.len() + 2);
    for label in domain.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(WireKeyError);
        }
        wire.push(label.len() as u8);
        wire.extend_from_slice(label.as_bytes());
    }
    wire.push(0);

    if wire.len() > 255 {
        return Err(WireKeyError);
    }

    Ok(wire)
}

fn temp_output_path(path: &Path) -> PathBuf {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("output.cdb");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    parent.join(format!(".{file_name}.{}.{}.tmp", std::process::id(), now))
}

fn fsync_dir(path: &Path) -> Result<(), OutputError> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|source| OutputError::Fsync {
            path: path.to_path_buf(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OutputKeyFormat;

    #[test]
    fn encodes_domain_as_dns_wire_qname() {
        assert_eq!(
            domain_to_wire("example.com").unwrap(),
            b"\x07example\x03com\x00"
        );
    }

    #[test]
    fn writes_readable_cdb_with_empty_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.cdb");
        let domains = BTreeSet::from(["a.example".to_string(), "b.example".to_string()]);

        write_cdb_atomic(&path, &domains, OutputKeyFormat::Wire).unwrap();

        let db = cdb::CDB::open(&path).unwrap();
        assert_eq!(
            db.get(&domain_to_wire("a.example").unwrap())
                .unwrap()
                .unwrap(),
            b""
        );
        assert_eq!(
            db.get(&domain_to_wire("b.example").unwrap())
                .unwrap()
                .unwrap(),
            b""
        );
        assert!(db.get(b"a.example").is_none());
        assert!(db.get(&domain_to_wire("c.example").unwrap()).is_none());
    }

    #[test]
    fn plaintext_format_preserves_legacy_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.cdb");
        let domains = BTreeSet::from(["a.example".to_string()]);

        write_cdb_atomic(&path, &domains, OutputKeyFormat::Plaintext).unwrap();

        let db = cdb::CDB::open(&path).unwrap();
        assert_eq!(db.get(b"a.example").unwrap().unwrap(), b"");
        assert!(db.get(&domain_to_wire("a.example").unwrap()).is_none());
    }

    #[test]
    fn output_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let path_a = dir.path().join("a.cdb");
        let path_b = dir.path().join("b.cdb");
        let domains = BTreeSet::from([
            "z.example".to_string(),
            "a.example".to_string(),
            "m.example".to_string(),
        ]);

        write_cdb_atomic(&path_a, &domains, OutputKeyFormat::Wire).unwrap();
        write_cdb_atomic(&path_b, &domains, OutputKeyFormat::Wire).unwrap();

        assert_eq!(fs::read(path_a).unwrap(), fs::read(path_b).unwrap());
    }
}
