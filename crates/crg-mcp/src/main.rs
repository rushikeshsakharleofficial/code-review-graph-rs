//! crg-mcp binary — starts the MCP server on stdio transport.

use rmcp::{ServiceExt, transport::stdio};
use tracing::info;

use crg_mcp::CrgServer;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Log to stderr — stdout is the MCP protocol channel.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("crg_mcp=info".parse()?)
                .add_directive("rmcp=warn".parse()?),
        )
        .init();

    let repo_root = CrgServer::resolve_repo_root();
    info!("Starting crg-mcp server for repo: {repo_root}");

    let server = CrgServer::new(repo_root);
    let service = server.serve(stdio()).await?;
    service.waiting().await?;

    Ok(())
}
