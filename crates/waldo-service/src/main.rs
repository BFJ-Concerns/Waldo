use std::{path::PathBuf, time::Duration};

use clap::Parser;
use waldo_service::{ControllerService, default_socket_path};

#[derive(Debug, Parser)]
#[command(name = "waldod", about = "Waldo's session-scoped desktop controller")]
struct Arguments {
    /// Override the private controller socket path.
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Release an inactive driving lease after this many seconds.
    #[arg(long, default_value_t = 30)]
    idle_timeout_seconds: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = Arguments::parse();
    let socket = match arguments.socket {
        Some(path) => path,
        None => default_socket_path()?,
    };
    let service =
        ControllerService::bind(&socket, Duration::from_secs(arguments.idle_timeout_seconds))?;

    eprintln!("Waldo controller listening on {}", socket.display());
    service
        .run_until(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
