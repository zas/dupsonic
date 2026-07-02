use anyhow::Result;
use globset::{Glob, GlobSet, GlobSetBuilder};
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
        files
            .into_iter()
            .filter(|f| !db.is_current(f, length).unwrap_or(false))
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

    let success_count = AtomicUsize::new(0);
    let error_count = AtomicUsize::new(0);

    let pool = rayon::ThreadPoolBuilder::new().num_threads(jobs).build()?;

    pool.install(|| {
        to_process.par_iter().for_each(|path| {
            let result = fingerprint::fingerprint_file(path, length);
            match result {
                Ok(fp_result) => {
                    if let Err(e) = db.store_fingerprint(path, &fp_result, length) {
                        warn!("Failed to store fingerprint for {}: {}", path.display(), e);
                        error_count.fetch_add(1, Ordering::Relaxed);
                    } else {
                        // Pre-compute and store band hashes for DB-based candidate generation
                        let hashes = crate::matcher::compute_band_hashes(&fp_result.fingerprint);
                        let _ = db.store_band_hashes(path, &hashes);
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
