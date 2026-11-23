use std::{collections::BTreeMap, net::IpAddr, str::FromStr};

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

/// Parsed source data.
///
/// `blocks` and `allows` are kept as `Vec<String>` (sorted, deduplicated)
/// rather than `BTreeSet<String>`. On the hot path we push every accepted
/// domain into the vec and run a single `sort_unstable` + `dedup` at the
/// end. BTreeSet insertion was the dominant cost (≈1.65M small heap allocs
/// per source); vec-push is amortized-O(1) and dedup is one O(n) pass.
#[derive(Debug, Clone)]
pub struct ParsedSource {
    pub detected_format: SourceFormat,
    pub blocks: Vec<String>,
    pub allows: Vec<String>,
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
        blocks: Vec::new(),
        allows: Vec::new(),
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
                RuleKind::Block => parsed.blocks.push(domain.domain),
                RuleKind::Allow => parsed.allows.push(domain.domain),
            }
        }

        parsed.stats.accepted_blocks += parsed.blocks.len() - before_blocks;
        parsed.stats.accepted_allows += parsed.allows.len() - before_allows;
    }

    finalize_sorted_set(&mut parsed.blocks, &mut parsed.stats.duplicate_blocks);
    finalize_sorted_set(&mut parsed.allows, &mut parsed.stats.duplicate_allows);

    parsed
}

/// Sort `vec` in place and collapse consecutive duplicates. `dup_count` is
/// incremented by the number of duplicates that were dropped.
#[inline]
fn finalize_sorted_set(vec: &mut Vec<String>, dup_count: &mut usize) {
    if vec.len() < 2 {
        return;
    }
    vec.sort_unstable();
    let mut write = 1usize;
    for read in 1..vec.len() {
        if vec[read] == vec[write - 1] {
            *dup_count += 1;
        } else {
            vec[write] = std::mem::take(&mut vec[read]);
            write += 1;
        }
    }
    vec.truncate(write);
}

