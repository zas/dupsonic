//! Audio decoding and Chromaprint fingerprint generation.
//!
//! Uses [Symphonia](https://docs.rs/symphonia) for format-agnostic audio decoding
//! and [chromaprint-next](https://docs.rs/chromaprint-next) for fingerprint generation.
//! The fingerprints are bit-identical to those produced by the reference `fpcalc` tool.

use chromaprint::{Algorithm, Fingerprinter};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use symphonia::core::audio::{Audio, GenericAudioBufferRef};
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoderOptions};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, Track, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;

/// Maximum duration to fingerprint (in seconds). Chromaprint only needs ~120s.
pub const DEFAULT_FINGERPRINT_DURATION_SECS: u64 = 120;

/// Categorized reasons why fingerprinting can fail.
#[derive(Debug)]
pub enum FingerprintError {
    /// File could not be opened (permissions, missing, locked).
    OpenFailed(std::io::Error),
    /// Audio format not recognized or file too short/corrupted to probe.
    UnrecognizedFormat(String),
    /// No audio track found in the container (e.g. video-only file).
    NoAudioTrack,
    /// Codec is unsupported or audio data is corrupted.
    DecodeFailed(String),
    /// Fingerprint was empty (file may be silence or too short).
    EmptyFingerprint,
}

impl std::fmt::Display for FingerprintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OpenFailed(e) => write!(f, "could not open file ({e})"),
            Self::UnrecognizedFormat(detail) => write!(
                f,
                "not a recognized audio format or file is too short/corrupted ({detail})"
            ),
            Self::NoAudioTrack => write!(f, "no audio track found in file"),
            Self::DecodeFailed(detail) => {
                write!(f, "unsupported or corrupted audio codec ({detail})")
            }
            Self::EmptyFingerprint => write!(
                f,
                "could not extract audio content (file may be silence or too short)"
            ),
        }
    }
}

impl std::error::Error for FingerprintError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::OpenFailed(e) => Some(e),
            _ => None,
        }
    }
}

/// Result of fingerprinting a single audio file.
#[derive(Debug, Clone)]
pub struct FingerprintResult {
    /// Raw fingerprint as a vector of u32 sub-fingerprints
    pub fingerprint: Vec<u32>,
    /// Full duration of the audio file in seconds (not capped at 120s)
    pub duration_secs: f64,
}

