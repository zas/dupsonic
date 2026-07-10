//! Integration tests for the full scan → find-dupes pipeline.
//!
//! These tests use pre-generated audio fixtures in tests/fixtures/classify/:
//! - hires.flac: 48kHz/24bit FLAC (10s, 440Hz sine)
//! - lowres.flac: 44.1kHz/16bit FLAC (10s, 440Hz sine)
//! - lossy.mp3: 128kbps MP3 (10s, 440Hz sine)
//! - retagged.flac: same audio as lowres, different tags
//! - exact_copy.flac: byte-identical copy of lowres
//! - chord.flac: 44.1kHz/16bit FLAC (10s, C major chord) — spectrally different
//! - short.flac: 44.1kHz/16bit FLAC (10s, 440Hz sine)
//! - long.flac: 44.1kHz/16bit FLAC (60s, 440Hz sine)

use std::path::{Path, PathBuf};
use tempfile::TempDir;

use dupsonic::database::Database;
use dupsonic::fingerprint::{compare_fingerprints, durations_compatible, fingerprint_file};
use dupsonic::matcher::{compute_band_hashes, find_duplicates};

/// Path to the test fixtures directory.
fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/classify")
}

#[test]
fn test_fingerprint_audio_file() {
    let path = fixtures_dir().join("lowres.flac");
    let result = fingerprint_file(&path, 120).unwrap();
    assert!(!result.fingerprint.is_empty());
    assert!(result.duration_secs > 9.0 && result.duration_secs < 11.0);
}

#[test]
fn test_identical_files_produce_identical_fingerprints() {
    let lowres = fixtures_dir().join("lowres.flac");
    let exact_copy = fixtures_dir().join("exact_copy.flac");

    let fp1 = fingerprint_file(&lowres, 120).unwrap();
    let fp2 = fingerprint_file(&exact_copy, 120).unwrap();

    let score = compare_fingerprints(&fp1.fingerprint, &fp2.fingerprint);
    assert_eq!(
        score, 1.0,
        "Identical audio should produce identical fingerprints"
    );
}

#[test]
fn test_different_frequencies_produce_different_fingerprints() {
    let sine = fixtures_dir().join("lowres.flac");
    let chord = fixtures_dir().join("chord.flac");

    let fp1 = fingerprint_file(&sine, 120).unwrap();
    let fp2 = fingerprint_file(&chord, 120).unwrap();

    let score = compare_fingerprints(&fp1.fingerprint, &fp2.fingerprint);
    assert!(
        score < 0.9,
        "Sine vs chord should score differently, got {score}"
    );
}

#[test]
fn test_same_audio_different_sample_rate() {
    let hires = fixtures_dir().join("hires.flac");
    let lowres = fixtures_dir().join("lowres.flac");

    let fp1 = fingerprint_file(&hires, 120).unwrap();
    let fp2 = fingerprint_file(&lowres, 120).unwrap();

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
    let short = fixtures_dir().join("short.flac");
    let long = fixtures_dir().join("long.flac");

    let fp_short = fingerprint_file(&short, 120).unwrap();
    let fp_long = fingerprint_file(&long, 120).unwrap();

    // Durations differ significantly — should be filtered out
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

    // 3 files with same audio (lowres, exact_copy, retagged) + 1 different (chord)
    let lowres = fixtures_dir().join("lowres.flac");
    let exact_copy = fixtures_dir().join("exact_copy.flac");
    let retagged = fixtures_dir().join("retagged.flac");
    let chord = fixtures_dir().join("chord.flac");

    for path in [&lowres, &exact_copy, &retagged, &chord] {
        let result = fingerprint_file(path, 120).unwrap();
        db.store_fingerprint(path, &result, 120).unwrap();
        let hashes = compute_band_hashes(&result.fingerprint);
        db.store_band_hashes(path, &hashes).unwrap();
    }

    let groups = find_duplicates(&db, 0.8, false).unwrap();

    // Should find exactly 1 group with the 3 duplicates
    assert_eq!(
        groups.len(),
        1,
        "Expected 1 duplicate group, got {}",
        groups.len()
    );
    assert_eq!(
        groups[0].files.len(),
        3,
        "Expected 3 files in group, got {}",
        groups[0].files.len()
    );

    // The chord file should NOT be in any group
    let all_paths: Vec<&Path> = groups
        .iter()
        .flat_map(|g| g.files.iter().map(|f| f.path.as_path()))
        .collect();
    assert!(!all_paths.contains(&chord.as_path()));
}

