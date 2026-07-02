# dupsonic

Find duplicate audio files by how they **sound**, not by filename or tags.

dupsonic uses acoustic fingerprinting to detect duplicates regardless of format, bitrate, or metadata — the same MP3 and FLAC of a track will be matched, even if their tags differ completely.

## Quick start

```bash
# Install
cargo install --path .

# Scan your music library
dupsonic scan ~/Music

# Find duplicates
dupsonic find-dupes

# See full details (format, bitrate, quality)
dupsonic find-dupes --details
```

Example output with `--details`:

```
── Duplicate Group 1 (2 files) ──
  [100%] ~/Music/Artist/Album/track.flac (3:18, FLAC, 48kHz/24bit ~1606kbps 38.1 MB)
  [97%]  ~/Music/Downloads/track.mp3 (3:18, MP3, 44kHz ~178kbps 3.7 MB)

── Duplicate Group 2 (2 files) ──
  [100%] ~/Music/Artist/Album/song.flac (2:52, FLAC, 44kHz/16bit ~992kbps 20.5 MB)
  [97%]  ~/Music/Old/song.ogg (2:52, OGG, 44kHz ~160kbps 3.3 MB)
```

You can immediately see which copy is higher quality and decide what to keep.

## Why dupsonic?

Existing tools fail at this:

