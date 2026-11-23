use std::collections::BTreeMap;

use crate::{
    cdb_writer::write_cdb_atomic, config::OutputConfig, error::OutputError, parser::ParsedSource,
};

#[derive(Debug, Clone)]
pub enum OutputPlan {
    Ready {
        output: OutputConfig,
        domains: Vec<String>,
        blocks_before_allow: usize,
        allows: usize,
    },
    Skipped {
        output: OutputConfig,
        unavailable_sources: Vec<String>,
    },
    Empty {
        output: OutputConfig,
    },
}

#[derive(Debug, Clone, Copy, Default)]
pub struct OutputWriteSummary {
    pub written_outputs: usize,
    pub empty_outputs: usize,
}

struct MergedGroup {
    /// Sorted, deduplicated, allow-subtracted block list.
    blocks: Vec<String>,
    blocks_before_allow: usize,
    allows: usize,
}

pub fn build_output_domains(
    outputs: &[OutputConfig],
    parsed: BTreeMap<String, ParsedSource>,
    unavailable: &std::collections::BTreeSet<String>,
) -> Vec<OutputPlan> {
    let mut plans = Vec::with_capacity(outputs.len());
    // Cache merged groups by their source-id signature so outputs that share
    // sources (e.g. the same group written to two paths) reuse the same
    // sorted/deduped Vec instead of re-merging or — as a previous bug —
    // emptying the source on the first pass via mem::take.
    let mut cache: BTreeMap<Vec<String>, MergedGroup> = BTreeMap::new();

    for output in outputs {
        let missing: Vec<String> = output
            .source_ids
            .iter()
            .filter(|source_id| {
                unavailable.contains(*source_id) || !parsed.contains_key(*source_id)
            })
            .cloned()
            .collect();

        if !missing.is_empty() {
            plans.push(OutputPlan::Skipped {
                output: output.clone(),
                unavailable_sources: missing,
            });
            continue;
        }

        let signature = output.source_ids.clone();
        let merged = cache
            .entry(signature)
            .or_insert_with(|| merge_group(&output.source_ids, &parsed));

        if merged.blocks.is_empty() {
            plans.push(OutputPlan::Empty {
                output: output.clone(),
            });
        } else {
            plans.push(OutputPlan::Ready {
                output: output.clone(),
                domains: merged.blocks.clone(),
                blocks_before_allow: merged.blocks_before_allow,
                allows: merged.allows,
            });
        }
    }

    plans
}

/// Merge the named sources into a single sorted/deduped block list, with
/// `allow` entries subtracted. Returns the `MergedGroup` that downstream
/// outputs will cache and clone.
fn merge_group(source_ids: &[String], parsed: &BTreeMap<String, ParsedSource>) -> MergedGroup {
    let mut blocks: Vec<String> = Vec::new();
    let mut allows: Vec<String> = Vec::new();
    for source_id in source_ids {
        if let Some(source) = parsed.get(source_id) {
            blocks.extend(source.blocks.iter().cloned());
            allows.extend(source.allows.iter().cloned());
        }
    }
    let blocks_before_allow = blocks.len();
    if !allows.is_empty() {
        // Sort + dedup allows, then remove them from blocks (also sorted+deduped).
        // `retain` on a sorted vec with a sorted needle is O(n+m) when we
        // walk both in order; here we use a HashSet-style lookup via
        // sort_unstable + binary_search because the allow list is typically
        // tiny compared to blocks. binary_search requires a sorted needle.
        allows.sort_unstable();
        allows.dedup();
        // Sort blocks now so the allow-subtract is one linear pass instead
        // of n × O(log n) HashSet lookups. (The downstream writer also wants
        // sorted input for deterministic CDB output.)
        blocks.sort_unstable();
        blocks.dedup();
        subtract_sorted(&mut blocks, &allows);
    } else {
        blocks.sort_unstable();
        blocks.dedup();
    }
    MergedGroup {
        blocks,
        blocks_before_allow,
        allows: allows.len(),
    }
}

/// Remove every element of `needles` from `haystack` in place. Both inputs
/// must be sorted+deduped. Linear pass; `needles` is the smaller set.
#[inline]
fn subtract_sorted(haystack: &mut Vec<String>, needles: &[String]) {
    if needles.is_empty() {
        return;
    }
    let mut write = 0usize;
    let mut read = 0usize;
    let mut n = 0usize;
    while read < haystack.len() {
        if n < needles.len() && haystack[read] == needles[n] {
            // skip this element
            n += 1;
        } else {
            if write != read {
                haystack[write] = std::mem::take(&mut haystack[read]);
            }
            write += 1;
        }
        read += 1;
    }
    haystack.truncate(write);
}

