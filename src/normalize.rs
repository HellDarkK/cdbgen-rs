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
