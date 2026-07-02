//! Integration tests for the full scan → find-dupes pipeline.
//!
//! These tests generate WAV audio fixtures programmatically (sine waves)
//! and verify that the fingerprinting + matching pipeline works correctly.

use std::io::Write;
use std::path::Path;
use tempfile::TempDir;

use dupsonic::database::Database;
use dupsonic::fingerprint::{
    compare_fingerprints, durations_compatible, fingerprint_file,
};
use dupsonic::matcher::find_duplicates;

/// Generate a WAV file containing a sine wave.
///
/// Parameters:
/// - freq: frequency in Hz
/// - duration_secs: length of audio
/// - sample_rate: samples per second
/// - path: where to write the file
fn generate_wav(freq: f64, duration_secs: f64, sample_rate: u32, path: &Path) {
    let num_samples = (sample_rate as f64 * duration_secs) as usize;
    let channels: u16 = 1;
    let bits_per_sample: u16 = 16;
    let byte_rate = sample_rate * channels as u32 * bits_per_sample as u32 / 8;
    let block_align = channels * bits_per_sample / 8;
    let data_size = (num_samples * channels as usize * (bits_per_sample / 8) as usize) as u32;

    let mut file = std::fs::File::create(path).unwrap();

    // RIFF header
    file.write_all(b"RIFF").unwrap();
    file.write_all(&(36 + data_size).to_le_bytes()).unwrap();
    file.write_all(b"WAVE").unwrap();

    // fmt chunk
    file.write_all(b"fmt ").unwrap();
    file.write_all(&16u32.to_le_bytes()).unwrap(); // chunk size
    file.write_all(&1u16.to_le_bytes()).unwrap(); // PCM format
    file.write_all(&channels.to_le_bytes()).unwrap();
    file.write_all(&sample_rate.to_le_bytes()).unwrap();
    file.write_all(&byte_rate.to_le_bytes()).unwrap();
    file.write_all(&block_align.to_le_bytes()).unwrap();
    file.write_all(&bits_per_sample.to_le_bytes()).unwrap();

    // data chunk
    file.write_all(b"data").unwrap();
    file.write_all(&data_size.to_le_bytes()).unwrap();

    // Generate samples
    for i in 0..num_samples {
        let t = i as f64 / sample_rate as f64;
        let sample = (t * freq * 2.0 * std::f64::consts::PI).sin();
        let sample_i16 = (sample * 32000.0) as i16;
        file.write_all(&sample_i16.to_le_bytes()).unwrap();
    }
}

/// Generate a WAV with pseudo-random noise-like audio (rapidly changing frequencies).
/// This creates audio that is spectrally very different from tonal/melodic content.
fn generate_noise_wav(duration_secs: f64, sample_rate: u32, path: &Path) {
    let num_samples = (sample_rate as f64 * duration_secs) as usize;
    let channels: u16 = 1;
    let bits_per_sample: u16 = 16;
    let byte_rate = sample_rate * channels as u32 * bits_per_sample as u32 / 8;
    let block_align = channels * bits_per_sample / 8;
    let data_size = (num_samples * channels as usize * (bits_per_sample / 8) as usize) as u32;

    let mut file = std::fs::File::create(path).unwrap();

    // RIFF header
    file.write_all(b"RIFF").unwrap();
    file.write_all(&(36 + data_size).to_le_bytes()).unwrap();
    file.write_all(b"WAVE").unwrap();

    // fmt chunk
    file.write_all(b"fmt ").unwrap();
    file.write_all(&16u32.to_le_bytes()).unwrap();
    file.write_all(&1u16.to_le_bytes()).unwrap();
    file.write_all(&channels.to_le_bytes()).unwrap();
    file.write_all(&sample_rate.to_le_bytes()).unwrap();
    file.write_all(&byte_rate.to_le_bytes()).unwrap();
    file.write_all(&block_align.to_le_bytes()).unwrap();
    file.write_all(&bits_per_sample.to_le_bytes()).unwrap();

    // data chunk
    file.write_all(b"data").unwrap();
    file.write_all(&data_size.to_le_bytes()).unwrap();

    // Generate pseudo-random noise using a simple LCG PRNG
    let mut rng_state: u64 = 0xDEADBEEF12345678;
    for _ in 0..num_samples {
        rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let sample_i16 = ((rng_state >> 33) as i32 - 16384) as i16;
        file.write_all(&sample_i16.to_le_bytes()).unwrap();
    }
}

