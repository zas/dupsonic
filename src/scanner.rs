//! Directory scanning and parallel fingerprinting orchestration.
//!
//! Discovers audio files by extension, respects `.dupsonic-ignore` files and
//! `--ignore` patterns, and fingerprints files in parallel using Rayon.
//! Results are sent through a channel to a writer thread that batches DB writes
//! in transactions. Only new or modified files are processed (change detection
//! via size + mtime).

use anyhow::Result;
use globset::{Glob, GlobSet, GlobSetBuilder};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::{debug, info, warn};
use walkdir::WalkDir;

use crate::database::{Database, FileMeta, ScanResult};
use crate::fingerprint;

/// Audio file extensions we support (via symphonia).
const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "ogg", "opus", "wav", "m4a", "aac", "wma", "aiff", "aif", "ape", "wv", "mpc",
    "oga", "spx", "webm", "mp4",
];

/// Scan directories for audio files and fingerprint them.
pub fn scan(
    db: &Database,
    paths: &[PathBuf],
    jobs: usize,
    length: u64,
    ignore_patterns: &[String],
    force: bool,
    quiet: bool,
) -> Result<()> {
    info!("Discovering audio files...");

    // Warn about paths that don't exist
    for path in paths {
        if !path.exists() {
            eprintln!("Warning: path does not exist: {}", path.display());
        }
    }

    // Build ignore glob set
    let ignore_set = build_ignore_set(ignore_patterns, paths)?;

    let files = discover_audio_files(paths, &ignore_set);
    info!("Found {} audio files", files.len());

    // Warn if no audio files found
    if files.is_empty() {
        eprintln!(
            "Warning: no audio files found in the specified path(s).\n\
             Supported formats: {}",
            AUDIO_EXTENSIONS.join(", ")
        );
        return Ok(());
    }

    // Filter out already-fingerprinted files (unless --force)
    let to_process: Vec<PathBuf> = if force {
        files
    } else {
        let cached = db.load_cached_metadata()?;
        files
            .into_iter()
            .filter(|f| {
                let meta = match std::fs::metadata(f) {
                    Ok(m) => m,
                    Err(_) => return true, // Can't stat — try to process
                };
                match cached.get(f) {
                    Some(&(size, mtime, fp_length)) => {
                        let length_matches = fp_length
                            .map(|l| l == length as i64)
                            .unwrap_or(false);
                        // Re-process if size, mtime, or fingerprint length changed
                        !(meta.len() as i64 == size
                            && crate::database::file_mtime_secs(&meta) == mtime
                            && length_matches)
                    }
                    None => true, // Not in cache — needs processing
                }
            })
            .collect()
    };

    if to_process.is_empty() {
        if !quiet {
            println!("All files already fingerprinted. Use --force to rescan.");
        }
        return Ok(());
    }

    if !quiet {
        println!(
            "Fingerprinting {} files using {} workers...",
            to_process.len(),
            jobs
        );
    }

    let pb = if quiet {
        ProgressBar::hidden()
    } else {
        ProgressBar::new(to_process.len() as u64)
    };
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}")
            .expect("valid template")
            .progress_chars("█▓░"),
    );

    // Channel for sending fingerprint results from workers to the DB writer
    let (tx, rx) = std::sync::mpsc::sync_channel::<ScanResult>(jobs * 4);

    // DB writer thread: batches results into transactions
    let batch_size = 64;
    let db_success = AtomicUsize::new(0);
    let db_error = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        // Writer thread
        let writer = scope.spawn(|| {
            let mut batch: Vec<ScanResult> = Vec::with_capacity(batch_size);
            let mut successes = 0usize;
            let mut errors = 0usize;

            for result in rx {
                match &result {
                    ScanResult::Success { .. } => successes += 1,
                    ScanResult::Error { .. } => errors += 1,
                }
                batch.push(result);

                if batch.len() >= batch_size {
                    if let Err(e) = db.store_batch(&batch) {
                        warn!("Failed to store batch: {}", e);
                    }
                    batch.clear();
                }
            }

            // Flush remaining
            if !batch.is_empty() {
                if let Err(e) = db.store_batch(&batch) {
                    warn!("Failed to store final batch: {}", e);
                }
            }

            (successes, errors)
        });

        // Fingerprint workers
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(jobs)
            .build()
            .expect("failed to build thread pool");

        pool.install(|| {
            to_process.par_iter().for_each(|path| {
                let scan_result = match fingerprint::fingerprint_file(path, length) {
                    Ok(fp_result) => {
                        let band_hashes =
                            crate::matcher::compute_band_hashes(&fp_result.fingerprint);
                        let meta = FileMeta::from_path(path);
                        match meta {
                            Some(meta) => {
                                debug!("Fingerprinted: {}", path.display());
                                ScanResult::Success {
                                    path: path.clone(),
                                    meta,
                                    duration_secs: fp_result.duration_secs,
                                    fingerprint: fp_result.fingerprint,
                                    fingerprint_length: length,
                                    band_hashes,
                                }
                            }
                            None => ScanResult::Error {
                                path: path.clone(),
                                meta: None,
                                error: "failed to read file metadata".to_string(),
                            },
                        }
                    }
                    Err(e) => {
                        debug!("Failed to fingerprint {}: {}", path.display(), e);
                        ScanResult::Error {
                            path: path.clone(),
                            meta: FileMeta::from_path(path),
                            error: e.to_string(),
                        }
                    }
                };

                let _ = tx.send(scan_result);
                pb.inc(1);
            });
        });

        // Close the channel so the writer finishes
        drop(tx);

        // Wait for writer and get counts
        let (successes, errors) = writer.join().unwrap();
        db_success.store(successes, Ordering::Relaxed);
        db_error.store(errors, Ordering::Relaxed);
    });

    pb.finish_with_message("done");

    let successes = db_success.load(Ordering::Relaxed);
    let errors = db_error.load(Ordering::Relaxed);
    if !quiet {
        println!("Finished: {} fingerprinted, {} errors", successes, errors);
    }

    Ok(())
}

