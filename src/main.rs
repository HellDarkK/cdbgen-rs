use clap::Parser;

use cdbgen_rs::{cli::Cli, error::AppError};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    cdbgen_rs::logging::init(cli.verbose);

    let code = match cdbgen_rs::run(cli).await {
        Ok(code) => code,
        Err(err) => {
            tracing::error!("{err}");
            err.exit_code()
        }
    };

    std::process::exit(code);
}

trait ExitCode {
    fn exit_code(&self) -> i32;
}

impl ExitCode for AppError {
    fn exit_code(&self) -> i32 {
        cdbgen_rs::error::exit_code_for_error(self)
    }
}
