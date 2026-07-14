use waldo_mcp::McpServer;
use waldo_socket::default_socket_path;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let server = McpServer::new(default_socket_path()?)?;
    server.run(tokio::io::stdin(), tokio::io::stdout()).await?;
    Ok(())
}
