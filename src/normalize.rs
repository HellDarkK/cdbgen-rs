use std::{net::IpAddr, str::FromStr};

pub fn normalize_domain(input: &str) -> Option<String> {
    let trimmed = input.trim().trim_end_matches('.');
    if trimmed.is_empty()
        || trimmed.contains('*')
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains(':')
    {
        return None;
    }

    // Fast path: if the input is already a valid ASCII-LDH domain that is
    // already lowercase, skip the idna round-trip. The validation here is a
    // strict subset of `idna::domain_to_ascii` defaults (UTS #46) so the
    // observable behaviour is unchanged. We also reject inputs that parse as
    // an IP address, matching the slow-path behaviour below.
    if let Some(ascii) = is_already_ascii_normalized(trimmed) {
        if IpAddr::from_str(ascii).is_err() {
            return Some(ascii.to_string());
        }
        return None;
    }

    let ascii = idna::domain_to_ascii(trimmed).ok()?.to_ascii_lowercase();
    if ascii.len() > 253 || ascii.is_empty() || IpAddr::from_str(&ascii).is_ok() {
        return None;
    }

    let labels: Vec<&str> = ascii.split('.').collect();
    if labels.len() < 2 {
        return None;
    }

    for label in labels {
        if label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return None;
        }
    }

    Some(ascii)
}

/// Returns `Some(&str)` if `s` is already a valid ASCII lowercase LDH domain
/// that `idna::domain_to_ascii` would leave unchanged. Otherwise returns None
/// and the caller should fall through to the slow idna path.
fn is_already_ascii_normalized(s: &str) -> Option<&str> {
    if s.is_empty() || s.len() > 253 {
        return None;
    }
    let bytes = s.as_bytes();
    let mut label_start = 0usize;
    let mut label_len = 0usize;
    let mut label_count = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'.' => {
                if !is_valid_label(bytes, label_start, label_len) {
                    return None;
                }
                label_count += 1;
                label_start = i + 1;
                label_len = 0;
            }
            b'a'..=b'z' | b'0'..=b'9' | b'-' => {
                label_len += 1;
                if label_len > 63 {
                    return None;
                }
            }
            _ => return None, // non-LDH, uppercase, or other invalid
        }
    }
    // Final label.
    if !is_valid_label(bytes, label_start, label_len) {
        return None;
    }
    label_count += 1;
    if label_count < 2 {
        return None;
    }
    Some(s)
}

#[inline]
fn is_valid_label(bytes: &[u8], start: usize, len: usize) -> bool {
    len >= 1
        && len <= 63
        && bytes[start] != b'-'
        && bytes[start + len - 1] != b'-'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercases_and_trims_trailing_dot() {
        assert_eq!(
            normalize_domain(" Example.COM. "),
            Some("example.com".to_string())
        );
    }

    #[test]
    fn converts_idna() {
        assert_eq!(
            normalize_domain("bücher.example"),
            Some("xn--bcher-kva.example".to_string())
        );
    }

    #[test]
    fn rejects_invalid_domains() {
        for input in [
            "",
            "localhost",
            "127.0.0.1",
            "*.example.com",
            "-bad.example",
            "bad-.example",
            "bad_label.example",
            "example.com/path",
        ] {
            assert_eq!(normalize_domain(input), None, "{input}");
        }
    }
}
