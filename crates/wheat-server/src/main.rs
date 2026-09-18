//! Standalone wheat native protocol server.
//!
//! Serves wheat repositories over WebSocket. Each repository is backed by
//! local file stores (objects, content, head pointer).
//!
//! Usage:
//!   wheat-server --data-dir ./my-repo
//!   wheat-server --data-dir ./my-repo --listen 127.0.0.1:7420

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;

use wheat_server::registry::Registry;
use wheat_server::server::{ServerConfig, run};

#[derive(Parser)]
#[command(name = "wheat-server", about = "Wheat native protocol server")]
struct Cli {
    /// Directory containing a wheat repository (objects/, content/,
    /// repository.head). The directory name becomes the repository name.
    #[arg(long)]
    data_dir: PathBuf,

    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:7420")]
    listen: SocketAddr,

    /// Optional repository name (defaults to the data directory name).
    #[arg(long)]
    name: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    let registry = if let Some(name) = cli.name {
        Registry::single_named(name, &cli.data_dir)
    } else {
        Registry::single(&cli.data_dir)
    };

    run(cli.listen, registry, ServerConfig::new()).await?;
    Ok(())
}
