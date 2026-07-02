use anyhow::{Context, Result};
use rusty_chromaprint::{Configuration, Fingerprinter};
use std::path::Path;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Maximum duration to fingerprint (in seconds). Chromaprint only needs ~120s.
const MAX_FINGERPRINT_DURATION_SECS: u64 = 120;

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
/// and rusty-chromaprint for fingerprint generation.
///
/// The fingerprint is generated from the first 120s of audio, but `duration_secs`
/// reports the full file duration (critical for deduplication since chromaprint
/// can't distinguish files that only differ after the 2-minute mark).
pub fn fingerprint_file(path: &Path) -> Result<FingerprintResult> {
    let file =
        std::fs::File::open(path).with_context(|| format!("Failed to open: {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .with_context(|| format!("Failed to probe format: {}", path.display()))?;

    let mut format = probed.format;

    let track = format
        .default_track()
        .ok_or_else(|| anyhow::anyhow!("No audio track found in: {}", path.display()))?;

    let sample_rate = track
        .codec_params
        .sample_rate
        .ok_or_else(|| anyhow::anyhow!("Unknown sample rate: {}", path.display()))?;
    let channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(2);
    let track_id = track.id;

    // Try to get duration from container metadata (most formats provide this)
    let full_duration_secs = get_track_duration(track);

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .with_context(|| format!("Failed to create decoder: {}", path.display()))?;

    let mut printer = Fingerprinter::new(&Configuration::preset_test2());
    printer
        .start(sample_rate, channels as u32)
        .map_err(|_| anyhow::anyhow!("Failed to start fingerprinter"))?;

    let max_samples = MAX_FINGERPRINT_DURATION_SECS * sample_rate as u64 * channels as u64;
    let mut total_samples: u64 = 0;
    let mut finished_fingerprinting = false;

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(e) => return Err(e.into()),
        };

        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(buf) => buf,
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
            Err(e) => return Err(e.into()),
        };

        if !finished_fingerprinting {
            let spec = *decoded.spec();
            let num_frames = decoded.frames();
            let mut sample_buf = SampleBuffer::<i16>::new(num_frames as u64, spec);
            sample_buf.copy_interleaved_ref(decoded);
            let samples = sample_buf.samples();

            printer.consume(samples);
            total_samples += samples.len() as u64;

            if total_samples >= max_samples {
                finished_fingerprinting = true;
                // If we already know the full duration from metadata, stop decoding
                if full_duration_secs.is_some() {
                    break;
                }
            }
        } else {
            // Past fingerprint window, just counting samples for duration
            let num_frames = decoded.frames();
            total_samples += (num_frames * channels) as u64;
        }
    }

    printer.finish();

    let fingerprint = printer.fingerprint().to_vec();
    if fingerprint.is_empty() {
        anyhow::bail!("Empty fingerprint for: {}", path.display());
    }

    // Use container-reported duration if available, otherwise use decoded sample count
    let duration_secs = full_duration_secs
        .unwrap_or_else(|| total_samples as f64 / (sample_rate as f64 * channels as f64));

    Ok(FingerprintResult {
        fingerprint,
        duration_secs,
    })
}

/// Extract track duration from symphonia's codec params / time base.
fn get_track_duration(track: &symphonia::core::formats::Track) -> Option<f64> {
    let n_frames = track.codec_params.n_frames?;
    if let Some(tb) = track.codec_params.time_base {
        let time = tb.calc_time(n_frames);
        Some(time.seconds as f64 + time.frac)
    } else {
        track
            .codec_params
            .sample_rate
            .map(|sr| n_frames as f64 / sr as f64)
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
        // Each sub-fingerprint has 16 matching bits out of 32
        let fp1 = vec![0x0000FFFF; 10];
        let fp2 = vec![0x00000000; 10];
        let score = compare_fingerprints(&fp1, &fp2);
        assert!((score - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_compare_different_lengths() {
        let fp1 = vec![0x12345678; 10];
        let fp2 = vec![0x12345678; 5];
        // Should compare only the overlapping portion (5 elements)
        assert_eq!(compare_fingerprints(&fp1, &fp2), 1.0);
    }

    #[test]
    fn test_durations_compatible_identical() {
        assert!(durations_compatible(180.0, 180.0));
    }

    #[test]
    fn test_durations_compatible_small_diff() {
        // MP3 padding typically adds ~0.05s
        assert!(durations_compatible(180.0, 180.5));
        assert!(durations_compatible(180.0, 182.0));
        assert!(durations_compatible(180.0, 183.0)); // within 3s absolute
    }

    #[test]
    fn test_durations_incompatible_large_diff() {
        // 3 min vs 7 min — clearly different tracks
        assert!(!durations_compatible(180.0, 420.0));
        // Same first 2 min but one file is much longer
        assert!(!durations_compatible(120.0, 300.0));
    }

    #[test]
    fn test_durations_compatible_short_tracks() {
        // Short tracks: 30s vs 33s — within 3s absolute tolerance
        assert!(durations_compatible(30.0, 33.0));
        // Short tracks: 30s vs 35s — exceeds 3s abs and 5% of 35 = 1.75s
        assert!(!durations_compatible(30.0, 35.0));
    }

    #[test]
    fn test_durations_compatible_unknown() {
        // If either duration is unknown (0), don't filter
        assert!(durations_compatible(0.0, 180.0));
        assert!(durations_compatible(180.0, 0.0));
    }

    #[test]
    fn test_durations_compatible_long_tracks() {
        // Long tracks: 10 min. 5% of 600 = 30s tolerance
        assert!(durations_compatible(600.0, 625.0));
        assert!(!durations_compatible(600.0, 650.0));
    }
}
