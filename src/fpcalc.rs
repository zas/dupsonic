//! Drop-in replacement for Chromaprint's `fpcalc` tool.
//!
//! Produces JSON output identical to `fpcalc -json -length 120 <file>`,
//! making dupsonic usable as a faster alternative to fpcalc in Picard.
//!
//! When the file's fingerprint is already in the database, returns it
//! instantly from cache. Otherwise computes it on the fly.

use anyhow::Result;
use std::path::Path;

use crate::acoustid::encode_fingerprint;
use crate::database::Database;
use crate::fingerprint::{fingerprint_file, FingerprintResult};

/// Run the fpcalc-compatible fingerprinting command.
///
/// Output format matches `fpcalc -json`:
/// ```json
/// {"duration": 172.84, "fingerprint": "AQADtEmkZJE..."}
/// ```
pub fn run(db: &Database, file: &Path, length: u64) -> Result<()> {
    // Try to use cached fingerprint from the database
    let result = if db.is_current(file, length)? {
        get_cached_fingerprint(db, file)
    } else {
        None
    };

    // If not cached, compute it
    let result = match result {
        Some(r) => r,
        None => {
            let r = fingerprint_file(file, length)?;
            // Cache it for future use
            let _ = db.store_fingerprint(file, &r, length);
            r
        }
    };

    // Output in fpcalc JSON format
    let encoded = encode_fingerprint(&result.fingerprint);
    let output = serde_json::json!({
        "duration": result.duration_secs,
        "fingerprint": encoded,
    });
    println!("{}", serde_json::to_string(&output)?);

    Ok(())
}

/// Try to retrieve a cached fingerprint from the database.
fn get_cached_fingerprint(db: &Database, file: &Path) -> Option<FingerprintResult> {
    let all = db.load_all_fingerprints().ok()?;
    all.into_iter()
        .find(|f| f.path == file)
        .map(|f| FingerprintResult {
            fingerprint: f.fingerprint,
            duration_secs: f.duration_secs,
        })
}
