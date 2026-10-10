use std::{path::PathBuf, process::ExitCode};

use clap::Parser;
use tokio::signal::unix::{SignalKind, signal};
use waldo_service::ControllerService;
use waldo_settings::Settings;

#[derive(Debug, Parser)]
#[command(name = "waldod", about = "Waldo's session-scoped desktop controller")]
struct Arguments {
    /// Override the private controller socket path (otherwise WALDO_SOCKET_PATH).
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Release an inactive driving lease after this many seconds (otherwise
    /// WALDO_LEASE_IDLE_TIMEOUT_SECONDS). Zero is refused: the lease cannot be
    /// left to persist ambiently.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    idle_timeout_seconds: Option<u64>,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("waldod: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = Arguments::parse();
    let settings = Settings::load()?;
    let socket = match arguments.socket {
        Some(path) => path,
        None => settings.socket_path()?,
    };
    let idle_timeout = match arguments.idle_timeout_seconds {
        Some(seconds) => std::time::Duration::from_secs(seconds),
        None => settings.lease_idle_timeout()?,
    };
    let service = ControllerService::bind(&socket, idle_timeout, &settings)?;

    eprintln!("Waldo controller listening on {}", socket.display());
    let mut terminate = signal(SignalKind::terminate())?;
    service
        .run_until(async {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
        })
        .await?;
    Ok(())
}