#[test]
fn test_pipeline_respects_duration_filter() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).unwrap();

    let short = fixtures_dir().join("short.flac");
    let long = fixtures_dir().join("long.flac");

    for path in [&short, &long] {
        let result = fingerprint_file(path, 120).unwrap();
        db.store_fingerprint(path, &result, 120).unwrap();
        let hashes = compute_band_hashes(&result.fingerprint);
        db.store_band_hashes(path, &hashes).unwrap();
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

    let path = fixtures_dir().join("lowres.flac");
    let result = fingerprint_file(&path, 120).unwrap();
    db.store_fingerprint(&path, &result, 120).unwrap();
    let hashes = compute_band_hashes(&result.fingerprint);
    db.store_band_hashes(&path, &hashes).unwrap();

    let groups = find_duplicates(&db, 0.8, false).unwrap();
    assert_eq!(groups.len(), 0, "Single file can't be a duplicate");
}

#[test]
fn test_incremental_scan_detection() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).unwrap();

    // Copy a fixture to a temp file so we can modify it
    let wav = dir.path().join("track.flac");
    std::fs::copy(fixtures_dir().join("lowres.flac"), &wav).unwrap();

    let result = fingerprint_file(&wav, 120).unwrap();
    db.store_fingerprint(&wav, &result, 120).unwrap();

    // File hasn't changed — should be detected as current
    assert!(db.is_current(&wav, 120).unwrap());

    // Modify the file (overwrite with different content)
    std::thread::sleep(std::time::Duration::from_millis(1100)); // ensure mtime changes
    std::fs::copy(fixtures_dir().join("chord.flac"), &wav).unwrap();

    // Now it should need re-scanning
    assert!(!db.is_current(&wav, 120).unwrap());
}

#[test]
fn test_duplicate_classification_5_files() {
    use dupsonic::matcher::{classify_matches, MatchKind};

    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).unwrap();

    // 5 files:
    // 1. hires.flac    - 48kHz/24bit FLAC (similar fingerprint, different quality)
    // 2. lowres.flac   - 44.1kHz/16bit FLAC
    // 3. lossy.mp3     - 128kbps MP3
    // 4. retagged.flac - same audio as lowres, different tags
    // 5. exact_copy.flac - byte-identical copy of lowres
    let hires = fixtures_dir().join("hires.flac");
    let lowres = fixtures_dir().join("lowres.flac");
    let lossy = fixtures_dir().join("lossy.mp3");
    let retagged = fixtures_dir().join("retagged.flac");
    let exact_copy = fixtures_dir().join("exact_copy.flac");

    // Fingerprint all files and store band hashes
    for path in [&hires, &lowres, &lossy, &retagged, &exact_copy] {
        let result = fingerprint_file(path, 120).unwrap();
        db.store_fingerprint(path, &result, 120).unwrap();
        let hashes = compute_band_hashes(&result.fingerprint);
        db.store_band_hashes(path, &hashes).unwrap();
    }

    // Find duplicates
    let mut groups = find_duplicates(&db, 0.8, false).unwrap();
    assert!(
        !groups.is_empty(),
        "Expected at least 1 duplicate group from 5 similar files"
    );

    // Classify matches (computes and caches hashes)
    classify_matches(&mut groups, &db);

    // Collect all files across all groups for inspection
    let all_files: Vec<_> = groups.iter().flat_map(|g| g.files.iter()).collect();

    // The exact copy should be in the results
    assert!(
        all_files.iter().any(|f| f.path == exact_copy),
        "Exact copy should appear in duplicate results"
    );

    // Verify group similarity is within valid range
    for group in &groups {
        assert!(
            group.similarity >= 0.8 && group.similarity <= 1.0,
            "Group similarity {} should be in [0.8, 1.0]",
            group.similarity
        );
    }

    // Find group(s) containing 100% matches (lowres, retagged, exact_copy)
    let perfect_groups: Vec<_> = groups.iter().filter(|g| g.similarity >= 1.0).collect();
    if !perfect_groups.is_empty() {
        let perfect_files: Vec<_> = perfect_groups.iter().flat_map(|g| g.files.iter()).collect();

        // exact_copy must be ExactCopy (byte-identical to lowres)
        if let Some(f) = perfect_files.iter().find(|f| f.path == exact_copy) {
            assert_eq!(
                f.match_kind,
                MatchKind::ExactCopy,
                "exact_copy.flac should be classified as ExactCopy"
            );
        }

        // lowres must be ExactCopy (byte-identical to exact_copy)
        if let Some(f) = perfect_files.iter().find(|f| f.path == lowres) {
            assert_eq!(
                f.match_kind,
                MatchKind::ExactCopy,
                "lowres.flac should be classified as ExactCopy (identical to exact_copy)"
            );
        }

        // retagged should be SameAudio (same audio stream, different tags)
        if let Some(f) = perfect_files.iter().find(|f| f.path == retagged) {
            assert_eq!(
                f.match_kind,
                MatchKind::SameAudio,
                "retagged.flac should be classified as SameAudio"
            );
        }
    }

    // Verify hashes are now cached in the DB
    let cached_hashes = db.load_hashes().unwrap();
    for path in [&lowres, &exact_copy, &retagged] {
        let entry = cached_hashes.get(path);
        assert!(
            entry.is_some() && entry.unwrap().0.is_some(),
            "Hash should be cached for {}",
            path.display()
        );
    }
}

