use std::collections::{BTreeMap, BTreeSet};

use crate::{
    cdb_writer::write_cdb_atomic, config::OutputConfig, error::OutputError, parser::ParsedSource,
};

#[derive(Debug, Clone)]
pub enum OutputPlan {
    Ready {
        output: OutputConfig,
        domains: BTreeSet<String>,
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

pub fn build_output_domains(
    outputs: &[OutputConfig],
    parsed: &BTreeMap<String, ParsedSource>,
    unavailable: &BTreeSet<String>,
) -> Vec<OutputPlan> {
    let mut plans = Vec::with_capacity(outputs.len());

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

        let mut blocks = BTreeSet::new();
        let mut allows = BTreeSet::new();
        for source_id in &output.source_ids {
            if let Some(source) = parsed.get(source_id) {
                blocks.extend(source.blocks.iter().cloned());
                allows.extend(source.allows.iter().cloned());
            }
        }

        let blocks_before_allow = blocks.len();
        for allow in &allows {
            blocks.remove(allow);
        }

        if blocks.is_empty() {
            plans.push(OutputPlan::Empty {
                output: output.clone(),
            });
        } else {
            plans.push(OutputPlan::Ready {
                output: output.clone(),
                domains: blocks,
                blocks_before_allow,
                allows: allows.len(),
            });
        }
    }

    plans
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
    use std::path::PathBuf;

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
        let plans = build_output_domains(&[output(vec!["a"])], &sources, &BTreeSet::new());

        match &plans[0] {
            OutputPlan::Ready { domains, .. } => {
                assert_eq!(domains, &BTreeSet::from(["a.example".to_string()]));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn skips_outputs_with_unavailable_sources() {
        let sources = BTreeMap::new();
        let unavailable = BTreeSet::from(["b".to_string()]);
        let plans = build_output_domains(&[output(vec!["a", "b"])], &sources, &unavailable);
        assert!(matches!(plans[0], OutputPlan::Skipped { .. }));
    }

    #[test]
    fn marks_empty_outputs() {
        let mut sources = BTreeMap::new();
        sources.insert("a".into(), parsed(&["a.example"], &["a.example"]));
        let plans = build_output_domains(&[output(vec!["a"])], &sources, &BTreeSet::new());
        assert!(matches!(plans[0], OutputPlan::Empty { .. }));
    }
}
