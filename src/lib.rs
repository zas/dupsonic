//! Find duplicate audio files by how they sound, not by filename or tags.
//!
//! dupsonic uses acoustic fingerprinting ([Chromaprint](https://acoustid.org/chromaprint))
//! to detect duplicates regardless of format, bitrate, or metadata — the same MP3 and FLAC
//! of a track will be matched, even if their tags differ completely.
//!
//! # Architecture
//!
//! The library is organized around a pipeline:
//!
//! 1. [`scanner`] — discovers audio files and orchestrates parallel fingerprinting
//! 2. [`fingerprint`] — decodes audio and generates Chromaprint fingerprints
//! 3. [`database`] — persists fingerprints in SQLite with change detection
//! 4. [`matcher`] — finds duplicate groups via LSH banding + fingerprint comparison
//! 5. [`output`] — formats results as human-readable text, JSON, or JSON Lines
//!
//! Optional enrichment modules:
//!
//! - [`identify`] — resolves MusicBrainz Recording IDs to filter false positives
//! - [`acoustid`] — AcoustID API client for fingerprint-based identification
//! - [`tags`] — reads metadata (artist, title, album, audio format info) from files
//! - [`keep`] — strategies for selecting which file to preserve in a duplicate group
//! - [`exec`] — executes shell commands on duplicate files
//! - [`fpcalc`] — drop-in replacement for Chromaprint's `fpcalc` CLI tool

#![warn(missing_docs)]

/// AcoustID API client with rate limiting.
pub mod acoustid;
/// SQLite-backed fingerprint cache with change detection.
pub mod database;
/// Execute commands on duplicate files (the non-kept ones).
pub mod exec;
/// Audio decoding and Chromaprint fingerprint generation.
pub mod fingerprint;
/// Drop-in replacement for Chromaprint's `fpcalc` tool.
pub mod fpcalc;
/// File and audio-stream hashing for duplicate classification.
pub mod hash;
/// Resolve recording MBIDs from tags and AcoustID.
pub mod identify;
/// Keep strategies for selecting which file to preserve.
pub mod keep;
/// LSH-based duplicate detection and grouping.
pub mod matcher;
/// Output formatting (human, JSON, JSON Lines).
pub mod output;
/// Directory scanning and parallel fingerprinting orchestration.
pub mod scanner;
/// Read metadata tags and audio format info from files.
pub mod tags;
