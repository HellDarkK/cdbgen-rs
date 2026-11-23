// filepath: examples/parse_bench.rs
//
// Synthetic micro-benchmark for the offline pipeline of cdbgen-rs.
// Usage: cargo run --release --example parse_bench

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::PathBuf,
    time::Instant,
};

use cdbgen_rs::{
    cdb_writer::write_cdb_atomic, config::OutputConfig, config::OutputKeyFormat,
    outputs::build_output_domains, parser::parse_source_bytes,
};

const HOSTS_LINES: usize = 1_500_000;
const ADBLOCK_LINES: usize = 600_000;
const UNBOUND_LINES: usize = 60_000;
const RPZ_LINES: usize = 40_000;
const COMMENT_RATIO: f64 = 0.04;

const HOST_TLDS: &[&str] = &[
    "com", "net", "org", "io", "co", "xyz", "site", "app", "dev", "me", "info", "biz", "us", "ru",
    "de", "cn", "tk", "ml", "ga", "cf", "pw", "top", "vip", "live", "stream",
];
const AD_TAGS: &[&str] = &[
    "ad",
    "ads",
    "adsrvr",
    "doubleclick",
    "googlesyndication",
    "adservice",
    "tracker",
    "metric",
    "telemetry",
    "banner",
    "pixel",
    "beacon",
    "affiliate",
    "sponsor",
    "promo",
    "pop",
    "popup",
    "redirect",
    "click",
    "impression",
    "stats",
    "analytics",
    "score",
    "cdn-ads",
    "static-ad",
    "tag-manager",
];
const AD_MIDS: &[&str] = &[
    "cdn", "static", "img", "images", "media", "assets", "edge", "origin", "api", "v1", "v2", "v3",
    "prod", "stage", "eu", "us", "ap", "sa", "node", "cluster", "lb",
];
const PATH_TAILS: &[&str] = &["/banner.js", "/pixel.png", "/track.gif", "/i.gif", ""];

