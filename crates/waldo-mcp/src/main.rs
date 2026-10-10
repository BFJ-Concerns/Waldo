use std::process::ExitCode;

use waldo_mcp::McpServer;
use waldo_settings::Settings;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("waldo-mcp: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let settings = Settings::load()?;
    let server =
        McpServer::with_image_max_edge(settings.socket_path()?, settings.model_image_max_edge()?);
    server.run(tokio::io::stdin(), tokio::io::stdout()).await?;
    Ok(())
}