/// Walk directories and collect paths to audio files.
fn discover_audio_files(paths: &[PathBuf], ignore_set: &GlobSet) -> Vec<PathBuf> {
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
            let path = entry.path();
            if !is_audio_file(path) {
                continue;
            }
            // Check against ignore patterns
            if ignore_set.is_match(path) {
                continue;
            }
            files.push(entry.into_path());
        }
    }

    files.sort();
    files.dedup();
    files
}

/// Build a GlobSet from gitignore-style patterns.
/// Also reads patterns from `.dupsonic-ignore` files found in the scan paths.
fn build_ignore_set(patterns: &[String], scan_paths: &[PathBuf]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();

    // Add CLI --ignore patterns
    for pattern in patterns {
        add_glob_pattern(&mut builder, pattern)?;
    }

    // Read .dupsonic-ignore files from scan paths and current directory
    let mut ignore_files = vec![PathBuf::from(".dupsonic-ignore")];
    for path in scan_paths {
        let ignore_file = if path.is_dir() {
            path.join(".dupsonic-ignore")
        } else {
            path.parent()
                .map(|p| p.join(".dupsonic-ignore"))
                .unwrap_or_default()
        };
        if !ignore_files.contains(&ignore_file) {
            ignore_files.push(ignore_file);
        }
    }

    for ignore_file in &ignore_files {
        if ignore_file.is_file() {
            if let Ok(content) = std::fs::read_to_string(ignore_file) {
                for line in content.lines() {
                    let line = line.trim();
                    // Skip empty lines and comments
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    add_glob_pattern(&mut builder, line)?;
                }
            }
        }
    }

    Ok(builder.build()?)
}

fn add_glob_pattern(builder: &mut GlobSetBuilder, pattern: &str) -> Result<()> {
    let glob = Glob::new(pattern)
        .or_else(|_| Glob::new(&format!("**/{}", pattern)))
        .map_err(|e| anyhow::anyhow!("Invalid ignore pattern '{}': {}", pattern, e))?;
    builder.add(glob);
    Ok(())
}

fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| AUDIO_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
        .unwrap_or(false)
}
