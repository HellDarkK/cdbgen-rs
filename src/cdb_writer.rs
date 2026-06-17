use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{cdb_fast::FastCdbMake, config::OutputKeyFormat, error::OutputError};

pub fn write_cdb_atomic(
    path: &Path,
    domains: &[String],
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
    domains: &[String],
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

    let mut writer = FastCdbMake::new(file).map_err(|source| OutputError::CreateTemp {
        path: tmp_path.to_path_buf(),
        source,
    })?;
    // Pre-allocate the entries vec so add() never reallocates.
    writer.reserve(domains.len());

    match key_format {
        OutputKeyFormat::Wire => {
            // Stack-allocated wire-key buffer (max 255 bytes per RFC 1035).
            // Reusing a stack buffer across iterations avoids the per-domain
            // Vec allocation that `domain_to_wire` would do.
            let mut key_buf = [0u8; 255];
            for domain in domains {
                let key_len = match encode_wire_into(&mut key_buf, domain) {
                    Ok(n) => n,
                    Err(_) => {
                        return Err(OutputError::InvalidWireKey {
                            path: final_path.to_path_buf(),
                            domain: domain.clone(),
                        });
                    }
                };
                if let Err(source) = writer.add(&key_buf[..key_len], b"") {
                    return Err(OutputError::WriteRecord {
                        path: final_path.to_path_buf(),
                        source,
                    });
                }
            }
        }
        OutputKeyFormat::Plaintext => {
            for domain in domains {
                if let Err(source) = writer.add(domain.as_bytes(), b"") {
                    return Err(OutputError::WriteRecord {
                        path: final_path.to_path_buf(),
                        source,
                    });
                }
            }
        }
    }
    writer.finish().map_err(|source| OutputError::Finalize {
        path: final_path.to_path_buf(),
        source,
    })?;
    // `sync_data` (not `sync_all`) is sufficient here: the directory fsync
    // below persists the file's metadata (size, mtime, etc.) as part of the
    // directory entry, so we only need to flush the file's data blocks.
    File::open(tmp_path)
        .and_then(|file| file.sync_data())
        .map_err(|source| OutputError::Fsync {
            path: tmp_path.to_path_buf(),
            source,
        })?;

    Ok(())
}

/// Encode a domain as a DNS wire-format qname into a stack-allocated buffer.
/// Returns the number of bytes written.
///
/// Tighter than `&str::split('.')`-based encoders because there's no per-call
/// iterator and no `extend_from_slice` bounds checks per label.
#[inline]
fn encode_wire_into(buf: &mut [u8; 255], domain: &str) -> Result<usize, WireKeyError> {
    let bytes = domain.as_bytes();
    if bytes.is_empty() || bytes.len() > 253 {
        return Err(WireKeyError);
    }
    // Fast ASCII check: any byte >= 0x80 is non-ASCII.
    for &b in bytes {
        if b >= 0x80 {
            return Err(WireKeyError);
        }
    }
    let mut out = 0usize;
    let mut last_dot = 0usize;
    let mut i = 0usize;
    let len = bytes.len();
    while i < len {
        if bytes[i] == b'.' {
            let label_len = i - last_dot;
            if label_len == 0 || label_len > 63 {
                return Err(WireKeyError);
            }
            buf[out] = label_len as u8;
            out += 1;
            // SAFETY: buf is 255 bytes; total output never exceeds 255.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr().add(last_dot),
                    buf.as_mut_ptr().add(out),
                    label_len,
                );
            }
            out += label_len;
            i += 1;
            last_dot = i;
        } else {
            i += 1;
        }
    }
    let label_len = len - last_dot;
    if label_len == 0 || label_len > 63 {
        return Err(WireKeyError);
    }
    buf[out] = label_len as u8;
    out += 1;
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr().add(last_dot),
            buf.as_mut_ptr().add(out),
            label_len,
        );
    }
    out += label_len;
    buf[out] = 0;
    out += 1;
    debug_assert!(out <= 255, "wire key overflow: {out}");
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireKeyError;

/// Public API: encode a domain as a DNS wire-format qname (used by tests
/// and any external code that wants the wire bytes for a single domain).
pub fn domain_to_wire(domain: &str) -> Result<Vec<u8>, WireKeyError> {
    let mut wire = Vec::with_capacity(domain.len() + 2);
    let mut stack = [0u8; 255];
    let n = encode_wire_into(&mut stack, domain)?;
    wire.extend_from_slice(&stack[..n]);
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
        let domains = vec!["a.example".to_string(), "b.example".to_string()];

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
        let domains = vec!["a.example".to_string()];

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
        let domains = vec![
            "z.example".to_string(),
            "a.example".to_string(),
            "m.example".to_string(),
        ];

        write_cdb_atomic(&path_a, &domains, OutputKeyFormat::Wire).unwrap();
        write_cdb_atomic(&path_b, &domains, OutputKeyFormat::Wire).unwrap();

        assert_eq!(fs::read(path_a).unwrap(), fs::read(path_b).unwrap());
    }
}
