//! AcoustID API client with rate limiting.
//!
//! Submits chromaprint fingerprints to the AcoustID lookup API and returns
//! MusicBrainz Recording IDs.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, warn};

const ACOUSTID_API_URL: &str = "https://api.acoustid.org/v2/lookup";

/// AcoustID API response structures.
#[derive(Debug, Deserialize)]
struct AcoustIdResponse {
    status: String,
    results: Option<Vec<AcoustIdResult>>,
    error: Option<AcoustIdError>,
}

#[derive(Debug, Deserialize)]
struct AcoustIdError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct AcoustIdResult {
    id: String,
    score: f64,
    recordings: Option<Vec<AcoustIdRecording>>,
}

#[derive(Debug, Deserialize)]
struct AcoustIdRecording {
    id: String,
}

/// Result of an AcoustID lookup for a single fingerprint.
#[derive(Debug, Clone)]
pub struct LookupResult {
    /// The AcoustID UUID
    pub acoustid: String,
    /// The MusicBrainz Recording ID (if resolved)
    pub recording_mbid: Option<String>,
    /// Confidence score (0.0 to 1.0)
    pub score: f64,
}

/// Rate-limited AcoustID API client.
pub struct AcoustIdClient {
    api_key: String,
    /// Minimum interval between requests (enforces rate limit)
    min_interval: Duration,
    /// Timestamp of last request
    last_request: Option<Instant>,
}

impl AcoustIdClient {
    /// Create a new client with the given API key.
    /// Rate limit: max 3 requests/second.
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            min_interval: Duration::from_millis(334), // ~3 req/s
            last_request: None,
        }
    }

    /// Look up a fingerprint against the AcoustID database.
    ///
    /// Returns the best matching result, or None if no match found.
    pub fn lookup(
        &mut self,
        fingerprint: &[u32],
        duration_secs: f64,
    ) -> Result<Option<LookupResult>> {
        self.rate_limit();

        let encoded_fp = encode_fingerprint(fingerprint);
        let duration = duration_secs as u32;

        debug!(
            "AcoustID lookup: duration={}s, fp_len={}",
            duration,
            fingerprint.len()
        );

        let response: AcoustIdResponse = ureq::get(ACOUSTID_API_URL)
            .query_pairs([
                ("client", self.api_key.as_str()),
                ("meta", "recordings"),
                ("duration", &duration.to_string()),
                ("fingerprint", &encoded_fp),
            ])
            .call()
            .context("AcoustID API request failed")?
            .body_mut()
            .read_json()
            .context("Failed to parse AcoustID response")?;

        self.last_request = Some(Instant::now());

        if response.status != "ok" {
            let msg = response
                .error
                .map(|e| e.message)
                .unwrap_or_else(|| "unknown error".to_string());
            warn!("AcoustID error: {}", msg);
            return Ok(None);
        }

        let results = response.results.unwrap_or_default();

        // Find the best result with a recording MBID
        for result in &results {
            if result.score < 0.5 {
                continue; // Skip low-confidence matches
            }

            let recording_mbid = result
                .recordings
                .as_ref()
                .and_then(|recs| recs.first())
                .map(|r| r.id.clone());

            return Ok(Some(LookupResult {
                acoustid: result.id.clone(),
                recording_mbid,
                score: result.score,
            }));
        }

        Ok(None)
    }

    /// Enforce rate limiting by sleeping if necessary.
    fn rate_limit(&self) {
        if let Some(last) = self.last_request {
            let elapsed = last.elapsed();
            if elapsed < self.min_interval {
                thread::sleep(self.min_interval - elapsed);
            }
        }
    }
}

/// Encode a raw fingerprint (Vec<u32>) to the compressed+base64 format
/// expected by the AcoustID API.
fn encode_fingerprint(fingerprint: &[u32]) -> String {
    use rusty_chromaprint::{Configuration, FingerprintCompressor};

    let config = Configuration::preset_test2();
    let compressor = FingerprintCompressor::from(&config);
    let compressed = compressor.compress(fingerprint);

    // AcoustID API expects standard base64 encoding of the compressed fingerprint.
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(&compressed)
}
