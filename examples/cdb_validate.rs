// filepath: examples/cdb_validate.rs
//
// End-to-end CDB validity check: builds synthetic sources, runs the same
// pipeline the binary uses, then re-opens the produced CDB and confirms
// every emitted key round-trips with the expected value.
//
// Usage: cargo run --release --example cdb_validate

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
};

use cdbgen_rs::{
    cdb_writer::domain_to_wire,
    config::{OutputConfig, OutputKeyFormat},
    outputs::{OutputPlan, build_output_domains, write_outputs},
    parser::parse_source_bytes,
};

const HOSTS_LINES: usize = 200_000;
const ADBLOCK_LINES: usize = 80_000;
const UNBOUND_LINES: usize = 10_000;
const RPZ_LINES: usize = 5_000;
const COMMENT_RATIO: f64 = 0.04;

const HOST_TLDS: &[&str] = &[
    "com", "net", "org", "io", "co", "xyz", "site", "app", "dev",
];
const AD_TAGS: &[&str] = &[
    "ad", "ads", "adsrvr", "doubleclick", "tracker", "metric", "telemetry",
    "banner", "pixel", "beacon", "stats", "analytics",
];
const AD_MIDS: &[&str] = &["cdn", "static", "img", "v1", "v2", "eu", "us"];
const PATH_TAILS: &[&str] = &["/banner.js", "/pixel.png", "/track.gif", ""];

fn main() {
    let mut rng = SplitMix64::new(0xC0FFEE_BEEF_C0DE);
    let tmp = std::env::temp_dir();

    println!("== cdbgen-rs CDB validity check ==");

    let bodies: BTreeMap<String, Vec<u8>> = [
        ("hosts", build_hosts(&mut rng)),
        ("adblock", build_adblock(&mut rng)),
        ("unbound", build_unbound(&mut rng)),
        ("rpz", build_rpz(&mut rng)),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();

    let mut parsed = BTreeMap::new();
    for (id, body) in &bodies {
        let p = parse_source_bytes(body);
        println!(
            "  source {:<8} blocks={:<8} allows={:<6} duplicates(block/allow)={}/{}",
            id,
            p.blocks.len(),
            p.allows.len(),
            p.stats.duplicate_blocks,
            p.stats.duplicate_allows,
        );
        parsed.insert(id.clone(), p);
    }

    let group_ids = vec![
        "hosts".to_string(),
        "adblock".to_string(),
        "unbound".to_string(),
        "rpz".to_string(),
    ];

    // Write both Wire and Plaintext outputs so we exercise both code paths.
    let wire_path = tmp.join("validate-wire.cdb");
    let plaintext_path = tmp.join("validate-plaintext.cdb");
    let outputs = vec![
        OutputConfig {
            group: "wire".into(),
            source_ids: group_ids.clone(),
            path: wire_path.clone(),
            key_format: OutputKeyFormat::Wire,
        },
        OutputConfig {
            group: "plaintext".into(),
            source_ids: group_ids.clone(),
            path: plaintext_path.clone(),
            key_format: OutputKeyFormat::Plaintext,
        },
    ];
    let unavailable = std::collections::BTreeSet::new();
    let plans = build_output_domains(&outputs, parsed.clone(), &unavailable);
    let summary = write_outputs(&plans).expect("write outputs");

    assert_eq!(summary.written_outputs, 2, "both outputs should write");
    assert_eq!(summary.empty_outputs, 0, "no output should be empty");

    // Build the expected domain set (sorted, deduped, with allows subtracted)
    // directly so we can compare against the on-disk CDB without trusting
    // the planner's output ordering.
    let expected: Vec<String> = {
        let mut blocks: Vec<String> = Vec::new();
        let mut allows: Vec<String> = Vec::new();
        for source_id in &group_ids {
            if let Some(src) = parsed.get(source_id) {
                blocks.extend(src.blocks.iter().cloned());
                allows.extend(src.allows.iter().cloned());
            }
        }
        blocks.sort_unstable();
        blocks.dedup();
        allows.sort_unstable();
        allows.dedup();
        blocks.retain(|d| !allows.binary_search(d).is_ok());
        blocks
    };
    assert!(!expected.is_empty(), "expected non-empty domain set");
    println!("\n  expected distinct domains: {}", expected.len());

    // ── Wire-format CDB ──
    let wire_size = fs::metadata(&wire_path).unwrap().len();
    println!("\n[wire]   {} bytes  path={}", wire_size, wire_path.display());
    let wire_db = cdb::CDB::open(&wire_path).expect("open wire CDB");
    let mut wire_hits = 0usize;
    let mut wire_misses: Vec<&str> = Vec::new();
    for d in &expected {
        let key = domain_to_wire(d).expect("encode wire key");
        match wire_db.get(&key) {
            Some(Ok(v)) if v.is_empty() => wire_hits += 1,
            _ => wire_misses.push(d.as_str()),
        }
    }
    println!("[wire]   hits={} misses={}", wire_hits, wire_misses.len());
    assert_eq!(wire_misses.len(), 0, "wire: missed keys: {:?}", &wire_misses[..wire_misses.len().min(5)]);
    assert_eq!(wire_hits, expected.len(), "wire: every expected domain must be present");

    // Negative check: a domain we did not generate must not be present.
    let absent_key = domain_to_wire("definitely-not-in-the-test.example").unwrap();
    assert!(wire_db.get(&absent_key).is_none(), "wire: spurious lookup returned a value");
    println!("[wire]   negative lookup confirmed absent");

    // ── Plaintext-format CDB ──
    let plain_size = fs::metadata(&plaintext_path).unwrap().len();
    println!("\n[plain]  {} bytes  path={}", plain_size, plaintext_path.display());
    let plain_db = cdb::CDB::open(&plaintext_path).expect("open plaintext CDB");
    let mut plain_hits = 0usize;
    let mut plain_misses: Vec<&str> = Vec::new();
    for d in &expected {
        match plain_db.get(d.as_bytes()) {
            Some(Ok(v)) if v.is_empty() => plain_hits += 1,
            _ => plain_misses.push(d.as_str()),
        }
    }
    println!("[plain]  hits={} misses={}", plain_hits, plain_misses.len());
    assert_eq!(plain_misses.len(), 0, "plaintext: missed keys: {:?}", &plain_misses[..plain_misses.len().min(5)]);
    assert_eq!(plain_hits, expected.len(), "plaintext: every expected domain must be present");

    // Negative check.
    assert!(
        plain_db.get(b"definitely-not-in-the-test.example").is_none(),
        "plaintext: spurious lookup returned a value"
    );
    println!("[plain]  negative lookup confirmed absent");

    // ── Output planner sanity: Ready plan should report the right counts ──
    let ready = plans
        .iter()
        .find_map(|p| match p {
            OutputPlan::Ready { domains, blocks_before_allow, allows, .. } => {
                Some((domains.len(), *blocks_before_allow, *allows))
            }
            _ => None,
        })
        .expect("at least one ready plan");
    println!(
        "\n[plan]   ready domains={} blocks_before_allow={} allows={}",
        ready.0, ready.1, ready.2
    );
    assert_eq!(ready.0, expected.len());

    println!("\n[OK] all CDB round-trip checks passed");
}

// ─── generators ───

struct Buf<'a>(&'a mut Vec<u8>);
impl Write for Buf<'_> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> { self.0.extend_from_slice(b); Ok(b.len()) }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

