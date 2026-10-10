use std::{fs, path::PathBuf};

use clap::{Parser, Subcommand};
use waldo_verification::{
    BoundaryTargets, FixtureRect, RunRecord, TargetGrid, capture_niri_topology,
};

mod clipboard_outside_in;
mod digest;
mod final_campaign;
mod fixture_poll;
mod outside_in;
mod private_json;
mod toolkit_preflight;

/// Largest accepted seed. Seeds reach the Electron fixture as JavaScript
/// numbers, exact only up to 2^53 - 1, after the campaign adds per-circuit
/// offsets of up to 0x2000.
const MAX_SEED: u64 = (1 << 53) - 1 - 0x2000;
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
        #[arg(long, value_parser = clap::value_parser!(u64).range(..=MAX_SEED))]
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
        #[arg(long, default_value_t = 0x9c2e, value_parser = clap::value_parser!(u64).range(..=MAX_SEED))]
        seed: u64,
    },
    /// Exercise accessibility actions against a deterministic GTK accessibility oracle.
    AccessibilityOutsideIn {
        evidence_directory: PathBuf,
        #[arg(long)]
        waldod: PathBuf,
        #[arg(long)]
        mcp: PathBuf,
        #[arg(long)]
        fixture: PathBuf,
        #[arg(long, default_value = "python3")]
        python: PathBuf,
        #[arg(long, default_value_t = 0xa3c7, value_parser = clap::value_parser!(u64).range(..=MAX_SEED))]
        seed: u64,
    },
    /// Exercise clipboard offer, preservation, and final-consumer certainty.
    ClipboardOutsideIn {
        evidence_directory: PathBuf,
        #[arg(long)]
        waldod: PathBuf,
        #[arg(long)]
        mcp: PathBuf,
        #[arg(long)]
        fixture: PathBuf,
        #[arg(long, default_value = "python3")]
        python: PathBuf,
        #[arg(long, default_value_t = 0xc8e5, value_parser = clap::value_parser!(u64).range(..=MAX_SEED))]
        seed: u64,
    },
    /// Preflight equivalent toolkit fixtures and emit provisional matrix admission evidence.
    ToolkitPreflight {
        evidence_directory: PathBuf,
        #[arg(long)]
        fixture_directory: PathBuf,
        #[arg(long, default_value = "python3")]
        python: PathBuf,
        #[arg(long, default_value = "electron")]
        electron: PathBuf,
        #[arg(long, default_value_t = 0xd35c, value_parser = clap::value_parser!(u64).range(..=MAX_SEED))]
        seed: u64,
        #[arg(long)]
        probe_only: bool,
    },
    /// Run the complete exact-binary, final-consumer desktop conformance campaign.
    FinalCampaign {
        evidence_directory: PathBuf,
        #[arg(long)]
        waldod: PathBuf,
        #[arg(long)]
        mcp: PathBuf,
        #[arg(long)]
        cli: PathBuf,
        #[arg(long)]
        fixture_directory: PathBuf,
        #[arg(long)]
        corpus_directory: PathBuf,
        #[arg(long)]
        plugin_manifest: PathBuf,
        #[arg(long, default_value = "python3")]
        python: PathBuf,
        #[arg(long, default_value = "electron")]
        electron: PathBuf,
        #[arg(long, default_value_t = 0xf17a, value_parser = clap::value_parser!(u64).range(..=MAX_SEED))]
        seed: u64,
        /// Skip migrated legacy circuits for a non-acceptance diagnostic run.
        #[arg(long)]
        skip_legacy: bool,
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
            require_accepted(summary.failures())?;
        }
        Command::AccessibilityOutsideIn {
            evidence_directory,
            waldod,
            mcp,
            fixture,
            python,
            seed,
        } => {
            let summary = outside_in::run_accessibility(outside_in::Options {
                evidence_directory,
                waldod,
                mcp,
                fixture,
                python,
                seed,
            })?;
            emit_json(&summary, None)?;
            require_accepted(summary.failures())?;
        }
        Command::ClipboardOutsideIn {
            evidence_directory,
            waldod,
            mcp,
            fixture,
            python,
            seed,
        } => {
            let summary = clipboard_outside_in::run(outside_in::Options {
                evidence_directory,
                waldod,
                mcp,
                fixture,
                python,
                seed,
            })?;
            emit_json(&summary, None)?;
        }
        Command::ToolkitPreflight {
            evidence_directory,
            fixture_directory,
            python,
            electron,
            seed,
            probe_only,
        } => {
            let summary = toolkit_preflight::run(toolkit_preflight::Options {
                evidence_directory,
                fixture_directory,
                python,
                electron,
                seed,
                probe_only,
            })?;
            emit_json(&summary, None)?;
        }
        Command::FinalCampaign {
            evidence_directory,
            waldod,
            mcp,
            cli,
            fixture_directory,
            corpus_directory,
            plugin_manifest,
            python,
            electron,
            seed,
            skip_legacy,
        } => {
            let summary = final_campaign::run(final_campaign::Options {
                evidence_directory,
                waldod,
                mcp,
                cli,
                fixture_directory,
                corpus_directory,
                plugin_manifest,
                python,
                electron,
                seed,
                skip_legacy,
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
    if let Some(output) = output {
        private_json::write(output, value)?;
    } else {
        let mut json = serde_json::to_vec_pretty(value)?;
        json.push(b'\n');
        print!("{}", String::from_utf8(json)?);
    }
    Ok(())
}

#[cfg(test)]
#[path = "tests/electron_defaults.rs"]
mod electron_defaults;

/// Fails the command, after its summary is printed, when any check in it
/// did not pass.
fn require_accepted(failures: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!("checks not passed: {}", failures.join("; ")).into())
    }
}
