//! Binary entrypoint for `rings-tui`.

use std::process::ExitCode;

use clap::Parser;
use rings_tui::config::TuiArgs;
use rings_tui::run_from_args;

#[tokio::main]
async fn main() -> ExitCode {
    let args = TuiArgs::parse();
    match run_from_args(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rings-tui: {error}");
            ExitCode::FAILURE
        }
    }
}
