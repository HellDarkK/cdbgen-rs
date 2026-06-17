// filepath: examples/parser_vec_bench.rs
//
// Experimental parser benchmark: Vec<String> + sort_unstable + dedup
// vs. the production BTreeSet<String> insertion path. Side-by-side run.
//
// Usage: cargo run --release --example parser_vec_bench

use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    time::Instant,
};

const HOSTS_LINES: usize = 1_500_000;
const ADBLOCK_LINES: usize = 600_000;
const UNBOUND_LINES: usize = 60_000;
const RPZ_LINES: usize = 40_000;
const COMMENT_RATIO: f64 = 0.04;

const HOST_TLDS: &[&str] = &[
    "com", "net", "org", "io", "co", "xyz", "site", "app", "dev", "me", "info", "biz", "us",
    "ru", "de", "cn", "tk", "ml", "ga", "cf", "pw", "top", "vip", "live", "stream",
];
const AD_TAGS: &[&str] = &[
    "ad", "ads", "adsrvr", "doubleclick", "googlesyndication", "adservice", "tracker",
    "metric", "telemetry", "banner", "pixel", "beacon", "affiliate", "sponsor", "promo",
    "pop", "popup", "redirect", "click", "impression", "stats", "analytics", "score",
    "cdn-ads", "static-ad", "tag-manager",
];
const AD_MIDS: &[&str] = &[
    "cdn", "static", "img", "images", "media", "assets", "edge", "origin", "api", "v1", "v2",
    "v3", "prod", "stage", "eu", "us", "ap", "sa", "node", "cluster", "lb",
];
const PATH_TAILS: &[&str] = &["/banner.js", "/pixel.png", "/track.gif", "/i.gif", ""];

