use std::{fs, path::PathBuf};

use clap::{Parser, Subcommand};
use waldo_verification::{
    BoundaryTargets, FixtureRect, RunRecord, TargetGrid, capture_niri_topology,
};

mod outside_in;

#[derive(Debug, Parser)]
#[command(about = "Prepare and validate deterministic Waldo verification evidence")]
struct Arguments {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Validate a retained run record and all of its evidence references.
    ValidateRecord { input: PathBuf },
    /// Emit a reproducible target grid for an independent fixture.
    TargetGrid {
        #[arg(long)]
        x: u32,
        #[arg(long)]
        y: u32,
        #[arg(long)]
        width: u32,
        #[arg(long)]
        height: u32,
        #[arg(long)]
        columns: u32,
        #[arg(long)]
        rows: u32,
        #[arg(long)]
        seed: u64,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Emit deterministic in-bounds corner, edge, and centre targets.
    BoundaryTargets {
        #[arg(long)]
        x: u32,
        #[arg(long)]
        y: u32,
        #[arg(long)]
        width: u32,
        #[arg(long)]
        height: u32,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Capture a serial-free topology oracle directly from Niri IPC.
    NiriTopology {
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Exercise real Waldo binaries against disposable application-level oracles.
    OutsideIn {
        evidence_directory: PathBuf,
        #[arg(long)]
        waldod: PathBuf,
        #[arg(long)]
        mcp: PathBuf,
        #[arg(long)]
        fixture: PathBuf,
        #[arg(long, default_value = "python3")]
        python: PathBuf,
        #[arg(long, default_value_t = 0x9c2e)]
        seed: u64,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    match Arguments::parse().command {
        Command::ValidateRecord { input } => {
            let record: RunRecord = serde_json::from_slice(&fs::read(&input)?)?;
            record.validate()?;
            println!("valid run record: {}", record.run_id);
        }
        Command::TargetGrid {
            x,
            y,
            width,
            height,
            columns,
            rows,
            seed,
            output,
        } => {
            let grid = TargetGrid::generate(
                FixtureRect {
                    x,
                    y,
                    width,
                    height,
                },
                columns,
                rows,
                seed,
            )?;
            emit_json(&grid, output)?;
        }
        Command::BoundaryTargets {
            x,
            y,
            width,
            height,
            output,
        } => {
            let targets = BoundaryTargets::generate(FixtureRect {
                x,
                y,
                width,
                height,
            })?;
            emit_json(&targets, output)?;
        }
        Command::NiriTopology { output } => {
            emit_json(&capture_niri_topology()?, output)?;
        }
        Command::OutsideIn {
            evidence_directory,
            waldod,
            mcp,
            fixture,
            python,
            seed,
        } => {
            let summary = outside_in::run(outside_in::Options {
                evidence_directory,
                waldod,
                mcp,
                fixture,
                python,
                seed,
            })?;
            emit_json(&summary, None)?;
        }
    }
    Ok(())
}

fn emit_json(
    value: &impl serde::Serialize,
    output: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut json = serde_json::to_vec_pretty(value)?;
    json.push(b'\n');
    if let Some(output) = output {
        fs::write(output, json)?;
    } else {
        print!("{}", String::from_utf8(json)?);
    }
    Ok(())
}
