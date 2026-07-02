use anyhow::{Context, Result};
use chromaprint::{Algorithm, Fingerprinter};
use std::path::Path;
use symphonia::core::audio::{Audio, GenericAudioBufferRef};
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoderOptions};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, Track, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

/// Maximum duration to fingerprint (in seconds). Chromaprint only needs ~120s.
pub const DEFAULT_FINGERPRINT_DURATION_SECS: u64 = 120;

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
pub fn fingerprint_file(path: &Path, max_duration_secs: u64) -> Result<FingerprintResult> {
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

    let audio_params = get_audio_codec_params(&track)
        .ok_or_else(|| anyhow::anyhow!("No audio codec params in: {}", path.display()))?;

    let sample_rate = audio_params
        .sample_rate
        .ok_or_else(|| anyhow::anyhow!("Unknown sample rate: {}", path.display()))?;
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
        .with_context(|| format!("Failed to create decoder: {}", path.display()))?;

    let mut printer = Fingerprinter::new(Algorithm::default());
    printer
        .start(sample_rate, channels as u16)
        .map_err(|e| anyhow::anyhow!("Failed to start fingerprinter: {e}"))?;

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
            Err(e) => return Err(e.into()),
        };

        if packet.track_id != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(buf) => buf,
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
            Err(e) => return Err(e.into()),
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
        anyhow::bail!("Empty fingerprint for: {}", path.display());
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