- **Czkawka, dupeGuru** — only compare metadata or file hashes. Same song in FLAC and MP3? Not detected.
- **Duplicate Cleaner** — claims audio comparison but [struggles with cross-format matching](https://community.metabrainz.org/t/extremely-large-music-collection-needs-advice-on-what-dedupe-program-to-use/608781).
- **Manual comparison** — impossible with 10k+ files.

dupsonic fingerprints the actual audio (using [Chromaprint](https://acoustid.org/chromaprint), the same technology behind MusicBrainz Picard's track identification) and compares the fingerprints to find duplicates.

## Commands

### `scan` — Fingerprint your library

```bash
dupsonic scan ~/Music                    # scan a directory
dupsonic scan ~/Music /mnt/external      # scan multiple directories
dupsonic scan -j 8 ~/Music              # use 8 parallel workers
dupsonic scan --force ~/Music           # re-fingerprint everything
```

Fingerprints are cached in a local database — subsequent scans only process new or modified files.

### `find-dupes` — Find duplicates

```bash
dupsonic find-dupes                      # default 80% similarity threshold
dupsonic find-dupes --threshold 0.9      # stricter matching
dupsonic find-dupes --details            # show format, bitrate, tags
dupsonic find-dupes --for ~/track.flac   # find dupes of one specific file
dupsonic find-dupes --same-tree          # only compare within same directory tree
dupsonic find-dupes --format json        # JSON output (for scripting)
dupsonic find-dupes --format jsonl       # JSON Lines (streaming)
```

### `identify` — Confirm duplicates via MusicBrainz

```bash
dupsonic identify --dupes-only           # resolve files in duplicate groups
dupsonic identify                        # resolve all unresolved files
```

Reads MusicBrainz Recording IDs from your file tags (instant, free). For untagged files, queries the [AcoustID](https://acoustid.org) service (rate-limited). Requires an API key:

```bash
export ACOUSTID_API_KEY=your_key_here
dupsonic identify --dupes-only
```

Register for a free key at https://acoustid.org/new-application.

### `status` / `clean-cache`

```bash
dupsonic status                          # show database stats
dupsonic clean-cache                     # remove entries for deleted files
```

## Output formats

**Human** (default) — for interactive use:
```
── Duplicate Group 1 (2 files) ──
  [100%] ~/Music/Artist/track.flac (3:42)
  [95%]  ~/Music/Downloads/track.mp3 (3:41)
```

**JSON** (`--format json`) — for scripting and Picard plugin integration:
```json
[{"group_id": 1, "files": [{"path": "...", "duration_secs": 222.0, "similarity": 1.0}]}]
```

With `--details`, JSON includes `format`, `size_bytes`, `sample_rate`, `bits_per_sample`, `channels`, `bitrate_kbps`, `recording_mbid`, `acoustid`, and `tags` (artist/title/album).

**JSON Lines** (`--format jsonl`) — one group per line, for streaming.

## How it works

1. **Scan** — decode audio, generate [Chromaprint](https://acoustid.org/chromaprint) fingerprint (first 120s) + measure full duration
2. **Cache** — store fingerprints in SQLite with file size/mtime for change detection
3. **Find candidates** — [Locality-Sensitive Hashing](https://en.wikipedia.org/wiki/Locality-sensitive_hashing) (banding) finds candidate pairs in O(n) time
4. **Filter** — reject candidates with incompatible durations (catches files that only share the same intro)
5. **Verify** — compare candidate fingerprints with full bit-error-rate scoring
6. **Group** — Union-Find groups duplicates transitively (if A≈B and B≈C, all three are grouped)
7. **Identify** (optional) — confirm via MusicBrainz Recording IDs from tags or AcoustID

## Performance

Designed for large collections (100k+ files):

- **Parallel scanning** — uses all CPU cores for fingerprinting
- **Incremental** — only new/modified files are re-fingerprinted
- **LSH matching** — O(n) candidate generation instead of O(n²) pairwise comparison
- **Duration pre-filter** — skips expensive fingerprint comparison when durations don't match

## Supported formats

MP3, FLAC, OGG/Vorbis, Opus, WAV, M4A/AAC, WMA, AIFF, APE, WavPack, Musepack, WebM/MP4 audio.

## Installation

### Download a pre-built binary

Grab the latest release for your platform from [GitHub Releases](https://github.com/zas/dupsonic/releases).

**Linux (x86_64):**
```bash
curl -LO https://github.com/zas/dupsonic/releases/latest/download/dupsonic-linux-x86_64.tar.gz
tar xzf dupsonic-linux-x86_64.tar.gz
sudo mv dupsonic /usr/local/bin/
```

**macOS (Apple Silicon):**
```bash
curl -LO https://github.com/zas/dupsonic/releases/latest/download/dupsonic-macos-aarch64.tar.gz
tar xzf dupsonic-macos-aarch64.tar.gz
sudo mv dupsonic /usr/local/bin/
```

**Windows:** Download `dupsonic-windows-x86_64.zip` from the releases page and add to your PATH.

### Build from source (requires Rust 1.95+)

```bash
git clone https://github.com/zas/dupsonic
cd dupsonic
cargo build --release
# Binary at target/release/dupsonic
```

## Similar projects

**[soundalike](https://codeberg.org/derat/soundalike)** by Daniel Erat is a well-established Go tool that also uses Chromaprint for acoustic duplicate detection. If you're looking for a mature, lightweight solution with built-in duplicate cleanup actions, check it out.

Key differences:

| | soundalike | dupsonic |
|---|---|---|
| External deps | Requires `fpcalc` installed | Single binary, no dependencies |
| Fingerprint length | 15s (faster, but more false positives) | 120s (more accurate, slightly slower scans) |
| Duration check | No | Yes (prevents false matches from shared intros) |
| MusicBrainz integration | No | Yes (`identify` command) |
| Duplicate cleanup | Yes (`-move-smaller`, `-move-interactive`) | Not yet |
| Exclude false positives | Yes (`-exclude`) | Not yet |
| Output formats | Human text | Human, JSON, JSON Lines |
| Matching algorithm | Lookup table + all-alignment bitwise | LSH banding + aligned bitwise |

Both tools support incremental scanning with SQLite caching and handle large collections. They make different trade-offs — soundalike is lighter and has built-in cleanup actions, dupsonic focuses on accuracy and MusicBrainz/Picard integration.

## Works with Picard

dupsonic is designed to complement [MusicBrainz Picard](https://picard.musicbrainz.org/):

- **Before Picard**: find and remove duplicates so Picard doesn't have to process them
- **After Picard**: use `identify --dupes-only` to leverage MBIDs that Picard wrote to your tags
- **From Picard**: JSON output enables future plugin integration via subprocess

## Database location

The fingerprint cache is stored at a platform-specific location:
- **Linux**: `~/.local/share/dupsonic/cache.db`
- **macOS**: `~/Library/Application Support/dupsonic/cache.db`
- **Windows**: `AppData\Roaming\dupsonic\cache.db`

Override with `--db <path>`.

## License

GPL-2.0-or-later