pub fn detect_source_format(text: &str) -> SourceFormat {
    #[derive(Default)]
    struct Scores {
        hostfile: usize,
        adblock: usize,
        unbound: usize,
        rpz: usize,
    }

    // Detect on a small sample; format is usually obvious within the first
    // handful of non-blank, non-comment lines. Capping the sample avoids
    // running 4 format extractors on every line of a multi-million-line
    // source, which dominated parse time.
    const SAMPLE_LIMIT: usize = 200;
    const DECISIVE_SCORE: usize = 40;
    let mut scores = Scores::default();
    let mut seen = 0usize;
    for (index, raw_line) in text.lines().enumerate() {
        let mut line = raw_line.trim();
        if index == 0 {
            line = line.trim_start_matches('\u{feff}');
        }
        if line.is_empty() || is_comment(line) {
            continue;
        }
        seen += 1;
        if seen > SAMPLE_LIMIT {
            break;
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

        if scores.unbound >= DECISIVE_SCORE
            || scores.rpz >= DECISIVE_SCORE
            || scores.adblock >= DECISIVE_SCORE
            || scores.hostfile >= DECISIVE_SCORE
        {
            break;
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

    // Parse tokens once and dispatch to the format-specific extractor.
    let tokens = tokens_before_inline_comment(line);
    if tokens.is_empty() {
        return Vec::new();
    }

    if let Some(domain) = extract_rpz_line_with(&tokens) {
        return vec![ExtractedDomain {
            kind: RuleKind::Block,
            domain,
        }];
    }

    if let Some(domains) = extract_hosts_line_with(&tokens) {
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

fn extract_rpz_line_with(tokens: &Tokens<'_>) -> Option<String> {
    let (record_index, _) = tokens
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, token)| token.eq_ignore_ascii_case("CNAME"))?;

    if tokens.get(record_index + 1) != Some(".") {
        return None;
    }

    let owner = tokens.get(0)?.trim_end_matches('.');
    let owner = owner.strip_prefix("*.").unwrap_or(owner);
    if owner == "@" || owner.starts_with('$') {
        return None;
    }

    normalize_domain(owner)
}

#[allow(dead_code)]
fn extract_rpz_line(line: &str) -> Option<String> {
    extract_rpz_line_with(&tokens_before_inline_comment(line))
}

fn looks_like_rpz_preamble(line: &str) -> bool {
    let tokens = tokens_before_inline_comment(line);
    if tokens.is_empty() {
        return false;
    }

    tokens.get(0).is_some_and(|t| t.starts_with('$'))
        || tokens
            .iter()
            .skip(1)
            .any(|token| is_dns_class(token) || is_dns_record_type(token))
}

fn extract_hosts_line_with(tokens: &Tokens<'_>) -> Option<Vec<String>> {
    if tokens.is_empty() || looks_like_dns_record_line(tokens) {
        return None;
    }

    let first = tokens.get(0)?;
    let domains = if IpAddr::from_str(first).is_ok() {
        tokens.iter().skip(1).filter_map(normalize_domain).collect()
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

#[allow(dead_code)]
fn extract_hosts_line(line: &str) -> Option<Vec<String>> {
    extract_hosts_line_with(&tokens_before_inline_comment(line))
}

/// Lazily tokenized slice of whitespace-separated tokens from `line` (without
/// inline comments). Uses an inline fixed-size array for the common case to
/// avoid heap allocation per parsed line.
#[derive(Debug)]
struct Tokens<'a> {
    src: &'a str,
    // (start, end) byte offsets into `src`. Up to 6 tokens inline.
    inline: [(usize, usize); 6],
    inline_len: u8,
    overflow: Option<Vec<(usize, usize)>>,
}

impl<'a> Tokens<'a> {
    fn parse(line: &'a str) -> Self {
        let mut inline: [(usize, usize); 6] = [(0, 0); 6];
        let mut inline_len: u8 = 0;
        let mut overflow: Option<Vec<(usize, usize)>> = None;

        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            // Skip leading whitespace.
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i >= bytes.len() {
                break;
            }
            let start = i;
            // Find end of token.
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let end = i;
            let tok = &line[start..end];
            if is_inline_comment_token(tok) {
                break;
            }
            if (inline_len as usize) < inline.len() {
                inline[inline_len as usize] = (start, end);
                inline_len += 1;
            } else {
                overflow
                    .get_or_insert_with(|| Vec::with_capacity(8))
                    .push((start, end));
            }
        }

        Self {
            src: line,
            inline,
            inline_len,
            overflow,
        }
    }

    fn is_empty(&self) -> bool {
        self.inline_len == 0 && self.overflow.as_ref().is_none_or(|v| v.is_empty())
    }

    #[allow(dead_code)]
    fn len(&self) -> usize {
        self.inline_len as usize + self.overflow.as_ref().map_or(0, |v| v.len())
    }

    fn get(&self, i: usize) -> Option<&'a str> {
        if i < self.inline_len as usize {
            let (s, e) = self.inline[i];
            return Some(&self.src[s..e]);
        }
        let ov = self.overflow.as_ref()?;
        ov.get(i - self.inline_len as usize)
            .map(|(s, e)| &self.src[*s..*e])
    }

    fn iter(&self) -> impl Iterator<Item = &'a str> + '_ {
        let n = self.inline_len as usize;
        (0..n)
            .map(move |i| {
                let (s, e) = self.inline[i];
                &self.src[s..e]
            })
            .chain(
                self.overflow
                    .iter()
                    .flatten()
                    .map(move |(s, e)| &self.src[*s..*e]),
            )
    }
}

fn tokens_before_inline_comment(line: &str) -> Tokens<'_> {
    Tokens::parse(line)
}

fn looks_like_dns_record_line(tokens: &Tokens<'_>) -> bool {
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

        let has = |v: &Vec<String>, s: &str| v.iter().any(|x| x == s);
        assert!(has(&parsed.blocks, "example.com"));
        assert!(has(&parsed.blocks, "ads.example.com"));
        assert!(has(&parsed.blocks, "alias.example.net"));
        assert!(has(&parsed.blocks, "tracker.example.net"));
        assert!(has(&parsed.blocks, "agh.example.org"));
        assert!(has(&parsed.allows, "allow.example.org"));
        assert!(!has(&parsed.blocks, "path.example"));
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

        let has = |s: &str| parsed.blocks.iter().any(|x| x == s);
        assert!(has("ads-one.example"));
        assert!(has("ads-two.example"));
        assert!(has("alias-two.example"));
        assert!(has("plain.example"));
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

        let has_block = |s: &str| parsed.blocks.iter().any(|x| x == s);
        assert!(has_block("ads.example.com"));
        assert!(has_block("tracker.example.net"));
        assert!(has_block("example.org"));
        assert!(has_block("wild.example.org"));
        assert!(!has_block("not-a-policy.example"));
        assert!(!has_block("a-record.example"));
        assert!(!has_block("localhost"));
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
