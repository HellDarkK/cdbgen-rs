// filepath: examples/profile_bench.rs
//
// Manual instrumentation profiler. Splits the offline pipeline into named
// sub-phases and prints per-phase wall time + a per-domain cost. No external
// profiler required (perf/flamegraph unavailable in this env).
//
// Usage: cargo run --release --example profile_bench

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    time::Instant,
};

use cdbgen_rs::normalize::normalize_domain;
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

#[derive(Default)]
struct Phase {
    name: &'static str,
    total: std::time::Duration,
    count: usize,
}

impl Phase {
    fn add(&mut self, t: std::time::Duration, count: usize) {
        self.total += t;
        self.count += count;
    }
    fn report(&self) {
        let per = if self.count > 0 {
            self.total / self.count as u32
        } else {
            std::time::Duration::ZERO
        };
        println!(
            "  {:<28} {:>10.3?}  count={:<10} per={:>10.3?}",
            self.name, self.total, self.count, per
        );
    }
}

fn main() {
    let mut rng = SplitMix64::new(0x00C0_FFEE_BEEF_C0DE);
    let tmp = std::env::temp_dir();

    println!("== cdbgen-rs profile ==");
    println!("hosts={HOSTS_LINES} adblock={ADBLOCK_LINES} unbound={UNBOUND_LINES} rpz={RPZ_LINES}");

    // ── generation ──
    let t = Instant::now();
    let mut bodies: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    bodies.insert("hosts".into(), build_hosts(&mut rng));
    bodies.insert("adblock".into(), build_adblock(&mut rng));
    bodies.insert("unbound".into(), build_unbound(&mut rng));
    bodies.insert("rpz".into(), build_rpz(&mut rng));
    let total_bytes: usize = bodies.values().map(|v| v.len()).sum();
    let gen_elapsed = t.elapsed();
    println!(
        "\n[gen]    {:>9.3?}  bytes={:.2} MiB",
        gen_elapsed,
        total_bytes as f64 / (1024.0 * 1024.0)
    );

    // ── parse ──
    let mut p_parse = Phase {
        name: "parse_source_bytes",
        ..Default::default()
    };
    let mut per_source: std::collections::BTreeMap<&str, (std::time::Duration, usize, usize)> =
        std::collections::BTreeMap::new();
    let mut parsed: BTreeMap<String, cdbgen_rs::parser::ParsedSource> = BTreeMap::new();
    let mut total_blocks = 0usize;
    let mut total_allows = 0usize;
    for (id, body) in &bodies {
        let ts = Instant::now();
        let p = parse_source_bytes(body);
        let dt = ts.elapsed();
        total_blocks += p.blocks.len();
        total_allows += p.allows.len();
        p_parse.add(dt, 1);
        per_source.insert(id.as_str(), (dt, p.blocks.len(), p.allows.len()));
        parsed.insert(id.clone(), p);
    }
    println!("\n[parse]   total blocks={total_blocks} allows={total_allows}");
    p_parse.report();
    let per_block_ns = p_parse.total.as_nanos() / total_blocks as u128;
    println!("  {:<28} {:>10}  ns/block", " ", per_block_ns);
    println!("  per-source breakdown:");
    for (id, (dt, blocks, allows)) in &per_source {
        println!(
            "    {:<8} {:>9.3?}  blocks={:<10} allows={}",
            id, dt, blocks, allows
        );
    }

    // ── plan (1 output) ──
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
    let mut p_plan1 = Phase {
        name: "build_output_domains x1",
        ..Default::default()
    };
    let mut p_plan1_clone = Phase {
        name: "  (incl BTreeSet clone)",
        ..Default::default()
    };

    // Build a fresh copy of parsed so we can measure plan twice.
    let mut p1_parsed: BTreeMap<String, cdbgen_rs::parser::ParsedSource> = BTreeMap::new();
    for (id, body) in &bodies {
        p1_parsed.insert(id.clone(), parse_source_bytes(body));
    }
    let ts = Instant::now();
    let plans = build_output_domains(&outputs, p1_parsed, &unavailable);
    let plan1_dt = ts.elapsed();
    p_plan1.add(plan1_dt, 1);
    let merged_domains = match &plans[0] {
        cdbgen_rs::outputs::OutputPlan::Ready { domains, .. } => domains.len(),
        _ => 0,
    };
    println!("\n[plan x1] merged={merged_domains}");
    p_plan1.report();

    // ── plan (2 outputs, shared group) — the post-fix shape ──
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
    let mut p2_parsed: BTreeMap<String, cdbgen_rs::parser::ParsedSource> = BTreeMap::new();
    for (id, body) in &bodies {
        p2_parsed.insert(id.clone(), parse_source_bytes(body));
    }
    let ts = Instant::now();
    let shared_plans = build_output_domains(&shared_outputs, p2_parsed, &unavailable);
    let plan2_dt = ts.elapsed();
    p_plan1_clone.add(plan2_dt, 1);
    let a = match &shared_plans[0] {
        cdbgen_rs::outputs::OutputPlan::Ready { domains, .. } => domains.len(),
        _ => 0,
    };
    let b = match &shared_plans[1] {
        cdbgen_rs::outputs::OutputPlan::Ready { domains, .. } => domains.len(),
        _ => 0,
    };
    assert_eq!(a, b);
    println!("\n[plan x2] shared={a} (2 outputs, 1 unique group)");
    p_plan1_clone.report();
    let overhead = plan2_dt.saturating_sub(plan1_dt);
    println!("  overhead of 2nd output = {:>9.3?}", overhead);

    // ── write ──
    let t_write = Instant::now();
    let mut p_write = Phase {
        name: "write_cdb_atomic",
        ..Default::default()
    };
    let mut bytes_written = 0usize;
    for plan in &shared_plans {
        if let cdbgen_rs::outputs::OutputPlan::Ready {
            output, domains, ..
        } = plan
        {
            let ts = Instant::now();
            write_cdb_atomic(&output.path, domains, output.key_format).unwrap();
            let dt = ts.elapsed();
            p_write.add(dt, domains.len());
            bytes_written += fs::metadata(&output.path).map(|m| m.len()).unwrap_or(0) as usize;
        }
    }
    let write_wall = t_write.elapsed();
    println!(
        "\n[write]   bytes={:.2} MiB",
        bytes_written as f64 / (1024.0 * 1024.0)
    );
    p_write.report();
    println!("  wall (incl fsync)      {:>10.3?}", write_wall);

    // ── normalize micro ──
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
    let mut p_norm = Phase {
        name: "normalize_domain",
        ..Default::default()
    };
    let ts = Instant::now();
    let mut accepted = 0usize;
    for s in &inputs {
        if normalize_domain(s).is_some() {
            accepted += 1;
        }
    }
    let norm_dt = ts.elapsed();
    p_norm.add(norm_dt, inputs.len());
    println!("\n[norm]    accepted={accepted}/{}", inputs.len());
    p_norm.report();

    // ── summary ──
    let pipeline_wall = p_parse.total + plan1_dt + plan2_dt + write_wall;
    println!(
        "\n[summary] pipeline (parse+plan1+plan2+write) = {:>9.3?}",
        pipeline_wall
    );
    let pct =
        |dt: std::time::Duration| -> f64 { 100.0 * dt.as_secs_f64() / pipeline_wall.as_secs_f64() };
    println!("  parse:   {:>6.2}%", pct(p_parse.total));
    println!("  plan x1: {:>6.2}%", pct(plan1_dt));
    println!("  plan x2: {:>6.2}%", pct(plan2_dt));
    println!("  write:   {:>6.2}%", pct(write_wall));
}

struct SplitMix64 {
    state: u64,
}
impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[(self.next_u64() as usize) % items.len()]
    }
    fn chance(&mut self, p: f64) -> bool {
        (self.next_u64() as f64 / u64::MAX as f64) < p
    }
}

struct Buf<'a>(&'a mut Vec<u8>);
impl<'a> Write for Buf<'a> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
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