/// Generate an acoustic fingerprint for an audio file.
///
/// Uses symphonia for decoding (supports mp3, ogg, flac, wav, aac, etc.)
/// and chromaprint-next for fingerprint generation (bit-identical to fpcalc).
///
/// The fingerprint is generated from the first `max_duration_secs` of audio,
/// but `duration_secs` reports the full file duration (critical for deduplication
/// since chromaprint can't distinguish files that only differ after the fingerprinted portion).
pub fn fingerprint_file(
    path: &Path,
    max_duration_secs: u64,
) -> Result<FingerprintResult, FingerprintError> {
    let file = std::fs::File::open(path).map_err(FingerprintError::OpenFailed)?;
    let file_len = file.metadata().map_err(FingerprintError::OpenFailed)?.len();

    // Some MP4/M4A files are truncated (an interrupted download or copy): the
    // `mdat` box header still advertises its original size, which now runs past
    // the real end of the file. Symphonia's isomp4 demuxer discards such an
    // mdat wholesale and then fails with "no atom pending read", even though the
    // audio that IS present decodes fine. Detect this and rewrite the mdat size
    // field to the bytes on disk so the available audio can still be
    // fingerprinted (symphonia then hits a clean EOF at the truncation point).
    let patches = mdat_size_patches(path, file_len);
    let source: Box<dyn MediaSource> = if patches.is_empty() {
        Box::new(file)
    } else {
        Box::new(PatchedSource {
            inner: file,
            len: file_len,
            patches,
        })
    };
    let mss = MediaSourceStream::new(source, Default::default());

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
        .map_err(|e| FingerprintError::UnrecognizedFormat(e.to_string()))?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or(FingerprintError::NoAudioTrack)?
        .clone();

    let audio_params = get_audio_codec_params(&track)
        .ok_or_else(|| FingerprintError::DecodeFailed("no audio codec params".to_string()))?;

    let sample_rate = audio_params
        .sample_rate
        .ok_or_else(|| FingerprintError::DecodeFailed("unknown sample rate".to_string()))?;
    let channels = audio_params
        .channels
        .as_ref()
        .map(|c| c.count())
        .unwrap_or(2);
    let track_id = track.id;

    // Try to get duration from container metadata (most formats provide this)
    let full_duration_secs = get_track_duration(&track, sample_rate);

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
        .map_err(|e| FingerprintError::DecodeFailed(e.to_string()))?;

    let mut printer = Fingerprinter::new(Algorithm::default());
    printer
        .start(sample_rate, channels as u16)
        .map_err(|e| FingerprintError::DecodeFailed(format!("fingerprinter start: {e}")))?;

    let max_samples = max_duration_secs * sample_rate as u64 * channels as u64;
    let mut total_samples: u64 = 0;
    let mut finished_fingerprinting = false;

    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            // A demuxer-level malformed-stream error (e.g. symphonia's isomp4
            // "no atom pending read" on some iTunes-authored .m4a files) means
            // the container can't be advanced further. Rather than discarding
            // the whole file, stop reading and fingerprint whatever audio we've
            // already decoded — this is usually the full track, since the error
            // tends to fire on a trailing/unexpected atom after the last packet.
            Err(symphonia::core::errors::Error::DecodeError(_)) => break,
            Err(e) => return Err(FingerprintError::DecodeFailed(e.to_string())),
        };

        if packet.track_id != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(buf) => buf,
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
            Err(e) => return Err(FingerprintError::DecodeFailed(e.to_string())),
        };

        if !finished_fingerprinting {
            let num_frames = generic_buf_frames(&decoded);

            // Skip empty frames
            if num_frames == 0 {
                continue;
            }

            // Convert decoded audio to interleaved i16 samples.
            // Use catch_unwind as safety net for any panics on malformed data.
            let samples_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                generic_buf_to_i16_interleaved(&decoded)
            }));

            let samples = match samples_result {
                Ok(s) => s,
                Err(_) => continue, // Skip this packet
            };

            printer.feed(&samples).ok();
            total_samples += samples.len() as u64;

            if total_samples >= max_samples {
                finished_fingerprinting = true;
                if full_duration_secs.is_some() {
                    break;
                }
            }
        } else {
            // Past fingerprint window, just counting samples for duration
            let num_frames = generic_buf_frames(&decoded);
            total_samples += (num_frames * channels) as u64;
        }
    }

    printer.finish().ok();

    let fingerprint = printer.fingerprint().to_vec();
    if fingerprint.is_empty() {
        return Err(FingerprintError::EmptyFingerprint);
    }

    let duration_secs = full_duration_secs
        .unwrap_or_else(|| total_samples as f64 / (sample_rate as f64 * channels as f64));

    Ok(FingerprintResult {
        fingerprint,
        duration_secs,
    })
}

/// Extract AudioCodecParameters from a track.
fn get_audio_codec_params(track: &Track) -> Option<AudioCodecParameters> {
    match track.codec_params.as_ref()? {
        CodecParameters::Audio(params) => Some(params.clone()),
        _ => None,
    }
}

/// Extract track duration from container metadata.
fn get_track_duration(track: &Track, sample_rate: u32) -> Option<f64> {
    if let (Some(tb), Some(duration)) = (track.time_base, track.duration) {
        // duration is in timebase units; convert to seconds: duration * (numer / denom)
        let secs = duration.get() as f64 * tb.numer.get() as f64 / tb.denom.get() as f64;
        Some(secs)
    } else {
        track.num_frames.map(|n| n as f64 / sample_rate as f64)
    }
}

/// MP4/M4A extensions that route to symphonia's isomp4 demuxer and can carry a
/// truncated `mdat`. Raw AAC (`.aac`) is ADTS, not an ISO-BMFF container.
const MP4_EXTENSIONS: &[&str] = &["m4a", "m4b", "m4p", "mp4", "mov"];

