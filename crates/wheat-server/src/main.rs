//! Standalone wheat native protocol server.
//!
//! Serves wheat repositories over WebSocket. Each repository is backed by
//! either local file stores or S3-backed packed segments.
//!
//! Usage:
//!   wheat-server --data-dir ./my-repo
//!   wheat-server --data-dir ./my-repo --listen 127.0.0.1:7420
//!   wheat-server --data-dir ./my-repo --store s3-packed  # S3 config from env vars

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;

use wheat_server::registry::Registry;
use wheat_server::server::{ServerConfig, run};

#[derive(Clone, Debug, Default, clap::ValueEnum)]
enum StoreMode {
    /// Local filesystem (objects/, content/, repository.head).
    #[default]
    Local,
    /// S3-backed packed segments. Configure via WHEAT_S3_* env vars.
    S3Packed,
}

#[derive(Parser)]
#[command(name = "wheat-server", about = "Wheat native protocol server")]
struct Cli {
    /// Directory containing a wheat repository. For `--store local`, this
    /// may be either a loose store (objects/, content/, repository.head) or
    /// a packed store (storage/, repository.head) — auto-detected. For
    /// `--store s3-packed`, the repository.head is read from this directory
    /// while objects and content are stored on S3.
    #[arg(long)]
    data_dir: PathBuf,

    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:7420")]
    listen: SocketAddr,

    /// Optional repository name (defaults to the data directory name).
    #[arg(long)]
    name: Option<String>,

    /// Storage backend for objects and content.
    #[arg(long, value_enum, default_value = "local")]
    store: StoreMode,
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

    let repo_name = cli.name.clone().unwrap_or_else(|| {
        cli.data_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("default")
            .to_string()
    });

    let registry = match cli.store {
        StoreMode::Local => Registry::single_named(repo_name, &cli.data_dir)?,
        StoreMode::S3Packed => {
            let config = s3_config_from_env()?;
            let store = wheat_store::RemotePackedStore::open(&config).await?;
            let content = store.content_store();
            let heads =
                wheat_store::LocalPublicationStore::new(cli.data_dir.join("repository.head"));
            Registry::single_with_stores(repo_name, Box::new(store), content, heads)
        }
    };

    run(cli.listen, registry, ServerConfig::new()).await?;
    Ok(())
}

fn s3_config_from_env() -> Result<wheat_store::RemotePackedStoreConfig, Box<dyn std::error::Error>>
{
    let endpoint = std::env::var("WHEAT_S3_ENDPOINT")
        .map_err(|_| "WHEAT_S3_ENDPOINT must be set for --store s3-packed")?;
    let bucket = std::env::var("WHEAT_S3_BUCKET")
        .map_err(|_| "WHEAT_S3_BUCKET must be set for --store s3-packed")?;
    let region = std::env::var("WHEAT_S3_REGION").unwrap_or_else(|_| "us-east-1".to_string());
    let access_key_id = std::env::var("WHEAT_S3_ACCESS_KEY_ID")
        .map_err(|_| "WHEAT_S3_ACCESS_KEY_ID must be set for --store s3-packed")?;
    let secret_access_key = std::env::var("WHEAT_S3_SECRET_ACCESS_KEY")
        .map_err(|_| "WHEAT_S3_SECRET_ACCESS_KEY must be set for --store s3-packed")?;
    let prefix = std::env::var("WHEAT_S3_PREFIX").unwrap_or_else(|_| "wheat".to_string());

    Ok(wheat_store::RemotePackedStoreConfig {
        s3: wheat_store::S3ObjectStoreConfig {
            endpoint: endpoint.clone(),
            bucket,
            region,
            access_key_id,
            secret_access_key,
            prefix,
            allow_http: endpoint.starts_with("http://"),
        },
        segment_cache_bytes: None,
    })
}
