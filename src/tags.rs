//! Read MusicBrainz recording IDs from audio file metadata tags.
//!
//! Supports ID3v2 (MP3), Vorbis comments (FLAC/OGG), and MP4/M4A tags
//! via symphonia's metadata reader.

use anyhow::{Context, Result};
use std::path::Path;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTag};

/// Extract the MusicBrainz Recording ID from a file's metadata tags.
///
/// Returns `None` if the file has no MBID tag or cannot be read.
pub fn read_recording_mbid(path: &Path) -> Result<Option<String>> {
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
        .with_context(|| format!("Failed to probe: {}", path.display()))?;

    // Check metadata from the format reader
    let metadata = format.metadata();
    if let Some(revision) = metadata.current() {
        for tag in &revision.media.tags {
            if let Some(StandardTag::MusicBrainzRecordingId(id)) = tag.std.as_ref() {
                let id = id.trim();
                if !id.is_empty() {
                    return Ok(Some(id.to_string()));
                }
            }
        }
    }

    Ok(None)
}
