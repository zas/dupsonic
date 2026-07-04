# dupsonic

<!-- badges -->
[![Crates.io](https://img.shields.io/crates/v/dupsonic)](https://crates.io/crates/dupsonic)
[![License: GPL-2.0-or-later](https://img.shields.io/crates/l/dupsonic)](LICENSE)
[![CI](https://github.com/zas/dupsonic/actions/workflows/ci.yml/badge.svg)](https://github.com/zas/dupsonic/actions)

Find duplicate audio files by how they **sound**, not by filename or tags.

dupsonic uses acoustic fingerprinting to detect duplicates regardless of format, bitrate, or metadata — the same MP3 and FLAC of a track will be matched, even if their tags differ completely.

## Quick start

```bash
dupsonic scan ~/Music       # fingerprint your library (only once, ~6s for 2000 files)
dupsonic find-dupes         # show duplicate groups
```

Output:

```
── Duplicate Group a6bacb6d (2 files, 97% similar) ──
  ~/Music/Artist/Album/track.flac (3:18)
  ~/Music/Downloads/track.mp3 (3:18)

── Duplicate Group 759f5320 (2 files, 97% similar) ──
  ~/Music/Artist/Album/song.flac (2:52)
  ~/Music/Old/song.ogg (2:52)

Summary: 2 duplicate groups, 2 redundant files
```

Files are sorted by quality (best first). Remove duplicates with:

```bash
dupsonic find-dupes --exec "mv {} /tmp/dupes/" --keep best          # preview
dupsonic find-dupes --exec "mv {} /tmp/dupes/" --keep best --apply  # execute
```

See [ADVANCED.md](ADVANCED.md) for the full command reference, `--keep` strategies, output formats, and configuration.

## Install

**From crates.io** (requires Rust toolchain):

```bash
cargo install dupsonic
```

**Pre-built binaries** from [GitHub Releases](https://github.com/zas/dupsonic/releases):

```bash
# Linux
curl -LO https://github.com/zas/dupsonic/releases/latest/download/dupsonic-linux-x86_64.tar.gz
tar xzf dupsonic-linux-x86_64.tar.gz
sudo mv dupsonic /usr/local/bin/

# macOS (Apple Silicon)
curl -LO https://github.com/zas/dupsonic/releases/latest/download/dupsonic-macos-aarch64.tar.gz
tar xzf dupsonic-macos-aarch64.tar.gz
sudo mv dupsonic /usr/local/bin/
```

**Windows:** download `dupsonic-windows-x86_64.zip` from the [releases page](https://github.com/zas/dupsonic/releases), extract `dupsonic.exe`, and place it in your PATH.

## Why dupsonic?

Existing tools fail at cross-format duplicate detection:

- **Czkawka, dupeGuru** — compare metadata or file hashes only. Same song in FLAC and MP3? Not detected.
- **Duplicate Cleaner** — claims audio comparison but [struggles with cross-format matching](https://community.metabrainz.org/t/extremely-large-music-collection-needs-advice-on-what-dedupe-program-to-use/608781).
- **Manual comparison** — impossible with 10k+ files.

dupsonic fingerprints the actual audio using [Chromaprint](https://acoustid.org/chromaprint) (the same technology behind MusicBrainz Picard) and compares fingerprints to find duplicates.

## Supported formats

MP3, FLAC, OGG/Vorbis, Opus, WAV, M4A/AAC, WMA, AIFF, APE, WavPack, Musepack, WebM/MP4 audio.

## Performance

Benchmark with 2025 files (mixed FLAC/MP3 collection):

| | dupsonic (15s) | dupsonic (120s) | soundalike (15s) |
|---|---|---|---|
| **Scan** | **~6s** | 36s | 2m 38s |
| **Find dupes** | 0.04s | 0.04s | (included in scan) |
| **Total** | **~6s** | **36s** | **2m 38s** |
| **Duplicates found** | 33 groups | 33 groups | 32 groups |

Designed for 100k+ file collections: parallel scanning, incremental cache, LSH-based O(n) matching, batched database writes.

## Similar projects

**[soundalike](https://codeberg.org/derat/soundalike)** by Daniel Erat — a mature Go tool using Chromaprint. Lightweight, has built-in move/delete commands. Requires external `fpcalc`, defaults to 15s fingerprints, no MusicBrainz integration.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for build instructions, architecture overview, and development workflow.

## License

GPL-2.0-or-later