/// Generate a WAV with multiple frequencies (chord) to create more complex audio.
fn generate_wav_chord(freqs: &[f64], duration_secs: f64, sample_rate: u32, path: &Path) {
    let num_samples = (sample_rate as f64 * duration_secs) as usize;
    let channels: u16 = 1;
    let bits_per_sample: u16 = 16;
    let byte_rate = sample_rate * channels as u32 * bits_per_sample as u32 / 8;
    let block_align = channels * bits_per_sample / 8;
    let data_size = (num_samples * channels as usize * (bits_per_sample / 8) as usize) as u32;

    let mut file = std::fs::File::create(path).unwrap();

    // RIFF header
    file.write_all(b"RIFF").unwrap();
    file.write_all(&(36 + data_size).to_le_bytes()).unwrap();
    file.write_all(b"WAVE").unwrap();

    // fmt chunk
    file.write_all(b"fmt ").unwrap();
    file.write_all(&16u32.to_le_bytes()).unwrap();
    file.write_all(&1u16.to_le_bytes()).unwrap();
    file.write_all(&channels.to_le_bytes()).unwrap();
    file.write_all(&sample_rate.to_le_bytes()).unwrap();
    file.write_all(&byte_rate.to_le_bytes()).unwrap();
    file.write_all(&block_align.to_le_bytes()).unwrap();
    file.write_all(&bits_per_sample.to_le_bytes()).unwrap();

    // data chunk
    file.write_all(b"data").unwrap();
    file.write_all(&data_size.to_le_bytes()).unwrap();

    let amplitude = 32000.0 / freqs.len() as f64;
    for i in 0..num_samples {
        let t = i as f64 / sample_rate as f64;
        let sample: f64 = freqs
            .iter()
            .map(|f| (t * f * 2.0 * std::f64::consts::PI).sin())
            .sum();
        let sample_i16 = (sample * amplitude) as i16;
        file.write_all(&sample_i16.to_le_bytes()).unwrap();
    }
}

#[test]
fn test_fingerprint_wav_file() {
    let dir = TempDir::new().unwrap();
    let wav_path = dir.path().join("test.wav");
    generate_wav(440.0, 10.0, 44100, &wav_path);

    let result = fingerprint_file(&wav_path).unwrap();
    assert!(!result.fingerprint.is_empty());
    assert!(result.duration_secs > 9.0 && result.duration_secs < 11.0);
}

#[test]
fn test_identical_files_produce_identical_fingerprints() {
    let dir = TempDir::new().unwrap();
    let wav1 = dir.path().join("a.wav");
    let wav2 = dir.path().join("b.wav");

    // Same audio written to two different files
    generate_wav(440.0, 15.0, 44100, &wav1);
    generate_wav(440.0, 15.0, 44100, &wav2);

    let fp1 = fingerprint_file(&wav1).unwrap();
    let fp2 = fingerprint_file(&wav2).unwrap();

    let score = compare_fingerprints(&fp1.fingerprint, &fp2.fingerprint);
    assert_eq!(score, 1.0, "Identical audio should produce identical fingerprints");
}

#[test]
fn test_different_frequencies_produce_different_fingerprints() {
    let dir = TempDir::new().unwrap();
    let wav1 = dir.path().join("melody.wav");
    let wav2 = dir.path().join("noise.wav");

    // Generate a melodic pattern (changing frequencies over time)
    generate_wav_chord(&[261.6, 329.6, 392.0], 15.0, 44100, &wav1);
    // Generate pseudo-random noise-like pattern (many rapid frequency changes)
    generate_noise_wav(15.0, 44100, dir.path().join("noise.wav").as_path());

    let fp1 = fingerprint_file(&wav1).unwrap();
    let fp2 = fingerprint_file(&wav2).unwrap();

    let score = compare_fingerprints(&fp1.fingerprint, &fp2.fingerprint);
    // Chromaprint uses coarse spectral features - structured audio vs noise should differ
    assert!(
        score < 0.9,
        "Melodic audio vs noise should score differently, got {score}"
    );
}

#[test]
fn test_same_audio_different_sample_rate() {
    let dir = TempDir::new().unwrap();
    let wav_44k = dir.path().join("44k.wav");
    let wav_22k = dir.path().join("22k.wav");

    // Same frequency but different sample rates
    // Note: chromaprint resamples internally, so these should still be similar
    generate_wav(440.0, 15.0, 44100, &wav_44k);
    generate_wav(440.0, 15.0, 22050, &wav_22k);

    let fp1 = fingerprint_file(&wav_44k).unwrap();
    let fp2 = fingerprint_file(&wav_22k).unwrap();

    let score = compare_fingerprints(&fp1.fingerprint, &fp2.fingerprint);
    // Same audio at different sample rates should still be very similar after chromaprint's
    // internal resampling to 11025 Hz
    assert!(
        score > 0.7,
        "Same audio at different sample rates should be similar, got {score}"
    );
}