fn main() {
    let mut rng = SplitMix64::new(0xC0FFEE_BEEF_C0DE);

    println!("== parser vector vs btree benchmark ==");
    println!("hosts={HOSTS_LINES} adblock={ADBLOCK_LINES} unbound={UNBOUND_LINES} rpz={RPZ_LINES}");

    // Build bodies
    let bodies: Vec<(&str, Vec<u8>)> = vec![
        ("hosts", build_hosts(&mut rng)),
        ("adblock", build_adblock(&mut rng)),
        ("unbound", build_unbound(&mut rng)),
        ("rpz", build_rpz(&mut rng)),
    ];
    let total_bytes: usize = bodies.iter().map(|(_, b)| b.len()).sum();
    println!("input bytes: {:.2} MiB", total_bytes as f64 / (1024.0 * 1024.0));

    // ── Baseline: production parser (BTreeSet) ──
    println!("\n--- production: BTreeSet<String> insertion ---");
    let baseline_mem_before = resident_mib();
    let mut baseline_total = std::time::Duration::ZERO;
    let mut baseline_blocks_total = 0usize;
    for (id, body) in &bodies {
        let ts = Instant::now();
        let parsed = cdbgen_rs::parser::parse_source_bytes(body);
        let dt = ts.elapsed();
        baseline_total += dt;
        baseline_blocks_total += parsed.blocks.len();
        let ns = if parsed.blocks.len() > 0 {
            dt.as_nanos() / parsed.blocks.len() as u128
        } else {
            0
        };
        println!("  {:<8} {:>9.3?}  blocks={:<10} ns/block={}", id, dt, parsed.blocks.len(), ns);
    }
    let baseline_mem_after = resident_mib();
    println!("  TOTAL     {:>9.3?}  blocks={}  resident +{:.1} MiB (after this run)",
        baseline_total, baseline_blocks_total,
        (baseline_mem_after as f64) - (baseline_mem_before as f64));

    // ── Experimental: Vec<String> + sort_unstable + dedup ──
    println!("\n--- experimental: Vec<String> + sort_unstable + dedup ---");
    let exp_mem_before = resident_mib();
    let mut exp_total = std::time::Duration::ZERO;
    let mut exp_insert_total = std::time::Duration::ZERO;
    let mut exp_sort_total = std::time::Duration::ZERO;
    let mut exp_dedup_total = std::time::Duration::ZERO;
    let mut exp_blocks_total = 0usize;

    for (id, body) in &bodies {
        let ts_total = Instant::now();
        let parsed = parse_vec_experimental(body);
        let dt_total = ts_total.elapsed();
        exp_total += dt_total;
        exp_insert_total += parsed.t_insert;
        exp_sort_total += parsed.t_sort;
        exp_dedup_total += parsed.t_dedup;
        exp_blocks_total += parsed.blocks.len();
        let ns = if parsed.blocks.len() > 0 {
            dt_total.as_nanos() / parsed.blocks.len() as u128
        } else {
            0
        };
        println!(
            "  {:<8} total={:>9.3?}  insert={:>9.3?}  sort={:>9.3?}  dedup={:>9.3?}  blocks={:<10} ns/block={}",
            id, dt_total, parsed.t_insert, parsed.t_sort, parsed.t_dedup, parsed.blocks.len(), ns
        );
    }
    let exp_mem_after = resident_mib();
    println!("  TOTAL     {:>9.3?}  blocks={}  resident +{:.1} MiB (after this run)",
        exp_total, exp_blocks_total,
        (exp_mem_after as f64) - (exp_mem_before as f64));

    // ── Correctness check (hosts + rpz + unbound only; adblock production
    //    has extra drop-rules that this experimental parser doesn't port) ──
    println!("\n--- correctness (sorted output equal?) ---");
    for (id, body) in &bodies {
        if *id == "adblock" {
            println!("  {:<8} [skipped: production has format-specific drop rules]", id);
            continue;
        }
        let baseline = cdbgen_rs::parser::parse_source_bytes(body);
        let experimental = parse_vec_experimental(body);
        let a: BTreeSet<&str> = baseline.blocks.iter().map(String::as_str).collect();
        let b: BTreeSet<&str> = experimental.blocks.iter().map(String::as_str).collect();
        let same = a == b;
        println!("  {:<8} baseline={:<10} experimental={:<10} equal={}",
            id, baseline.blocks.len(), experimental.blocks.len(), same);
        assert!(same, "mismatch for {id}");
    }

    // ── End-to-end pipeline comparison ──
    // Production: parse → BTreeSet → plan → write
    // Experimental: parse → Vec → sort+dedup → BTreeSet → plan → write
    // The conversion Vec→BTreeSet is a real cost that the new path adds.
    println!("\n--- end-to-end pipeline ---");
    let tmp = std::env::temp_dir();

    // Production pipeline.
    let t = Instant::now();
    let mut parsed_prod: BTreeMap<String, cdbgen_rs::parser::ParsedSource> = BTreeMap::new();
    for (id, body) in &bodies {
        parsed_prod.insert((*id).to_string(), cdbgen_rs::parser::parse_source_bytes(body));
    }
    let parse_prod_dt = t.elapsed();
    let outputs = vec![cdbgen_rs::config::OutputConfig {
        group: "merged".into(),
        source_ids: vec!["hosts".into(), "adblock".into(), "unbound".into(), "rpz".into()],
        path: tmp.join("prod.cdb"),
        key_format: cdbgen_rs::config::OutputKeyFormat::Wire,
    }];
    let t = Instant::now();
    let plans = cdbgen_rs::outputs::build_output_domains(
        &outputs,
        parsed_prod,
        &BTreeSet::new(),
    );
    let plan_prod_dt = t.elapsed();
    let t = Instant::now();
    for plan in &plans {
        if let cdbgen_rs::outputs::OutputPlan::Ready { output, domains, .. } = plan {
            cdbgen_rs::cdb_writer::write_cdb_atomic(&output.path, domains, output.key_format).unwrap();
        }
    }
    let write_prod_dt = t.elapsed();
    let total_prod = parse_prod_dt + plan_prod_dt + write_prod_dt;
    println!("  production   parse={:>9.3?}  plan={:>9.3?}  write={:>9.3?}  TOTAL={:?}",
        parse_prod_dt, plan_prod_dt, write_prod_dt, total_prod);

    // Experimental pipeline.
    let t = Instant::now();
    let mut parsed_exp: BTreeMap<String, cdbgen_rs::parser::ParsedSource> = BTreeMap::new();
    for (id, body) in &bodies {
        let exp = parse_vec_experimental(body);
        // Convert to ParsedSource (BTreeSet). The "BTreeSet::from_iter" cost
        // is the integration tax of mixing the new parser with the existing
        // output pipeline. Dedup is already done in the Vec so from_iter is
        // n unique inserts.
        let mut blocks = Vec::new();
        for s in exp.blocks { blocks.push(s); }
        parsed_exp.insert(
            (*id).to_string(),
            cdbgen_rs::parser::ParsedSource {
                detected_format: cdbgen_rs::parser::SourceFormat::Hostfile,
                blocks,
                allows: Vec::new(),
                stats: Default::default(),
            },
        );
    }
    let parse_exp_dt = t.elapsed();
    let outputs2 = vec![cdbgen_rs::config::OutputConfig {
        group: "merged".into(),
        source_ids: vec!["hosts".into(), "adblock".into(), "unbound".into(), "rpz".into()],
        path: tmp.join("exp.cdb"),
        key_format: cdbgen_rs::config::OutputKeyFormat::Wire,
    }];
    let t = Instant::now();
    let plans2 = cdbgen_rs::outputs::build_output_domains(
        &outputs2,
        parsed_exp,
        &BTreeSet::new(),
    );
    let plan_exp_dt = t.elapsed();
    let t = Instant::now();
    for plan in &plans2 {
        if let cdbgen_rs::outputs::OutputPlan::Ready { output, domains, .. } = plan {
            cdbgen_rs::cdb_writer::write_cdb_atomic(&output.path, domains, output.key_format).unwrap();
        }
    }
    let write_exp_dt = t.elapsed();
    let total_exp = parse_exp_dt + plan_exp_dt + write_exp_dt;
    println!("  experimental parse={:>9.3?}  plan={:>9.3?}  write={:>9.3?}  TOTAL={:?}",
        parse_exp_dt, plan_exp_dt, write_exp_dt, total_exp);

    let saved = total_prod.saturating_sub(total_exp);
    let speedup = total_prod.as_secs_f64() / total_exp.as_secs_f64();
    println!("  pipeline speedup = {:.2}x  saved = {} ms", speedup, saved.as_millis());

    // ── Summary ──
    let speedup = baseline_total.as_secs_f64() / exp_total.as_secs_f64();
    let saved_ms = baseline_total.saturating_sub(exp_total).as_millis();
    println!("\n=== summary ===");
    println!("baseline   parse_total = {:>9.3?}", baseline_total);
    println!("experimental parse_total = {:>9.3?}", exp_total);
    println!("speedup = {:.2}x  saved = {} ms", speedup, saved_ms);
    println!("baseline   ns/block = {}", baseline_total.as_nanos() / baseline_blocks_total as u128);
    println!("experimental ns/block = {}", exp_total.as_nanos() / exp_blocks_total as u128);
}

