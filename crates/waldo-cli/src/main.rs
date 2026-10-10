use std::process::ExitCode;

use clap::Parser;
use waldo_cli::{Arguments, execute, reports_failure};

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("waldo: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Prints the whole result, then reports whether it records success.
async fn run() -> Result<bool, Box<dyn std::error::Error>> {
    let value = execute(Arguments::parse()).await?;
    serde_json::to_writer_pretty(std::io::stdout(), &value)?;
    println!();
    Ok(!reports_failure(&value))
}