fn build_hosts(rng: &mut SplitMix64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HOSTS_LINES * 32);
    let mut w = Buf(&mut buf);
    writeln!(w, "# hosts blocklist (synthetic)").unwrap();
    for i in 0..HOSTS_LINES {
        if rng.chance(COMMENT_RATIO) { writeln!(w, "# comment {i}").unwrap(); continue; }
        let tld = rng.pick(HOST_TLDS);
        let tag = rng.pick(AD_TAGS);
        let mid = rng.pick(AD_MIDS);
        match rng.usize(3) {
            0 => writeln!(w, "0.0.0.0 {tag}-{i}.{mid}.example.{tld}").unwrap(),
            1 => writeln!(w, "127.0.0.1 {tag}-{i}.{mid}.example.{tld} # inline").unwrap(),
            _ => writeln!(w, "{tag}-{i}.{mid}.example.{tld} 0.0.0.0").unwrap(),
        }
    }
    buf
}
fn build_adblock(rng: &mut SplitMix64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(ADBLOCK_LINES * 32);
    let mut w = Buf(&mut buf);
    writeln!(w, "! uBO-style blocklist (synthetic)").unwrap();
    for i in 0..ADBLOCK_LINES {
        if rng.chance(COMMENT_RATIO) { writeln!(w, "! comment {i}").unwrap(); continue; }
        let tld = rng.pick(HOST_TLDS);
        let tag = rng.pick(AD_TAGS);
        let mid = rng.pick(AD_MIDS);
        let tail = rng.pick(PATH_TAILS);
        match rng.usize(4) {
            0 => writeln!(w, "||{tag}-{i}.{mid}.example.{tld}^").unwrap(),
            1 => writeln!(w, "||{tag}-{i}.{mid}.example.{tld}{tail}").unwrap(),
            2 => writeln!(w, "@@||allow-{i}.{mid}.example.{tld}^").unwrap(),
            _ => writeln!(w, "0.0.0.0 {tag}-{i}.{mid}.example.{tld}").unwrap(),
        }
    }
    buf
}
fn build_unbound(rng: &mut SplitMix64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(UNBOUND_LINES * 64);
    let mut w = Buf(&mut buf);
    writeln!(w, "server:").unwrap();
    for i in 0..UNBOUND_LINES {
        let tld = rng.pick(HOST_TLDS);
        let tag = rng.pick(AD_TAGS);
        let mid = rng.pick(AD_MIDS);
        writeln!(w, "local-zone: \"{tag}-{i}.{mid}.example.{tld}.\" always_null").unwrap();
    }
    buf
}
fn build_rpz(rng: &mut SplitMix64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(RPZ_LINES * 32);
    let mut w = Buf(&mut buf);
    writeln!(w, "$TTL 3600").unwrap();
    writeln!(w, "@ SOA localhost. root.localhost. 1 14400 3600 86400 3600").unwrap();
    for i in 0..RPZ_LINES {
        let tld = rng.pick(HOST_TLDS);
        let tag = rng.pick(AD_TAGS);
        let mid = rng.pick(AD_MIDS);
        writeln!(w, "{tag}-{i}.{mid}.example.{tld} CNAME .").unwrap();
    }
    buf
}

struct SplitMix64(u64);
impl SplitMix64 {
    fn new(seed: u64) -> Self { Self(seed) }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn usize(&mut self, n: usize) -> usize { (self.next_u64() as usize) % n }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T { &items[(self.next_u64() as usize) % items.len()] }
    fn chance(&mut self, p: f64) -> bool { ((self.next_u64() >> 11) as f64 / ((1u64 << 53) as f64)) < p }
}