// ─── experimental parser ───

struct ExperimentalParsed {
    blocks: Vec<String>,
    t_insert: std::time::Duration,
    t_sort: std::time::Duration,
    t_dedup: std::time::Duration,
}

/// Minimal fast-path normalizer. Mirrors the production fast-path
/// `is_already_ascii_normalized` semantics (valid ASCII LDH lowercase).
/// Skips idna entirely since our generator only emits LDH inputs.
#[inline]
fn fast_normalize(s: &str) -> Option<&str> {
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
            _ => return None,
        }
    }
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

/// Strip an `0.0.0.0 ` / `127.0.0.1 ` prefix or take the whole trimmed line
/// as the domain. Mirrors the production hosts extractor well enough for the
/// benchmark to be apples-to-apples.
#[inline]
fn extract_host_domain(line: &str) -> Option<&str> {
    let s = line.trim();
    // unbound: `local-zone: "example.com" static`
    if let Some(rest) = s.strip_prefix("local-zone:") {
        let rest = rest.trim();
        if let Some(quote_start) = rest.find('"') {
            let after = &rest[quote_start + 1..];
            if let Some(quote_end) = after.find('"') {
                return Some(&after[..quote_end]);
            }
        }
        return None;
    }
    // hosts-format: "0.0.0.0 example.com" or "IP example.com"
    if let Some(pos) = s.find(char::is_whitespace) {
        let (ip, rest) = s.split_at(pos);
        let rest = rest.trim();
        if ip.parse::<std::net::IpAddr>().is_ok() {
            return Some(rest);
        }
    }
    // adblock: "||example.com^" or "||example.com^$third-party" or "||example.com/path^"
    if let Some(rest) = s.strip_prefix("||") {
        // Cut at first ^ / $ / / (path) — same rule production uses.
        let end = rest.find(|c: char| c == '^' || c == '$' || c == '/').unwrap_or(rest.len());
        return Some(&rest[..end]);
    }
    // rpz: "example.com CNAME ."
    if let Some(pos) = s.find(char::is_whitespace) {
        return Some(&s[..pos]);
    }
    Some(s)
}

