use anyhow::Result;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::{debug, info, warn};
use walkdir::WalkDir;

use crate::database::Database;
use crate::fingerprint;

/// Audio file extensions we support (via symphonia).
const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "ogg", "opus", "wav", "m4a", "aac", "wma", "aiff", "aif", "ape", "wv", "mpc",
    "oga", "spx", "webm", "mp4",
];

/// Scan directories for audio files and fingerprint them.
pub fn scan(db: &Database, paths: &[PathBuf], jobs: usize, force: bool) -> Result<()> {
    info!("Discovering audio files...");
    let files = discover_audio_files(paths);
    info!("Found {} audio files", files.len());

    // Filter out already-fingerprinted files (unless --force)
    let to_process: Vec<PathBuf> = if force {
        files
    } else {
        files
            .into_iter()
            .filter(|f| !db.is_current(f).unwrap_or(false))
            .collect()
    };

    if to_process.is_empty() {
        println!("All files already fingerprinted. Use --force to rescan.");
        return Ok(());
    }

    println!(
        "Fingerprinting {} files using {} workers...",
        to_process.len(),
        jobs
    );

    let pb = ProgressBar::new(to_process.len() as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}")
            .expect("valid template")
            .progress_chars("█▓░"),
    );

    let success_count = AtomicUsize::new(0);
    let error_count = AtomicUsize::new(0);

    let pool = rayon::ThreadPoolBuilder::new().num_threads(jobs).build()?;

    pool.install(|| {
        to_process.par_iter().for_each(|path| {
            let result = fingerprint::fingerprint_file(path);
            match result {
                Ok(fp_result) => {
                    if let Err(e) = db.store_fingerprint(path, &fp_result) {
                        warn!("Failed to store fingerprint for {}: {}", path.display(), e);
                        error_count.fetch_add(1, Ordering::Relaxed);
                    } else {
                        success_count.fetch_add(1, Ordering::Relaxed);
                        debug!("Fingerprinted: {}", path.display());
                    }
                }
                Err(e) => {
                    debug!("Failed to fingerprint {}: {}", path.display(), e);
                    if let Err(store_err) = db.store_error(path, &e.to_string()) {
                        warn!(
                            "Failed to store error for {}: {}",
                            path.display(),
                            store_err
                        );
                    }
                    error_count.fetch_add(1, Ordering::Relaxed);
                }
            }
            pb.inc(1);
        });
    });

    pb.finish_with_message("done");

    let successes = success_count.load(Ordering::Relaxed);
    let errors = error_count.load(Ordering::Relaxed);
    println!("Finished: {} fingerprinted, {} errors", successes, errors);

    Ok(())
}

/// Walk directories and collect paths to audio files.
fn discover_audio_files(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();

    for base_path in paths {
        for entry in WalkDir::new(base_path)
            .follow_links(true)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            if is_audio_file(entry.path()) {
                files.push(entry.into_path());
            }
        }
    }

    files.sort();
    files.dedup();
    files
}

fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| AUDIO_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
        .unwrap_or(false)
}
