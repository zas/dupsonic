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
            if let Some(std_tag) = tag.std.as_ref() {
                // In file tags (Vorbis/ID3/MP4), MUSICBRAINZ_TRACKID has always been
                // the Recording MBID. This is true for all versions of Picard.
                // (The confusing rename is internal to Picard only, not in file tags.)
                // MUSICBRAINZ_RECORDINGID may also appear from other taggers.
                let id = match std_tag {
                    StandardTag::MusicBrainzTrackId(id) => Some(id),
                    StandardTag::MusicBrainzRecordingId(id) => Some(id),
                    _ => None,
                };
                if let Some(id) = id {
                    let id = id.trim();
                    if !id.is_empty() {
                        return Ok(Some(id.to_string()));
                    }
                }
            }
        }
    }

    Ok(None)
}

/// Basic tags extracted from a file.
#[derive(Debug, Clone)]
pub struct BasicTags {
    pub artist: Option<String>,
    pub title: Option<String>,
    pub album: Option<String>,
}

/// Audio format details for a file.
#[derive(Debug, Clone)]
pub struct AudioInfo {
    pub sample_rate: Option<u32>,
    pub bits_per_sample: Option<u32>,
    pub channels: Option<usize>,
    pub bitrate_kbps: Option<u32>,
}

/// Read audio format info (sample rate, bit depth, channels, bitrate) from a file.
pub fn read_audio_info(path: &Path) -> Result<Option<AudioInfo>> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Ok(None),
    };
    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let format = match symphonia::default::get_probe().probe(
        &hint,
        mss,
        FormatOptions::default(),
        MetadataOptions::default(),
    ) {
        Ok(f) => f,
        Err(_) => return Ok(None),
    };

    use symphonia::core::codecs::CodecParameters;
    use symphonia::core::formats::TrackType;

    let track = match format.default_track(TrackType::Audio) {
        Some(t) => t,
        None => return Ok(None),
    };

    let (sample_rate, bits_per_sample, channels) = match track.codec_params.as_ref() {
        Some(CodecParameters::Audio(params)) => (
            params.sample_rate,
            params.bits_per_sample,
            params.channels.as_ref().map(|c| c.count()),
        ),
        _ => (None, None, None),
    };

    // Compute bitrate from file size and duration for lossy formats
    let bitrate_kbps = if let (Some(tb), Some(duration)) = (track.time_base, track.duration) {
        let duration_secs = duration.get() as f64 * tb.numer.get() as f64 / tb.denom.get() as f64;
        if duration_secs > 0.0 && file_size > 0 {
            Some((file_size as f64 * 8.0 / duration_secs / 1000.0) as u32)
        } else {
            None
        }
    } else {
        None
    };

    Ok(Some(AudioInfo {
        sample_rate,
        bits_per_sample,
        channels,
        bitrate_kbps,
    }))
}

/// Read basic metadata tags (artist, title, album) from a file.
///
/// Returns `None` if the file cannot be read or has no tags.
pub fn read_basic_tags(path: &Path) -> Result<Option<BasicTags>> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Ok(None),
    };
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let mut format = match symphonia::default::get_probe().probe(
        &hint,
        mss,
        FormatOptions::default(),
        MetadataOptions::default(),
    ) {
        Ok(f) => f,
        Err(_) => return Ok(None),
    };

    let metadata = format.metadata();
    let revision = match metadata.current() {
        Some(r) => r,
        None => return Ok(None),
    };

    let mut artist = None;
    let mut title = None;
    let mut album = None;

    for tag in &revision.media.tags {
        if let Some(ref std_tag) = tag.std {
            match std_tag {
                StandardTag::Artist(v) => {
                    if artist.is_none() {
                        artist = Some(v.to_string());
                    }
                }
                StandardTag::TrackTitle(v) => {
                    title = Some(v.to_string());
                }
                StandardTag::Album(v) => {
                    album = Some(v.to_string());
                }
                _ => {}
            }
        }
    }

    if artist.is_none() && title.is_none() && album.is_none() {
        return Ok(None);
    }

    Ok(Some(BasicTags {
        artist,
        title,
        album,
    }))
}