fn parse_vec_experimental(body: &[u8]) -> ExperimentalParsed {
    let text = std::str::from_utf8(body).expect("utf8 in benchmark");
    let mut blocks: Vec<String> = Vec::new();

    let t_insert = Instant::now();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        if let Some(d) = extract_host_domain(line) {
            if let Some(norm) = fast_normalize(d) {
                blocks.push(norm.to_string());
            }
        }
    }
    let t_insert = t_insert.elapsed();

    let t_sort = Instant::now();
    blocks.sort_unstable();
    let t_sort = t_sort.elapsed();

    let t_dedup = Instant::now();
    blocks.dedup();
    let t_dedup = t_dedup.elapsed();

    ExperimentalParsed { blocks, t_insert, t_sort, t_dedup }
}

// ─── generators (copy of parse_bench) ───

struct SplitMix64 { state: u64 }
impl SplitMix64 {
    fn new(seed: u64) -> Self { Self { state: seed } }
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T { &items[(self.next_u64() as usize) % items.len()] }
    fn chance(&mut self, p: f64) -> bool {
        (self.next_u64() as f64 / u64::MAX as f64) < p
    }
}

struct Buf<'a>(&'a mut Vec<u8>);
impl<'a> Write for Buf<'a> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> { self.0.extend_from_slice(buf); Ok(buf.len()) }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

fn build_hosts(rng: &mut SplitMix64) -> Vec<u8> {
    let mut out = Vec::with_capacity(HOSTS_LINES * 32);
    let mut w = Buf(&mut out);
    for i in 0..HOSTS_LINES {
        if rng.chance(COMMENT_RATIO) {
            writeln!(w, "# comment {i}").unwrap();
        } else {
            let tld = rng.pick(HOST_TLDS);
            let tag = rng.pick(AD_TAGS);
            let mid = rng.pick(AD_MIDS);
            writeln!(w, "0.0.0.0 {tag}{i}.{mid}.example.{tld}").unwrap();
        }
    }
    out
}
fn build_adblock(rng: &mut SplitMix64) -> Vec<u8> {
    let mut out = Vec::with_capacity(ADBLOCK_LINES * 32);
    let mut w = Buf(&mut out);
    for i in 0..ADBLOCK_LINES {
        if rng.chance(COMMENT_RATIO) {
            writeln!(w, "! comment {i}").unwrap();
        } else {
            let tld = rng.pick(HOST_TLDS);
            let tag = rng.pick(AD_TAGS);
            let mid = rng.pick(AD_MIDS);
            let tail = rng.pick(PATH_TAILS);
            writeln!(w, "||{tag}{i}.{mid}.example.{tld}{tail}^").unwrap();
        }
    }
    out
}
fn build_unbound(rng: &mut SplitMix64) -> Vec<u8> {
    let mut out = Vec::with_capacity(UNBOUND_LINES * 32);
    let mut w = Buf(&mut out);
    for i in 0..UNBOUND_LINES {
        if rng.chance(COMMENT_RATIO) {
            writeln!(w, "# comment {i}").unwrap();
        } else {
            let tld = rng.pick(HOST_TLDS);
            let tag = rng.pick(AD_TAGS);
            writeln!(w, "local-zone: \"{tag}{i}.example.{tld}\" static").unwrap();
        }
    }
    out
}
fn build_rpz(rng: &mut SplitMix64) -> Vec<u8> {
    let mut out = Vec::with_capacity(RPZ_LINES * 32);
    let mut w = Buf(&mut out);
    for i in 0..RPZ_LINES {
        if rng.chance(COMMENT_RATIO) {
            writeln!(w, "# comment {i}").unwrap();
        } else {
            let tld = rng.pick(HOST_TLDS);
            let tag = rng.pick(AD_TAGS);
            writeln!(w, "{tag}{i}.example.{tld} CNAME .").unwrap();
        }
    }
    out
}

// ─── memory probe (Linux VmRSS) ───
fn resident_mib() -> f64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: u64 = rest.split_whitespace().next().and_then(|x| x.parse().ok()).unwrap_or(0);
            return kb as f64 / 1024.0;
        }
    }
    0.0
}