fn is_mp4_container(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| MP4_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Scan the top-level atoms of an MP4/M4A file for an `mdat` box whose declared
/// size overruns the end of the file, and return the byte patches that rewrite
/// its size field to the number of bytes actually present.
///
/// Returns an empty vec for well-formed files (the common case), so the caller
/// decodes the original file untouched. This only walks the small top-level
/// atom headers, not the media data.
fn mdat_size_patches(path: &Path, file_len: u64) -> Vec<(u64, u8)> {
    if !is_mp4_container(path) {
        return Vec::new();
    }

    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    let mut pos: u64 = 0;
    let mut hdr = [0u8; 16];
    while pos + 8 <= file_len {
        if f.seek(SeekFrom::Start(pos)).is_err() || f.read_exact(&mut hdr[..8]).is_err() {
            break;
        }
        let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
        let atom_type = [hdr[4], hdr[5], hdr[6], hdr[7]];

        // Resolve the atom size and where/how wide its size field is.
        let (atom_size, size_off, size_is_64) = if size32 == 1 {
            // 64-bit size stored immediately after the type field.
            if f.read_exact(&mut hdr[8..16]).is_err() {
                break;
            }
            let size64 = u64::from_be_bytes(hdr[8..16].try_into().unwrap());
            (size64, pos + 8, true)
        } else if size32 == 0 {
            // Size 0 already means "extends to end of file", and nothing can
            // follow it — no repair needed or possible.
            break;
        } else {
            (size32 as u64, pos, false)
        };

        if atom_size < 8 {
            break; // malformed header; give up rather than loop
        }

        let atom_end = pos.saturating_add(atom_size);

        if atom_type == *b"mdat" && atom_end > file_len {
            // Rewrite the size field so the box ends exactly at EOF.
            let corrected = file_len - pos;
            let bytes_at = |slice: &[u8]| -> Vec<(u64, u8)> {
                slice
                    .iter()
                    .enumerate()
                    .map(|(i, &b)| (size_off + i as u64, b))
                    .collect()
            };
            return if size_is_64 {
                bytes_at(&corrected.to_be_bytes())
            } else if corrected <= u32::MAX as u64 {
                bytes_at(&(corrected as u32).to_be_bytes())
            } else {
                Vec::new() // corrected size won't fit a 32-bit field
            };
        }

        pos = atom_end;
    }

    Vec::new()
}

/// A [`MediaSource`] that overlays a handful of single-byte patches onto a file
/// as it is read, used to repair a truncated `mdat` box size (see
/// [`mdat_size_patches`]). All other bytes pass through unchanged.
struct PatchedSource {
    inner: std::fs::File,
    len: u64,
    /// (absolute file offset, replacement byte).
    patches: Vec<(u64, u8)>,
}

impl Read for PatchedSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let start = self.inner.stream_position()?;
        let n = self.inner.read(buf)?;
        let end = start + n as u64;
        for &(off, val) in &self.patches {
            if off >= start && off < end {
                buf[(off - start) as usize] = val;
            }
        }
        Ok(n)
    }
}

impl Seek for PatchedSource {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

impl MediaSource for PatchedSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}

/// Get the number of frames from a GenericAudioBufferRef.
fn generic_buf_frames(buf: &GenericAudioBufferRef<'_>) -> usize {
    match buf {
        GenericAudioBufferRef::U8(b) => b.frames(),
        GenericAudioBufferRef::U16(b) => b.frames(),
        GenericAudioBufferRef::U24(b) => b.frames(),
        GenericAudioBufferRef::U32(b) => b.frames(),
        GenericAudioBufferRef::S8(b) => b.frames(),
        GenericAudioBufferRef::S16(b) => b.frames(),
        GenericAudioBufferRef::S24(b) => b.frames(),
        GenericAudioBufferRef::S32(b) => b.frames(),
        GenericAudioBufferRef::F32(b) => b.frames(),
        GenericAudioBufferRef::F64(b) => b.frames(),
    }
}

/// Convert any GenericAudioBufferRef to interleaved i16 samples.
fn generic_buf_to_i16_interleaved(buf: &GenericAudioBufferRef<'_>) -> Vec<i16> {
    macro_rules! convert {
        ($b:expr) => {{
            let mut out = vec![0i16; $b.frames() * $b.num_planes()];
            $b.copy_to_slice_interleaved(&mut out);
            out
        }};
    }

    match buf {
        GenericAudioBufferRef::U8(b) => convert!(b),
        GenericAudioBufferRef::U16(b) => convert!(b),
        GenericAudioBufferRef::U24(b) => convert!(b),
        GenericAudioBufferRef::U32(b) => convert!(b),
        GenericAudioBufferRef::S8(b) => convert!(b),
        GenericAudioBufferRef::S16(b) => convert!(b),
        GenericAudioBufferRef::S24(b) => convert!(b),
        GenericAudioBufferRef::S32(b) => convert!(b),
        GenericAudioBufferRef::F32(b) => convert!(b),
        GenericAudioBufferRef::F64(b) => convert!(b),
    }
}

