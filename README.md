# dupsonic

A fast, cross-platform CLI tool for finding duplicate audio files using acoustic fingerprinting.

Unlike metadata-only tools (Czkawka, dupeGuru), this identifies duplicates by **how they sound** — the same recording encoded as MP3, OGG, FLAC, or any other format will be correctly identified as a duplicate.

## Motivation

Users with large music collections frequently need to deduplicate files, but existing tools either:
- Only compare metadata/tags (fails when same audio has different tags)
- Only compare file hashes (fails when same audio is in different formats/bitrates)
- Are platform-specific or have poor UX
- Can't handle large collections (100k+ files)

This tool uses [Chromaprint](https://acoustid.org/chromaprint) acoustic fingerprinting (the same technology behind AcoustID/MusicBrainz Picard's audio identification) to detect duplicates regardless of format, bitrate, or tag differences.

See: [Community discussion](https://community.metabrainz.org/t/extremely-large-music-collection-needs-advice-on-what-dedupe-program-to-use/608781)

## Features

- **Acoustic fingerprinting** — identifies duplicates by audio content, not metadata
- **Format-agnostic** — MP3 vs OGG vs FLAC vs WAV are correctly compared
- **Incremental scanning** — SQLite cache means only new/modified files are re-fingerprinted
- **Parallel processing** — uses all available CPU cores for fingerprinting
- **Large collection support** — designed for 100k+ file collections (LSH-based matching)
- **Duration-aware** — won't falsely match files that only share the same intro
- **Multiple output formats** — human-readable, JSON, JSON Lines (for Picard plugin integration)
- **Cross-platform** — runs on Linux, macOS, Windows

## Installation

```bash
cargo install --path .
```

## Usage

### 1. Scan your music library

```bash
# Scan one or more directories
dupsonic scan ~/Music

# Use more workers for faster scanning
dupsonic scan -j 8 ~/Music /mnt/external/Music

# Force re-scan of already-fingerprinted files
dupsonic scan --force ~/Music
```

### 2. Find duplicates

```bash
# Find duplicates with default 80% similarity threshold
dupsonic find-dupes

# Stricter matching (90% similarity)
dupsonic find-dupes --threshold 0.9

# JSON output (for scripting / Picard plugin)
dupsonic find-dupes --format json

# Only compare within the same directory tree
dupsonic find-dupes --same-tree
```

### 3. Manage the cache

```bash
# Check database status
dupsonic status

# Remove entries for deleted files
dupsonic clean-cache
```

## Output formats

### Human (default)
```
── Duplicate Group 1 (3 files) ──
  [100%] /home/user/Music/Artist/Album/track.flac (3:42)
  [95%]  /home/user/Music/Downloads/track.mp3 (3:41)
  [92%]  /home/user/Music/Old/track.ogg (3:42)
```

### JSON
```json
[
  {
    "group_id": 1,
    "files": [
      {"path": "/home/user/Music/Artist/Album/track.flac", "duration_secs": 222.0, "similarity": 1.0},
      {"path": "/home/user/Music/Downloads/track.mp3", "duration_secs": 221.0, "similarity": 0.95}
    ]
  }
]
```

## Design decisions

| Decision | Rationale |
|----------|-----------|
| Rust | Cross-platform, fast, no runtime deps, single binary, callable from Picard plugin via subprocess |
| rusty-chromaprint | Pure Rust port of Chromaprint with built-in fingerprint matching — no C library dependency |
| Symphonia | Pure Rust audio decoder supporting all major formats — no FFmpeg dependency |
| SQLite cache | Incremental scans are essential for large collections; WAL mode handles concurrent reads. SQLite is compiled from source (bundled) — no system library required |
| LSH banding | O(n) candidate pair generation instead of O(n²) — enables 100k+ file collections |
| Union-Find grouping | Transitive duplicate detection: if A≈B and B≈C, all three are grouped together |
| Duration comparison | Chromaprint only fingerprints first 120s; full-duration check prevents false matches |
| JSON Lines output | Streaming-friendly for Picard plugin integration |

## Notes

- **Minimum duration**: Files shorter than ~3 seconds may not produce reliable fingerprints. Very short clips lack enough spectral information for accurate matching.
- **Verbosity**: Use `-v` for progress info, `-vv` for debug output, `-vvv` for trace-level logging. You can also set `RUST_LOG=debug` for fine-grained control.
- **Database location**: The default cache path is platform-specific (`~/.local/share/dupsonic/cache.db` on Linux, `~/Library/Application Support/dupsonic/` on macOS, `AppData\Roaming\dupsonic\` on Windows). Override with `--db <path>`.

## Supported formats

MP3, FLAC, OGG/Vorbis, Opus, WAV, M4A/AAC, WMA, AIFF, APE, WavPack, Musepack, WebM/MP4 audio.

## How it works

1. **Scan**: Walk directories, decode audio files, generate Chromaprint fingerprint (first 120s) + full duration
2. **Cache**: Store fingerprints in SQLite with file size/mtime for change detection
3. **LSH**: Hash fingerprint bands to find candidate pairs in O(n) time
4. **Filter**: Reject candidates with incompatible durations (>3s or >5% difference)
5. **Match**: Verify candidates with full bit-error-rate fingerprint comparison
6. **Group**: Use Union-Find to transitively group duplicates
7. **Report**: Output results in the requested format

## License

GPL-2.0-or-later
