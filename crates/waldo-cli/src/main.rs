use std::process::ExitCode;

use clap::Parser;
use waldo_cli::{Arguments, execute};

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("waldo: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let value = execute(Arguments::parse()).await?;
    serde_json::to_writer_pretty(std::io::stdout(), &value)?;
    println!();
    Ok(())
}
