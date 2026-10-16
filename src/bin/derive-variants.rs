//! CLI wrapper around `crate::derive` — derives variants for the given run
//! directories. The server also derives automatically on first download.

#[path = "../derive.rs"]
mod derive;

fn main() -> anyhow::Result<()> {
    let dirs: Vec<std::path::PathBuf> =
        std::env::args().skip(1).map(std::path::PathBuf::from).collect();
    if dirs.is_empty() {
        eprintln!("usage: derive-variants <run-dir> [run-dir ...]");
        std::process::exit(2);
    }

    let mut total = 0usize;
    for dir in &dirs {
        let started = std::time::Instant::now();
        let count = derive::derive_run(dir)
            .with_context(|| format!("{}", dir.display()))?;
        let rate = if started.elapsed().as_secs_f64() > 0.0 {
            count as f64 / started.elapsed().as_secs_f64()
        } else {
            0.0
        };
        println!("{}: {count} images ({rate:.1}/s)", dir.display());
        total += count;
    }
    println!("derived variants for {total} images");
    Ok(())
}

use anyhow::Context as _;
