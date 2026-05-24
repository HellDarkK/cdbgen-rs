use std::{
    collections::{BTreeMap, BTreeSet},
    net::IpAddr,
    str::FromStr,
};

use crate::{error::AppError, fetcher::FetchedSource, normalize::normalize_domain};

#[derive(Debug, Clone, Default)]
pub struct ParseStats {
    pub total_lines: usize,
    pub blank_lines: usize,
    pub comment_lines: usize,
    pub accepted_blocks: usize,
    pub accepted_allows: usize,
    pub rejected_lines: usize,
    pub duplicate_blocks: usize,
    pub duplicate_allows: usize,
}

#[derive(Debug, Clone)]
pub struct ParsedSource {
    pub blocks: BTreeSet<String>,
    pub allows: BTreeSet<String>,
    pub stats: ParseStats,
}

pub async fn parse_sources(
    sources: BTreeMap<String, FetchedSource>,
) -> Result<BTreeMap<String, ParsedSource>, AppError> {
    let mut tasks = Vec::new();
    for (source_id, fetched) in sources {
        tasks.push(tokio::task::spawn_blocking(move || {
            let parsed = parse_source_bytes(&fetched.body);
            (source_id, parsed)
        }));
    }

    let mut parsed = BTreeMap::new();
    for task in tasks {
        let (source_id, source) = task
            .await
            .map_err(|err| AppError::ParseTask(err.to_string()))?;
        parsed.insert(source_id, source);
    }

    Ok(parsed)
}