fn main() {
    let mut rng = SplitMix64::new(0x00C0_FFEE_BEEF_C0DE);
    let tmp = std::env::temp_dir();

    println!("== cdbgen-rs offline pipeline benchmark ==");
    println!("hosts={HOSTS_LINES} adblock={ADBLOCK_LINES} unbound={UNBOUND_LINES} rpz={RPZ_LINES}");

    let mut bodies: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    bodies.insert("hosts".into(), build_hosts(&mut rng));
    bodies.insert("adblock".into(), build_adblock(&mut rng));
    bodies.insert("unbound".into(), build_unbound(&mut rng));
    bodies.insert("rpz".into(), build_rpz(&mut rng));
    let total_bytes: usize = bodies.values().map(|v| v.len()).sum();
    println!(
        "generated {:.2} MiB across {} sources",
        total_bytes as f64 / (1024.0 * 1024.0),
        bodies.len()
    );

    let t = Instant::now();
    let mut parsed = BTreeMap::new();
    let mut total_blocks = 0usize;
    let mut total_allows = 0usize;
    for (id, body) in &bodies {
        let ts = Instant::now();
        let parsed_source = parse_source_bytes(body);
        total_blocks += parsed_source.blocks.len();
        total_allows += parsed_source.allows.len();
        parsed.insert(id.clone(), parsed_source);
        eprintln!("[parse/{id:>7}] {:>9.3?}", ts.elapsed());
    }
    let parse_elapsed = t.elapsed();
    println!(
        "[parse]   {:>9.3?}  blocks={total_blocks} allows={total_allows}",
        parse_elapsed
    );

    let group_ids = vec![
        "hosts".into(),
        "adblock".into(),
        "unbound".into(),
        "rpz".into(),
    ];
    let outputs = vec![OutputConfig {
        group: "merged".into(),
        source_ids: group_ids.clone(),
        path: tmp.join("merged.cdb"),
        key_format: OutputKeyFormat::Wire,
    }];
    let unavailable = BTreeSet::new();
    let t = Instant::now();
    let plans = build_output_domains(&outputs, parsed, &unavailable);
    let plan_elapsed = t.elapsed();
    let merged_domains = match &plans[0] {
        cdbgen_rs::outputs::OutputPlan::Ready { domains, .. } => domains.len(),
        _ => 0,
    };
    println!("[plan]    {:>9.3?}  merged={merged_domains}", plan_elapsed);

    // Shared-group case: same source_ids written to two paths. Mirrors the
    // real config (dnsdist + cdn write of the same merged set). The fix
    // caches the merged BTreeSet so the second output reuses it instead of
    // re-appending from the (now-empty) sources.
    let shared_outputs = vec![
        OutputConfig {
            group: "shared".into(),
            source_ids: group_ids.clone(),
            path: tmp.join("shared-a.cdb"),
            key_format: OutputKeyFormat::Wire,
        },
        OutputConfig {
            group: "shared".into(),
            source_ids: group_ids.clone(),
            path: tmp.join("shared-b.cdb"),
            key_format: OutputKeyFormat::Wire,
        },
    ];
    let mut shared_parsed: BTreeMap<String, cdbgen_rs::parser::ParsedSource> = BTreeMap::new();
    for (id, body) in &bodies {
        shared_parsed.insert(id.clone(), parse_source_bytes(body));
    }
    let t = Instant::now();
    let shared_plans = build_output_domains(&shared_outputs, shared_parsed, &unavailable);
    let shared_plan_elapsed = t.elapsed();
    let a = match &shared_plans[0] {
        cdbgen_rs::outputs::OutputPlan::Ready { domains, .. } => domains.len(),
        _ => 0,
    };
    let b = match &shared_plans[1] {
        cdbgen_rs::outputs::OutputPlan::Ready { domains, .. } => domains.len(),
        _ => 0,
    };
    assert_eq!(a, b, "shared outputs must agree on domain count");
    println!(
        "[plan2]   {:>9.3?}  shared={a} (both outputs)",
        shared_plan_elapsed
    );

    let t = Instant::now();
    for plan in &plans {
        if let cdbgen_rs::outputs::OutputPlan::Ready {
            output, domains, ..
        } = plan
        {
            write_cdb_atomic(&output.path, domains, output.key_format).unwrap();
        }
    }
    let write_elapsed = t.elapsed();
    let final_path = match &plans[0] {
        cdbgen_rs::outputs::OutputPlan::Ready { output, .. } => output.path.clone(),
        _ => PathBuf::new(),
    };
    let size = fs::metadata(&final_path)
        .map(|m| m.len())
        .unwrap_or_default();
    println!(
        "[write]   {:>9.3?}  bytes={} ({:.2} MiB)",
        write_elapsed,
        size,
        size as f64 / (1024.0 * 1024.0)
    );

    let inputs: Vec<String> = (0..200_000)
        .map(|i| {
            format!(
                "{}-{i}.{}.example.{}",
                AD_TAGS[i % AD_TAGS.len()],
                AD_MIDS[i % AD_MIDS.len()],
                HOST_TLDS[i % HOST_TLDS.len()]
            )
        })
        .collect();
    let t = Instant::now();
    let mut accepted = 0usize;
    for s in &inputs {
        if cdbgen_rs::normalize::normalize_domain(s).is_some() {
            accepted += 1;
        }
    }
    let norm_elapsed = t.elapsed();
    println!(
        "[norm]    {:>9.3?}  accepted={accepted}/{}",
        norm_elapsed,
        inputs.len()
    );
    println!(
        "[total]   {:>9.3?}",
        parse_elapsed + plan_elapsed + write_elapsed
    );
}

struct Buf<'a>(&'a mut Vec<u8>);
impl Write for Buf<'_> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Buf<'_> {
    fn writeln(&mut self, s: &str) -> std::io::Result<()> {
        self.0.extend_from_slice(s.as_bytes());
        self.0.push(b'\n');
        Ok(())
    }
}

