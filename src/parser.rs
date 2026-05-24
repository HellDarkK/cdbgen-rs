use std::{
    collections::{BTreeMap, BTreeSet},
    net::IpAddr,
    str::FromStr,
};

use crate::{error::AppError, fetcher::FetchedSource, normalize::normalize_domain};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SourceFormat {
    #[default]
    Hostfile,
    Adblock,
    Unbound,
    Rpz,
}

impl SourceFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceFormat::Hostfile => "hostfile",
            SourceFormat::Adblock => "adblock",
            SourceFormat::Unbound => "unbound",
            SourceFormat::Rpz => "rpz",
        }
    }
}

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
    pub detected_format: SourceFormat,
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
        detected_format: detect_source_format(&text),
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

pub fn detect_source_format(text: &str) -> SourceFormat {
    #[derive(Default)]
    struct Scores {
        hostfile: usize,
        adblock: usize,
        unbound: usize,
        rpz: usize,
    }

    let mut scores = Scores::default();
    for (index, raw_line) in text.lines().enumerate() {
        let mut line = raw_line.trim();
        if index == 0 {
            line = line.trim_start_matches('\u{feff}');
        }
        if line.is_empty() || is_comment(line) {
            continue;
        }

        if looks_like_unbound_line(line) {
            scores.unbound += 4;
        }
        if extract_rpz_line(line).is_some() || looks_like_rpz_preamble(line) {
            scores.rpz += 4;
        }
        if looks_like_adblock_line(line) {
            scores.adblock += 3;
        }
        if !extract_hosts_line(line).unwrap_or_default().is_empty() {
            scores.hostfile += 1;
        }
    }

    [
        (SourceFormat::Unbound, scores.unbound),
        (SourceFormat::Rpz, scores.rpz),
        (SourceFormat::Adblock, scores.adblock),
        (SourceFormat::Hostfile, scores.hostfile),
    ]
    .into_iter()
    .max_by_key(|(_, score)| *score)
    .map(|(format, _)| format)
    .unwrap_or(SourceFormat::Hostfile)
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
        return extract_first_config_value(rest).and_then(normalize_domain);
    }
    if let Some(rest) = trimmed.strip_prefix("local-data:") {
        return extract_first_config_value(rest)
            .and_then(|value| value.split_whitespace().next().and_then(normalize_domain));
    }
    None
}

fn looks_like_unbound_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed == "server:" || trimmed.starts_with("local-zone:") || trimmed.starts_with("local-data:")
}

fn extract_first_config_value(input: &str) -> Option<&str> {
    let trimmed = input.trim();
    if let Some(rest) = trimmed.strip_prefix('"') {
        let end = rest.find('"')?;
        return Some(rest[..end].trim());
    }
    trimmed.split_whitespace().next()
}

fn extract_rpz_line(line: &str) -> Option<String> {
    let tokens = tokens_before_inline_comment(line);
    let (record_index, _) = tokens
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, token)| token.eq_ignore_ascii_case("CNAME"))?;

    if tokens.get(record_index + 1).copied() != Some(".") {
        return None;
    }

    let owner = tokens[0].trim_end_matches('.');
    let owner = owner.strip_prefix("*.").unwrap_or(owner);
    if owner == "@" || owner.starts_with('$') {
        return None;
    }

    normalize_domain(owner)
}

fn looks_like_rpz_preamble(line: &str) -> bool {
    let tokens = tokens_before_inline_comment(line);
    if tokens.is_empty() {
        return false;
    }

    tokens[0].starts_with('$')
        || tokens
            .iter()
            .skip(1)
            .any(|token| is_dns_class(token) || is_dns_record_type(token))
}

fn extract_hosts_line(line: &str) -> Option<Vec<String>> {
    let tokens = tokens_before_inline_comment(line);
    if tokens.is_empty() || looks_like_dns_record_line(&tokens) {
        return None;
    }

    let first = tokens[0];
    let domains = if IpAddr::from_str(first).is_ok() {
        tokens
            .into_iter()
            .skip(1)
            .filter_map(normalize_domain)
            .collect()
    } else if tokens
        .get(1)
        .is_none_or(|second| IpAddr::from_str(second).is_ok())
    {
        normalize_domain(first).into_iter().collect()
    } else {
        Vec::new()
    };

    if domains.is_empty() {
        None
    } else {
        Some(domains)
    }
}

fn tokens_before_inline_comment(line: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    for token in line.split_whitespace() {
        if is_inline_comment_token(token) {
            break;
        }
        tokens.push(token);
    }
    tokens
}

fn looks_like_dns_record_line(tokens: &[&str]) -> bool {
    tokens
        .iter()
        .skip(1)
        .take(4)
        .any(|token| is_dns_class(token) || is_dns_record_type(token))
}

fn is_dns_class(value: &str) -> bool {
    matches!(value.to_ascii_uppercase().as_str(), "IN" | "CH" | "HS")
}

fn is_dns_record_type(value: &str) -> bool {
    matches!(
        value.to_ascii_uppercase().as_str(),
        "A" | "AAAA"
            | "CAA"
            | "CNAME"
            | "DNSKEY"
            | "DS"
            | "HTTPS"
            | "MX"
            | "NS"
            | "PTR"
            | "SOA"
            | "SRV"
            | "SVCB"
            | "TXT"
    )
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

fn looks_like_adblock_line(line: &str) -> bool {
    let body = line.strip_prefix("@@").unwrap_or(line);
    body.starts_with("||")
        || body.starts_with('|')
        || body.contains('$')
        || body.contains("##")
        || body.contains("#@#")
        || body.contains("#?#")
        || body.contains("#$#")
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
    fn autodetects_supported_formats() {
        assert_eq!(
            parse_source_bytes(b"0.0.0.0 ads.example\n").detected_format,
            SourceFormat::Hostfile
        );
        assert_eq!(
            parse_source_bytes(b"||ads.example^\n@@||allow.example^\n").detected_format,
            SourceFormat::Adblock
        );
        assert_eq!(
            parse_source_bytes(br#"local-zone: "ads.example." always_null"#).detected_format,
            SourceFormat::Unbound
        );
        assert_eq!(
            parse_source_bytes(b"$TTL 3600\nads.example CNAME .\n").detected_format,
            SourceFormat::Rpz
        );
    }

    #[test]
    fn parses_hostfile_field_one_field_two_and_auto() {
        let parsed = parse_source_bytes(
            br#"
            ads-one.example 0.0.0.0
            127.0.0.1 ads-two.example alias-two.example
            plain.example
            "#,
        );

        assert!(parsed.blocks.contains("ads-one.example"));
        assert!(parsed.blocks.contains("ads-two.example"));
        assert!(parsed.blocks.contains("alias-two.example"));
        assert!(parsed.blocks.contains("plain.example"));
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
            not-a-policy.example CNAME passthru.
            a-record.example A 0.0.0.0
            "#,
        );

        assert!(parsed.blocks.contains("ads.example.com"));
        assert!(parsed.blocks.contains("tracker.example.net"));
        assert!(parsed.blocks.contains("example.org"));
        assert!(parsed.blocks.contains("wild.example.org"));
        assert!(!parsed.blocks.contains("not-a-policy.example"));
        assert!(!parsed.blocks.contains("a-record.example"));
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