#[test]
fn test_fingerprint_nonexistent_file_returns_open_failed() {
    use dupsonic::fingerprint::FingerprintError;

    let path = PathBuf::from("/nonexistent/path/to/audio.flac");
    let result = fingerprint_file(&path, 120);

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(err, FingerprintError::OpenFailed(_)),
        "Expected OpenFailed, got: {err:?}"
    );
    // Verify Display message is user-friendly
    let msg = err.to_string();
    assert!(
        msg.contains("could not open file"),
        "Error message should be user-friendly, got: {msg}"
    );
}

#[test]
fn test_fingerprint_non_audio_file_returns_unrecognized_format() {
    use dupsonic::fingerprint::FingerprintError;

    // Create a temporary file with garbage content but an audio extension
    let dir = TempDir::new().unwrap();
    let fake_audio = dir.path().join("not_audio.flac");
    std::fs::write(&fake_audio, b"This is not audio data at all").unwrap();

    let result = fingerprint_file(&fake_audio, 120);

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(err, FingerprintError::UnrecognizedFormat(_)),
        "Expected UnrecognizedFormat, got: {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("not a recognized audio format"),
        "Error message should be user-friendly, got: {msg}"
    );
}

#[test]
fn test_fingerprint_truncated_file_returns_error() {
    use dupsonic::fingerprint::FingerprintError;

    // Create a file with a valid FLAC magic number but truncated content
    let dir = TempDir::new().unwrap();
    let truncated = dir.path().join("truncated.flac");
    // fLaC magic followed by garbage — enough to start probing but not enough to decode
    let mut data = b"fLaC".to_vec();
    data.extend_from_slice(&[0u8; 100]);
    std::fs::write(&truncated, &data).unwrap();

    let result = fingerprint_file(&truncated, 120);

    assert!(result.is_err());
    let err = result.unwrap_err();
    // Could be UnrecognizedFormat or DecodeFailed depending on how far symphonia gets
    assert!(
        matches!(
            err,
            FingerprintError::UnrecognizedFormat(_)
                | FingerprintError::DecodeFailed(_)
                | FingerprintError::EmptyFingerprint
                | FingerprintError::NoAudioTrack
        ),
        "Expected a structured error for truncated file, got: {err:?}"
    );
    // Regardless of variant, Display should produce a useful message
    let msg = err.to_string();
    assert!(!msg.is_empty(), "Error message should not be empty");
}

#[test]
fn test_fingerprint_empty_file_returns_unrecognized_format() {
    use dupsonic::fingerprint::FingerprintError;

    let dir = TempDir::new().unwrap();
    let empty = dir.path().join("empty.mp3");
    std::fs::write(&empty, b"").unwrap();

    let result = fingerprint_file(&empty, 120);

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(err, FingerprintError::UnrecognizedFormat(_)),
        "Expected UnrecognizedFormat for empty file, got: {err:?}"
    );
}

#[test]
fn test_fingerprint_error_display_includes_details() {
    use dupsonic::fingerprint::FingerprintError;

    // Verify that each error variant produces a non-empty, descriptive message
    let errors = [
        FingerprintError::OpenFailed(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "permission denied",
        )),
        FingerprintError::UnrecognizedFormat("end of stream".to_string()),
        FingerprintError::NoAudioTrack,
        FingerprintError::DecodeFailed("unsupported codec".to_string()),
        FingerprintError::EmptyFingerprint,
    ];

    let expected_fragments = [
        "could not open file",
        "not a recognized audio format",
        "no audio track found",
        "unsupported or corrupted audio codec",
        "could not extract audio content",
    ];

    for (err, expected) in errors.iter().zip(expected_fragments.iter()) {
        let msg = err.to_string();
        assert!(
            msg.contains(expected),
            "FingerprintError::{:?} display should contain '{}', got: '{}'",
            err,
            expected,
            msg
        );
    }
}