pub fn write_outputs(plans: &[OutputPlan]) -> Result<OutputWriteSummary, OutputError> {
    let mut summary = OutputWriteSummary::default();

    for plan in plans {
        match plan {
            OutputPlan::Ready {
                output,
                domains,
                blocks_before_allow,
                allows,
            } => {
                tracing::info!(
                    output = %output.path.display(),
                    key_format = output.key_format.as_str(),
                    domains = domains.len(),
                    blocks_before_allow,
                    allows,
                    "writing output"
                );
                write_cdb_atomic(&output.path, domains, output.key_format)?;
                tracing::info!(
                    output = %output.path.display(),
                    key_format = output.key_format.as_str(),
                    domains = domains.len(),
                    "atomic replacement complete"
                );
                summary.written_outputs += 1;
            }
            OutputPlan::Skipped {
                output,
                unavailable_sources,
            } => {
                tracing::warn!(
                    output = %output.path.display(),
                    key_format = output.key_format.as_str(),
                    unavailable = ?unavailable_sources,
                    "skipping output due to unavailable sources"
                );
            }
            OutputPlan::Empty { output } => {
                tracing::error!(
                    output = %output.path.display(),
                    key_format = output.key_format.as_str(),
                    "output would contain zero domains; preserving existing file"
                );
                summary.empty_outputs += 1;
            }
        }
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::ParseStats;
    use std::{collections::BTreeSet, path::PathBuf};

    fn output(source_ids: Vec<&str>) -> OutputConfig {
        OutputConfig {
            group: source_ids.join(", "),
            source_ids: source_ids.into_iter().map(ToOwned::to_owned).collect(),
            path: PathBuf::from("/tmp/out.cdb"),
            key_format: crate::config::OutputKeyFormat::Wire,
        }
    }

    fn parsed(blocks: &[&str], allows: &[&str]) -> ParsedSource {
        ParsedSource {
            detected_format: crate::parser::SourceFormat::Hostfile,
            blocks: blocks.iter().map(|value| (*value).to_string()).collect(),
            allows: allows.iter().map(|value| (*value).to_string()).collect(),
            stats: ParseStats::default(),
        }
    }

    #[test]
    fn subtracts_allows() {
        let mut sources = BTreeMap::new();
        sources.insert(
            "a".into(),
            parsed(&["a.example", "b.example"], &["b.example"]),
        );
        let plans = build_output_domains(&[output(vec!["a"])], sources, &BTreeSet::new());

        match &plans[0] {
            OutputPlan::Ready { domains, .. } => {
                assert_eq!(domains, &vec!["a.example".to_string()]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn skips_outputs_with_unavailable_sources() {
        let sources = BTreeMap::new();
        let unavailable = BTreeSet::from(["b".to_string()]);
        let plans = build_output_domains(&[output(vec!["a", "b"])], sources, &unavailable);
        assert!(matches!(plans[0], OutputPlan::Skipped { .. }));
    }

    #[test]
    fn marks_empty_outputs() {
        let mut sources = BTreeMap::new();
        sources.insert("a".into(), parsed(&["a.example"], &["a.example"]));
        let plans = build_output_domains(&[output(vec!["a"])], sources, &BTreeSet::new());
        assert!(matches!(plans[0], OutputPlan::Empty { .. }));
    }

    #[test]
    fn shares_domains_across_outputs_with_same_sources() {
        // Two outputs targeting different paths but built from the same
        // source group must each receive the merged domain set. The previous
        // implementation used mem::take and would empty the source after the
        // first output, leaving the second with zero domains.
        let mut sources = BTreeMap::new();
        sources.insert(
            "a".into(),
            parsed(&["a.example", "b.example"], &["b.example"]),
        );
        let mut out_a = output(vec!["a"]);
        out_a.path = PathBuf::from("/tmp/a.cdb");
        let mut out_b = output(vec!["a"]);
        out_b.path = PathBuf::from("/tmp/b.cdb");
        let plans = build_output_domains(&[out_a, out_b], sources, &BTreeSet::new());

        assert_eq!(plans.len(), 2);
        for plan in &plans {
            match plan {
                OutputPlan::Ready { domains, .. } => {
                    assert_eq!(domains, &vec!["a.example".to_string()]);
                }
                other => panic!("expected Ready, got {other:?}"),
            }
        }
    }
}
