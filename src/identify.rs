//! The `identify` command: resolve recording MBIDs from tags and AcoustID.
//!
//! Strategy:
//! 1. Read MBIDs from existing file tags (free, instant)
//! 2. For remaining unresolved files, query AcoustID API (rate-limited)
//! 3. Store results in the database for future use

use anyhow::{bail, Result};
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::HashSet;
use tracing::{debug, warn};

use crate::acoustid::AcoustIdClient;
use crate::database::Database;
use crate::matcher;
use crate::tags;

/// Run the identify workflow.
pub fn run(db: &Database, api_key: Option<&str>, dupes_only: bool, threshold: f64) -> Result<()> {
    // Determine which files need identification
    let files_to_identify = if dupes_only {
        // Only identify files that are in duplicate groups
        let groups = matcher::find_duplicates(db, threshold, false)?;
        let mut paths: HashSet<std::path::PathBuf> = HashSet::new();
        for group in &groups {
            for file in &group.files {
                paths.insert(file.path.clone());
            }
        }
        let unresolved = db.load_unresolved()?;
        unresolved
            .into_iter()
            .filter(|f| paths.contains(&f.path))
            .collect::<Vec<_>>()
    } else {
        db.load_unresolved()?
    };

    if files_to_identify.is_empty() {
        println!("All files already identified.");
        return Ok(());
    }

    println!("Identifying {} files...", files_to_identify.len());

    // Phase 1: Read MBIDs from file tags (free, no API calls)
    println!("Phase 1: Reading MBIDs from file tags...");
    let pb = ProgressBar::new(files_to_identify.len() as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template(
                "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta})",
            )
            .expect("valid template")
            .progress_chars("█▓░"),
    );

    let mut resolved_from_tags = 0;
    let mut still_unresolved = Vec::new();

    for file in &files_to_identify {
        match tags::read_recording_mbid(&file.path) {
            Ok(Some(mbid)) => {
                debug!("Tag MBID for {}: {}", file.path.display(), mbid);
                db.store_recording_mbid(&file.path, &mbid, None)?;
                resolved_from_tags += 1;
            }
            Ok(None) => {
                still_unresolved.push(file);
            }
            Err(e) => {
                debug!("Failed to read tags from {}: {}", file.path.display(), e);
                still_unresolved.push(file);
            }
        }
        pb.inc(1);
    }

    pb.finish_and_clear();
    println!(
        "  Resolved {} from tags, {} remaining",
        resolved_from_tags,
        still_unresolved.len()
    );

    // Phase 2: Query AcoustID for remaining files
    if still_unresolved.is_empty() {
        println!("All files resolved from tags!");
        return Ok(());
    }

    let api_key = match api_key {
        Some(key) => key.to_string(),
        None => {
            bail!(
                "AcoustID API key required for online lookup.\n\
                 Set ACOUSTID_API_KEY env var or pass --api-key.\n\
                 Register at: https://acoustid.org/new-application"
            );
        }
    };

    println!(
        "Phase 2: Querying AcoustID for {} files (rate-limited to 3 req/s)...",
        still_unresolved.len()
    );

    let eta_secs = still_unresolved.len() as f64 / 3.0;
    println!(
        "  Estimated time: {:.0}s ({:.1} minutes)",
        eta_secs,
        eta_secs / 60.0
    );

    let pb = ProgressBar::new(still_unresolved.len() as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}")
            .expect("valid template")
            .progress_chars("█▓░"),
    );

    let mut client = AcoustIdClient::new(api_key);
    let mut resolved_from_api = 0;
    let mut api_failures = 0;

    for file in &still_unresolved {
        pb.set_message(
            file.path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
        );

        match client.lookup(&file.fingerprint, file.duration_secs) {
            Ok(Some(result)) => {
                if let Some(ref mbid) = result.recording_mbid {
                    debug!(
                        "AcoustID resolved {}: {} (score: {:.2})",
                        file.path.display(),
                        mbid,
                        result.score
                    );
                    db.store_recording_mbid(&file.path, mbid, Some(&result.acoustid))?;
                    resolved_from_api += 1;
                } else {
                    // Got an AcoustID but no linked recording
                    debug!(
                        "AcoustID {} has no linked recording for {}",
                        result.acoustid,
                        file.path.display()
                    );
                }
            }
            Ok(None) => {
                debug!("No AcoustID match for {}", file.path.display());
            }
            Err(e) => {
                warn!("AcoustID lookup failed for {}: {}", file.path.display(), e);
                api_failures += 1;
            }
        }

        pb.inc(1);
    }

    pb.finish_and_clear();

    println!("\nResults:");
    println!("  Resolved from tags: {}", resolved_from_tags);
    println!("  Resolved from AcoustID: {}", resolved_from_api);
    println!("  API failures: {}", api_failures);
    println!(
        "  Total identified: {}/{}",
        resolved_from_tags + resolved_from_api,
        files_to_identify.len()
    );

    Ok(())
}
