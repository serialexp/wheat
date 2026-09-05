use std::{path::PathBuf, time::Instant};

use wheat_git::import_repository;
use wheat_store::ContentStore;

fn main() {
    let repository = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .expect("usage: cargo run -p wheat-git --example import_metrics -- REPOSITORY");
    let storage = std::env::temp_dir().join(format!("wheat-import-metrics-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&storage);
    let import_started = Instant::now();
    let imported = import_repository(&repository, &ContentStore::new(&storage))
        .expect("Git repository imports into Wheat");
    let import_elapsed = import_started.elapsed();
    let closure_started = Instant::now();
    let closure = imported.repository.canonical_closure();
    let closure_elapsed = closure_started.elapsed();
    let metadata_bytes = closure
        .objects
        .iter()
        .map(|object| object.bytes.len())
        .sum::<usize>();
    println!("import_ms={:.3}", import_elapsed.as_secs_f64() * 1_000.0);
    println!("closure_ms={:.3}", closure_elapsed.as_secs_f64() * 1_000.0);
    println!("commits={}", imported.commit_count);
    println!("merges={}", imported.merge_count);
    println!("branches={}", imported.branches.len());
    println!("unique_content={}", imported.unique_content);
    println!("wheat_objects={}", closure.objects.len());
    println!("wheat_metadata_bytes={metadata_bytes}");
    println!("required_content={}", closure.required_content.len());
    let _ = std::fs::remove_dir_all(storage);
}
