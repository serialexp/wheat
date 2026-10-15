//! Standalone wheat native protocol server.
//!
//! Serves wheat repositories over WebSocket. Each repository is backed by
//! either local file stores or S3-backed packed segments.
//!
//! Usage:
//!   wheat-server --data-dir ./my-repo
//!   wheat-server --data-dir ./my-repo --listen 127.0.0.1:7420
//!   wheat-server --data-dir ./my-repo --store s3-packed  # S3 config from env vars
//!   wheat-server --data-dir ./my-repo --config wheat-server.toml --listen 0.0.0.0:7420
//!   openssl rand -hex 32 | wheat-server hash-token        # hash for token_hashes
//!
//! Without a config file that has an `[auth]` table, the server lets anyone in
//! and so refuses to listen on anything but a loopback address.

use std::io::Read as _;
use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tracing_subscriber::{Layer as _, layer::SubscriberExt as _, util::SubscriberInitExt as _};

use wheat_server::access::{MAX_AUTH_TOKEN_BYTES, token_hash_hex};
use wheat_server::config::FileConfig;
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

#[derive(Subcommand)]
enum ServerCommand {
    /// Read a token from stdin and print the hash to put in `token_hashes`.
    HashToken,
}

#[derive(Parser)]
#[command(
    name = "wheat-server",
    about = "Wheat native protocol server",
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<ServerCommand>,

    /// Directory containing a wheat repository. For `--store local`, this
    /// may be either a loose store (objects/, content/, repository.head) or
    /// a packed store (storage/, repository.head) — auto-detected. For
    /// `--store s3-packed`, the repository.head is read from this directory
    /// while objects and content are stored on S3.
    #[arg(long, required = true)]
    data_dir: Option<PathBuf>,

    /// TOML config file. Its `[auth]` table lists principals, their token
    /// hashes and their access per repository.
    #[arg(long)]
    config: Option<PathBuf>,

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
    // `WHEAT_TRACE` / `WHEAT_TRACE_SUMMARY` record spans beside the log, and report
    // when the server is stopped with Ctrl-C.
    let (_span_trace, span_layer) = wheat_trace::SpanTrace::from_env().unzip();
    let log_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(log_filter))
        .with(span_layer)
        .init();

    let cli = Cli::parse();
    if let Some(ServerCommand::HashToken) = cli.command {
        println!("{}", hash_token_from_stdin()?);
        return Ok(());
    }
    let data_dir = cli.data_dir.expect("clap requires --data-dir when serving");

    let file_config = match &cli.config {
        Some(path) => FileConfig::load(path)?,
        None => FileConfig::default(),
    };
    let server_config = ServerConfig::from_file(&file_config)?;
    server_config.check_listen_address(cli.listen)?;

    let repo_name = cli.name.clone().unwrap_or_else(|| {
        data_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("default")
            .to_string()
    });

    let registry = match cli.store {
        StoreMode::Local => Registry::single_named(repo_name, &data_dir)?,
        StoreMode::S3Packed => {
            let config = s3_config_from_env()?;
            let store = wheat_store::RemotePackedStore::open(&config).await?;
            let content = store.content_store();
            let heads = wheat_store::LocalPublicationStore::new(data_dir.join("repository.head"));
            Registry::single_with_stores(repo_name, Box::new(store), content, heads)
        }
    };

    tokio::select! {
        served = run(cli.listen, registry, server_config) => served?,
        stopped = tokio::signal::ctrl_c() => {
            stopped?;
            tracing::info!("interrupted; stopping");
        }
    }
    Ok(())
}

/// Read one token from stdin and return its hash for `token_hashes`. Reading
/// stdin keeps the token out of shell history and `ps`.
fn hash_token_from_stdin() -> Result<String, Box<dyn std::error::Error>> {
    let mut input = Vec::new();
    std::io::stdin()
        .take(MAX_AUTH_TOKEN_BYTES as u64 + 3)
        .read_to_end(&mut input)?;
    let token = input
        .strip_suffix(b"\r\n")
        .or_else(|| input.strip_suffix(b"\n"))
        .unwrap_or(&input);
    if token.is_empty() {
        return Err("no token on stdin".into());
    }
    if token.len() > MAX_AUTH_TOKEN_BYTES {
        return Err(format!("tokens are at most {MAX_AUTH_TOKEN_BYTES} bytes").into());
    }
    if token.contains(&b'\n') {
        return Err("the token must be a single line".into());
    }
    Ok(token_hash_hex(token))
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