pub fn parse_source_bytes(body: &[u8]) -> ParsedSource {
    let text = String::from_utf8_lossy(body);
    let mut parsed = ParsedSource {
        blocks: BTreeSet::new(),
        allows: BTreeSet::new(),
        stats: ParseStats::default(),
    };

    for (index, raw_line) in text.lines().enumerate() {
        let mut line = raw_line.trim();
        if index == 0 {
            line = line.trim_start_matches('\u{feff}');
        }

        parsed.stats.total_lines += 1;
        if line.is_empty() {
            parsed.stats.blank_lines += 1;
            continue;
        }
        if is_comment(line) {
            parsed.stats.comment_lines += 1;
            continue;
        }

        let before_blocks = parsed.blocks.len();
        let before_allows = parsed.allows.len();

        let extracted = extract_domains(line);
        if extracted.is_empty() {
            parsed.stats.rejected_lines += 1;
            continue;
        }

        for domain in extracted {
            match domain.kind {
                RuleKind::Block => {
                    if !parsed.blocks.insert(domain.domain) {
                        parsed.stats.duplicate_blocks += 1;
                    }
                }
                RuleKind::Allow => {
                    if !parsed.allows.insert(domain.domain) {
                        parsed.stats.duplicate_allows += 1;
                    }
                }
            }
        }

        parsed.stats.accepted_blocks += parsed.blocks.len() - before_blocks;
        parsed.stats.accepted_allows += parsed.allows.len() - before_allows;
    }

    parsed
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuleKind {
    Block,
    Allow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExtractedDomain {
    kind: RuleKind,
    domain: String,
}

fn extract_domains(line: &str) -> Vec<ExtractedDomain> {
    if let Some(domain) = extract_unbound_line(line) {
        return vec![ExtractedDomain {
            kind: RuleKind::Block,
            domain,
        }];
    }

    if let Some(domain) = extract_rpz_line(line) {
        return vec![ExtractedDomain {
            kind: RuleKind::Block,
            domain,
        }];
    }

    if let Some(domains) = extract_hosts_line(line) {
        return domains
            .into_iter()
            .map(|domain| ExtractedDomain {
                kind: RuleKind::Block,
                domain,
            })
            .collect();
    }

    let (kind, body) = if let Some(rest) = line.strip_prefix("@@") {
        (RuleKind::Allow, rest.trim())
    } else {
        (RuleKind::Block, line)
    };

    extract_filter_domain(body)
        .into_iter()
        .map(|domain| ExtractedDomain { kind, domain })
        .collect()
}

fn extract_unbound_line(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed.strip_prefix("local-zone:") {
        return extract_first_quoted_value(rest).and_then(normalize_domain);
    }
    if let Some(rest) = trimmed.strip_prefix("local-data:") {
        return extract_first_quoted_value(rest)
            .and_then(|value| value.split_whitespace().next().and_then(normalize_domain));
    }
    None
}

fn extract_first_quoted_value(input: &str) -> Option<&str> {
    let start = input.find('"')? + 1;
    let rest = &input[start..];
    let end = rest.find('"')?;
    Some(rest[..end].trim())
}

fn extract_rpz_line(line: &str) -> Option<String> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    if tokens.len() < 2 || !is_rpz_record_type(tokens[1]) {
        return None;
    }

    let owner = tokens[0].trim_end_matches('.');
    let owner = owner.strip_prefix("*.").unwrap_or(owner);
    if owner == "@" {
        return None;
    }

    normalize_domain(owner)
}

fn is_rpz_record_type(value: &str) -> bool {
    matches!(value.to_ascii_uppercase().as_str(), "CNAME" | "A" | "AAAA")
}

fn extract_hosts_line(line: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    for token in line.split_whitespace() {
        if is_inline_comment_token(token) {
            break;
        }
        tokens.push(token);
    }

    let first = tokens.first()?;
    if IpAddr::from_str(first).is_err() {
        return None;
    }

    let domains: Vec<String> = tokens
        .into_iter()
        .skip(1)
        .filter_map(normalize_domain)
        .collect();

    Some(domains)
}

fn extract_filter_domain(line: &str) -> Option<String> {
    let without_options = line.split_once('$').map_or(line, |(before, _)| before);
    let candidate = without_options.trim();

    if candidate.is_empty()
        || candidate.starts_with('/')
        || candidate.contains('*')
        || candidate.contains("##")
        || candidate.contains("#@#")
        || candidate.contains("#?#")
        || candidate.contains("#$#")
    {
        return None;
    }

    if let Some(rest) = candidate.strip_prefix("||") {
        return extract_double_anchor(rest);
    }

    if let Some(rest) = candidate.strip_prefix('|') {
        if rest.starts_with("http://") || rest.starts_with("https://") {
            return None;
        }
        return extract_until_exact_boundary(rest);
    }

    if candidate.contains('/') || candidate.contains('?') || candidate.contains('#') {
        return None;
    }

    let bare = candidate.trim_end_matches('^').trim_end_matches('|').trim();
    normalize_domain(bare)
}

fn extract_double_anchor(rest: &str) -> Option<String> {
    extract_until_exact_boundary(rest)
}

fn extract_until_exact_boundary(rest: &str) -> Option<String> {
    let mut end = rest.len();
    let mut boundary = None;

    for (idx, ch) in rest.char_indices() {
        if matches!(ch, '^' | '|' | '/' | ':' | '?' | '#') {
            end = idx;
            boundary = Some(ch);
            break;
        }
    }

    if matches!(boundary, Some('/' | ':' | '?' | '#')) {
        return None;
    }

    normalize_domain(&rest[..end])
}

fn is_comment(line: &str) -> bool {
    line.starts_with('#')
        || line.starts_with('!')
        || line.starts_with(';')
        || line.starts_with("//")
}

fn is_inline_comment_token(token: &str) -> bool {
    token.starts_with('#') || token.starts_with(';') || token.starts_with("//")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_hosts_and_adblock() {
        let parsed = parse_source_bytes(
            br#"
            # comment
            Example.COM
            0.0.0.0 ads.example.com alias.example.net # inline
            127.0.0.1 tracker.example.net
            ||agh.example.org^
            @@||allow.example.org^
            /regex/
            ||path.example/path.js
            ||wild*.example^
            "#,
        );

        assert!(parsed.blocks.contains("example.com"));
        assert!(parsed.blocks.contains("ads.example.com"));
        assert!(parsed.blocks.contains("alias.example.net"));
        assert!(parsed.blocks.contains("tracker.example.net"));
        assert!(parsed.blocks.contains("agh.example.org"));
        assert!(parsed.allows.contains("allow.example.org"));
        assert!(!parsed.blocks.contains("path.example"));
        assert_eq!(parsed.stats.comment_lines, 1);
        assert!(parsed.stats.rejected_lines >= 3);
    }

    #[test]
    fn parses_unbound_and_rpz_lines() {
        let parsed = parse_source_bytes(
            br#"
            server:
            local-zone: "ads.example.com." always_null
            local-data: "tracker.example.net. A 0.0.0.0"
            $TTL 3600
            @ SOA localhost. root.localhost. 1 14400 3600 86400 3600
            example.org CNAME .
            *.wild.example.org CNAME .
            "#,
        );

        assert!(parsed.blocks.contains("ads.example.com"));
        assert!(parsed.blocks.contains("tracker.example.net"));
        assert!(parsed.blocks.contains("example.org"));
        assert!(parsed.blocks.contains("wild.example.org"));
        assert!(!parsed.blocks.contains("localhost"));
    }

    #[test]
    fn skips_whole_line_adblock_comments() {
        let parsed = parse_source_bytes(b"! title\n; comment\n// comment\n# comment\n");
        assert!(parsed.blocks.is_empty());
        assert_eq!(parsed.stats.comment_lines, 4);
    }

    #[test]
    fn tracks_duplicates() {
        let parsed =
            parse_source_bytes(b"example.com\nExample.COM.\n@@||allow.example^\n@@allow.example\n");
        assert_eq!(parsed.blocks.len(), 1);
        assert_eq!(parsed.allows.len(), 1);
        assert_eq!(parsed.stats.duplicate_blocks, 1);
        assert_eq!(parsed.stats.duplicate_allows, 1);
    }
}