fn build_hosts(rng: &mut SplitMix64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HOSTS_LINES * 80);
    let mut out = Buf(&mut buf);
    out.writeln("# hosts blocklist (synthetic)").unwrap();
    for i in 0..HOSTS_LINES {
        if rng.chance(COMMENT_RATIO) {
            out.writeln("# comment line").unwrap();
            continue;
        }
        let mid = AD_MIDS[rng.usize(AD_MIDS.len())];
        let tag = AD_TAGS[rng.usize(AD_TAGS.len())];
        let tld = HOST_TLDS[rng.usize(HOST_TLDS.len())];
        match rng.usize(3) {
            0 => writeln!(out.0, "0.0.0.0 {tag}-{i}.{mid}.example.{tld}").unwrap(),
            1 => writeln!(out.0, "127.0.0.1 {tag}-{i}.{mid}.example.{tld} # inline").unwrap(),
            _ => writeln!(out.0, "{tag}-{i}.{mid}.example.{tld} 0.0.0.0").unwrap(),
        }
    }
    buf
}

fn build_adblock(rng: &mut SplitMix64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(ADBLOCK_LINES * 80);
    let mut out = Buf(&mut buf);
    out.writeln("! uBO-style blocklist (synthetic)").unwrap();
    for i in 0..ADBLOCK_LINES {
        if rng.chance(COMMENT_RATIO) {
            out.writeln("! comment").unwrap();
            continue;
        }
        let mid = AD_MIDS[rng.usize(AD_MIDS.len())];
        let tag = AD_TAGS[rng.usize(AD_TAGS.len())];
        let tld = HOST_TLDS[rng.usize(HOST_TLDS.len())];
        let tail = PATH_TAILS[rng.usize(PATH_TAILS.len())];
        match rng.usize(4) {
            0 => writeln!(out.0, "||{tag}-{i}.{mid}.example.{tld}^").unwrap(),
            1 => writeln!(out.0, "||{tag}-{i}.{mid}.example.{tld}{tail}").unwrap(),
            2 => writeln!(out.0, "@@||allow-{i}.{mid}.example.{tld}^").unwrap(),
            _ => writeln!(out.0, "0.0.0.0 {tag}-{i}.{mid}.example.{tld}").unwrap(),
        }
    }
    buf
}

fn build_unbound(rng: &mut SplitMix64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(UNBOUND_LINES * 100);
    let mut out = Buf(&mut buf);
    out.writeln("server:").unwrap();
    for i in 0..UNBOUND_LINES {
        let mid = AD_MIDS[rng.usize(AD_MIDS.len())];
        let tag = AD_TAGS[rng.usize(AD_TAGS.len())];
        let tld = HOST_TLDS[rng.usize(HOST_TLDS.len())];
        writeln!(
            out.0,
            "local-zone: \"{tag}-{i}.{mid}.example.{tld}.\" always_null"
        )
        .unwrap();
    }
    buf
}

fn build_rpz(rng: &mut SplitMix64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(RPZ_LINES * 80);
    let mut out = Buf(&mut buf);
    out.writeln("$TTL 3600").unwrap();
    out.writeln("@ SOA localhost. root.localhost. 1 14400 3600 86400 3600")
        .unwrap();
    for i in 0..RPZ_LINES {
        let mid = AD_MIDS[rng.usize(AD_MIDS.len())];
        let tag = AD_TAGS[rng.usize(AD_TAGS.len())];
        let tld = HOST_TLDS[rng.usize(HOST_TLDS.len())];
        writeln!(out.0, "{tag}-{i}.{mid}.example.{tld} CNAME .").unwrap();
    }
    buf
}

struct SplitMix64(u64);
impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn usize(&mut self, n: usize) -> usize {
        (self.next_u64() as usize) % n
    }
    fn chance(&mut self, p: f64) -> bool {
        ((self.next_u64() >> 11) as f64 / ((1u64 << 53) as f64)) < p
    }
}
