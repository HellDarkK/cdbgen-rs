use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Clone, Parser)]
#[command(name = "cdbgen-rs")]
#[command(
    version,
    about = "Generate DJB-compatible CDB files from remote blocklists"
)]
pub struct Cli {
    #[arg(long, default_value = "/etc/cdbgen/config.toml")]
    pub config: PathBuf,

    #[arg(long)]
    pub dry_run: bool,

    #[arg(long)]
    pub verbose: bool,

    #[arg(long)]
    pub force_refresh: bool,
}
