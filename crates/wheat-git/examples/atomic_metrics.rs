//! Fresh-destination atomic import timing; leaves the installed repository for inspection.
use std::{path::PathBuf, time::Instant};

use wheat_git::atomic_import;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let source = PathBuf::from(args.next().ok_or("source path required")?);
    let destination = PathBuf::from(args.next().ok_or("fresh destination path required")?);
    let branch = args
        .next()
        .ok_or("full working ref required")?
        .into_string()
        .map_err(|_| "branch must be UTF-8")?;
    let start = Instant::now();
    let head = atomic_import(&source, &destination, &branch).await?;
    println!(
        "seconds={:.6} generation={} envelope={}",
        start.elapsed().as_secs_f64(),
        head.generation,
        head.envelope
    );
    Ok(())
}