/// Compare two fingerprints and return a bit-error-rate similarity score (0.0 to 1.0).
///
/// This is the core comparison used after LSH candidate selection.
/// It computes (matching bits) / (total bits) across the overlapping sub-fingerprints.
pub fn compare_fingerprints(fp1: &[u32], fp2: &[u32]) -> f64 {
    if fp1.is_empty() || fp2.is_empty() {
        return 0.0;
    }

    let len = fp1.len().min(fp2.len());
    if len == 0 {
        return 0.0;
    }

    let total_bits = len as f64 * 32.0;
    let matching_bits: u32 = fp1[..len]
        .iter()
        .zip(&fp2[..len])
        .map(|(a, b)| 32 - (a ^ b).count_ones())
        .sum();

    matching_bits as f64 / total_bits
}

/// Check if two durations are compatible for duplicate detection.
///
/// Chromaprint only considers the first ~120s of audio, so two files with identical
/// first 2 minutes but different total lengths should NOT be duplicates.
/// We allow a small tolerance for codec padding differences.
pub fn durations_compatible(dur1: f64, dur2: f64) -> bool {
    if dur1 <= 0.0 || dur2 <= 0.0 {
        return true; // Can't filter if we don't know duration
    }

    let diff = (dur1 - dur2).abs();
    let max_dur = dur1.max(dur2);

    // Allow up to 3 seconds absolute difference OR 5% relative difference,
    // whichever is larger. This accounts for:
    // - Codec frame padding (MP3 encoder delay/padding)
    // - Container overhead differences
    // - Slightly different encode settings
    let abs_tolerance: f64 = 3.0;
    let rel_tolerance = max_dur * 0.05;
    let tolerance = abs_tolerance.max(rel_tolerance);

    diff <= tolerance
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compare_identical_fingerprints() {
        let fp = vec![0x12345678, 0xABCDEF01, 0x00FF00FF, 0xDEADBEEF];
        assert_eq!(compare_fingerprints(&fp, &fp), 1.0);
    }

    #[test]
    fn test_compare_empty_fingerprints() {
        assert_eq!(compare_fingerprints(&[], &[1, 2, 3]), 0.0);
        assert_eq!(compare_fingerprints(&[1, 2, 3], &[]), 0.0);
        assert_eq!(compare_fingerprints(&[], &[]), 0.0);
    }

    #[test]
    fn test_compare_completely_different() {
        let fp1 = vec![0x00000000; 100];
        let fp2 = vec![0xFFFFFFFF; 100];
        assert_eq!(compare_fingerprints(&fp1, &fp2), 0.0);
    }

    #[test]
    fn test_compare_half_matching() {
        let fp1 = vec![0x0000FFFF; 10];
        let fp2 = vec![0x00000000; 10];
        let score = compare_fingerprints(&fp1, &fp2);
        assert!((score - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_compare_different_lengths() {
        let fp1 = vec![0x12345678; 10];
        let fp2 = vec![0x12345678; 5];
        assert_eq!(compare_fingerprints(&fp1, &fp2), 1.0);
    }

    #[test]
    fn test_durations_compatible_identical() {
        assert!(durations_compatible(180.0, 180.0));
    }

    #[test]
    fn test_durations_compatible_small_diff() {
        assert!(durations_compatible(180.0, 180.5));
        assert!(durations_compatible(180.0, 182.0));
        assert!(durations_compatible(180.0, 183.0));
    }

    #[test]
    fn test_durations_incompatible_large_diff() {
        assert!(!durations_compatible(180.0, 420.0));
        assert!(!durations_compatible(120.0, 300.0));
    }

    #[test]
    fn test_durations_compatible_short_tracks() {
        assert!(durations_compatible(30.0, 33.0));
        assert!(!durations_compatible(30.0, 35.0));
    }

    #[test]
    fn test_durations_compatible_unknown() {
        assert!(durations_compatible(0.0, 180.0));
        assert!(durations_compatible(180.0, 0.0));
    }

    #[test]
    fn test_durations_compatible_long_tracks() {
        assert!(durations_compatible(600.0, 625.0));
        assert!(!durations_compatible(600.0, 650.0));
    }
}
