//! Read MusicBrainz recording IDs from audio file metadata tags.
//!
//! Supports ID3v2 (MP3), Vorbis comments (FLAC/OGG), and MP4/M4A tags
//! via symphonia's metadata reader.

use anyhow::{Context, Result};
use std::path::Path;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, RawValue, StandardTag};

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
            // Check standard tags (Vorbis/FLAC/MP4)
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

            // Check raw UFID frame for ID3v2 (MP3 files)
            // Picard stores the recording MBID as UFID with owner "http://musicbrainz.org"
            if tag.raw.key == "UFID" {
                // Check if the OWNER subfield is "http://musicbrainz.org"
                let is_musicbrainz = tag.raw.sub_fields.as_ref().is_some_and(|sfs| {
                    sfs.iter().any(|sf| {
                        sf.field == "OWNER"
                            && matches!(&sf.value, RawValue::String(s) if s.contains("musicbrainz.org"))
                    })
                });
                if is_musicbrainz {
                    // The UFID value is the MBID as ASCII bytes (may have trailing null)
                    if let RawValue::Binary(b) = &tag.raw.value {
                        let id = String::from_utf8_lossy(b)
                            .trim_end_matches('\0')
                            .trim()
                            .to_string();
                        if !id.is_empty() && id.len() == 36 {
                            return Ok(Some(id));
                        }
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
    /// Artist name (ID3: TPE1, Vorbis: ARTIST, MP4: ©ART).
    pub artist: Option<String>,
    /// Track title (ID3: TIT2, Vorbis: TITLE, MP4: ©nam).
    pub title: Option<String>,
    /// Album name (ID3: TALB, Vorbis: ALBUM, MP4: ©alb).
    pub album: Option<String>,
}

/// Audio format details for a file.
#[derive(Debug, Clone)]
pub struct AudioInfo {
    /// Sample rate in Hz (e.g. 44100, 48000, 96000).
    pub sample_rate: Option<u32>,
    /// Bits per sample (e.g. 16, 24, 32). `None` for lossy formats.
    pub bits_per_sample: Option<u32>,
    /// Number of audio channels.
    pub channels: Option<usize>,
    /// Average bitrate in kbps.
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