#[test]
fn test_duration_filter_catches_different_length_tracks() {
    // A 30-second track and a 5-minute track with same start should NOT be duplicates
    let dir = TempDir::new().unwrap();
    let short = dir.path().join("short.wav");
    let long = dir.path().join("long.wav");

    generate_wav(440.0, 30.0, 44100, &short);
    generate_wav(440.0, 300.0, 44100, &long);

    let fp_short = fingerprint_file(&short).unwrap();
    let fp_long = fingerprint_file(&long).unwrap();

    // Fingerprints of the overlapping portion would be similar, but durations differ
    assert!(!durations_compatible(
        fp_short.duration_secs,
        fp_long.duration_secs
    ));
}

#[test]
fn test_full_pipeline_finds_duplicates() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).unwrap();

    // Create 3 "duplicate" files (same audio) and 1 different file
    let dup1 = dir.path().join("dup1.wav");
    let dup2 = dir.path().join("dup2.wav");
    let dup3 = dir.path().join("dup3.wav");
    let diff = dir.path().join("different.wav");

    generate_wav(440.0, 20.0, 44100, &dup1);
    generate_wav(440.0, 20.0, 44100, &dup2);
    generate_wav(440.0, 20.0, 44100, &dup3);
    generate_wav_chord(&[261.6, 329.6, 392.0], 20.0, 44100, &diff); // C major chord

    // Fingerprint all files
    for path in [&dup1, &dup2, &dup3, &diff] {
        let result = fingerprint_file(path).unwrap();
        db.store_fingerprint(path, &result).unwrap();
    }

    // Find duplicates
    let groups = find_duplicates(&db, 0.8, false).unwrap();

    // Should find exactly 1 group with the 3 duplicates
    assert_eq!(groups.len(), 1, "Expected 1 duplicate group, got {}", groups.len());
    assert_eq!(
        groups[0].files.len(),
        3,
        "Expected 3 files in group, got {}",
        groups[0].files.len()
    );

    // The "different" file should NOT be in any group
    let all_paths: Vec<&Path> = groups
        .iter()
        .flat_map(|g| g.files.iter().map(|f| f.path.as_path()))
        .collect();
    assert!(!all_paths.contains(&diff.as_path()));
}

#[test]
fn test_pipeline_respects_duration_filter() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).unwrap();

    // Same frequency but very different durations — should NOT be grouped
    let short = dir.path().join("short.wav");
    let long = dir.path().join("long.wav");

    generate_wav(440.0, 10.0, 44100, &short);
    generate_wav(440.0, 60.0, 44100, &long);

    for path in [&short, &long] {
        let result = fingerprint_file(path).unwrap();
        db.store_fingerprint(path, &result).unwrap();
    }

    let groups = find_duplicates(&db, 0.8, false).unwrap();
    assert_eq!(
        groups.len(),
        0,
        "Files with very different durations should not be grouped as duplicates"
    );
}

#[test]
fn test_pipeline_no_files() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).unwrap();

    let groups = find_duplicates(&db, 0.8, false).unwrap();
    assert_eq!(groups.len(), 0);
}

#[test]
fn test_pipeline_single_file() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).unwrap();

    let wav = dir.path().join("only.wav");
    generate_wav(440.0, 15.0, 44100, &wav);
    let result = fingerprint_file(&wav).unwrap();
    db.store_fingerprint(&wav, &result).unwrap();

    let groups = find_duplicates(&db, 0.8, false).unwrap();
    assert_eq!(groups.len(), 0, "Single file can't be a duplicate");
}

#[test]
fn test_incremental_scan_detection() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).unwrap();

    let wav = dir.path().join("track.wav");
    generate_wav(440.0, 10.0, 44100, &wav);

    let result = fingerprint_file(&wav).unwrap();
    db.store_fingerprint(&wav, &result).unwrap();

    // File hasn't changed — should be detected as current
    assert!(db.is_current(&wav).unwrap());

    // Modify the file
    std::thread::sleep(std::time::Duration::from_millis(1100)); // ensure mtime changes
    generate_wav(880.0, 10.0, 44100, &wav);

    // Now it should need re-scanning
    assert!(!db.is_current(&wav).unwrap());
}
