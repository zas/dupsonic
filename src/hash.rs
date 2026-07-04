//! File and audio-stream hashing for duplicate classification.
//!
//! Provides two hash types:
//! - **File hash**: SHA-256 of the entire file (detects byte-identical copies)
//! - **Audio hash**: SHA-256 of the audio stream packets only, skipping
//!   container metadata and tags (detects same-audio-different-tags)

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

/// Compute SHA-256 hash of an entire file.
pub fn file_sha256(path: &Path) -> Result<String> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("Failed to open: {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Compute SHA-256 hash of the audio stream only (skipping metadata/tags).
///
/// Iterates over raw audio packets from the container without decoding.
/// Two files with identical audio but different tags will produce the same hash.
/// Two files with the same audio but different codecs (e.g., FLAC vs MP3) will differ.
pub fn audio_sha256(path: &Path) -> Result<String> {
    let file =
        std::fs::File::open(path).with_context(|| format!("Failed to open: {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .with_context(|| format!("Failed to probe format: {}", path.display()))?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| anyhow::anyhow!("No audio track found in: {}", path.display()))?
        .clone();
    let track_id = track.id;

    let mut hasher = Sha256::new();

    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(e) => return Err(e.into()),
        };

        if packet.track_id != track_id {
            continue;
        }

        // Hash the raw compressed audio data (not decoded)
        hasher.update(&packet.data);
    }

    Ok(format!("{:x}", hasher.finalize()))
}
