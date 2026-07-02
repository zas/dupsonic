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
dupsonic scan --length 15 ~/Music       # fast scan (15s, like soundalike)
dupsonic scan --length 300 ~/Music      # for podcasts/audiobooks with long shared intros
dupsonic scan --force ~/Music           # re-fingerprint everything
```

Fingerprints are cached in a local database — subsequent scans only process new or modified files.

> **Note:** The `--length` value must be consistent across scans. Fingerprints generated with different lengths are not comparable — if you change `--length`, re-scan with `--force`. The default (120s) is a good balance of accuracy and speed for music.

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

### `find-dupes --exec` — Act on duplicates

```bash
# Preview what would happen (default is dry-run)
dupsonic find-dupes --exec "trash-put {}" --keep best

# Actually execute
dupsonic find-dupes --exec "rm {}" --keep best --apply

# Keep FLAC files, remove the rest
dupsonic find-dupes --exec "rm {}" --keep ext:flac --apply

# Keep files in your curated library
dupsonic find-dupes --exec "rm {}" --keep regex:"^/home/user/Music/Library/" --apply

# Move lossy copies to a folder
dupsonic find-dupes --exec "mv {} /tmp/dupes/" --keep ext:flac,wav --apply
```

**`--keep` strategies** decide which file to preserve in each group:

| Strategy | Keeps |
|----------|-------|
| `best` (default) | Highest quality: lossless > lossy, then sample rate, bit depth, file size |
| `ext:flac,wav` | Files matching the given extension(s) (case-insensitive) |
| `regex:<pattern>` | Files whose path matches the regex |
| `iregex:<pattern>` | Same, case-insensitive |
| `largest` / `smallest` | By file size |
| `newest` / `oldest` | By modification time |

When multiple files match the keep strategy, the best quality among them is kept. When *no* file matches, the group is skipped entirely (nothing is executed).

**Safety:** `--apply` is required to execute. Without it, dupsonic only shows what would happen. Paths are properly shell-quoted to handle spaces, parentheses, and special characters safely.

### `identify` — Eliminate false positives via MusicBrainz

```bash
dupsonic identify                        # resolve files in duplicate groups (default)
dupsonic identify --all                  # resolve all unresolved files
```

The `identify` step is **optional** — `find-dupes` works fine without it. But it helps in one specific case: when two *different* recordings are acoustically similar enough to trigger a match. Examples:

- A track and its remix/rough mix (similar arrangement, same artist)
- Two tracks by the same band with identical duration and similar instrumentation

After running `identify`, `find-dupes` automatically filters out groups where MBIDs prove the files are different recordings. Use `--no-mbid-filter` to see all acoustic matches regardless.

**How it works:**
1. Reads the recording MBID from file tags (`MUSICBRAINZ_TRACKID` in Vorbis/FLAC/MP4 — this is the standard tag written by all versions of Picard for the [recording](https://musicbrainz.org/doc/Recording) identifier)
2. For files without tags, queries [AcoustID](https://acoustid.org) (rate-limited to 3 req/s)

**Caveats:**
- **AcoustID queries are slow** — at 3 req/s, 1000 untagged files takes ~5 minutes. For collections tagged by Picard, tag reading is instant and no API key is needed.
- **MBIDs can be wrong** — files may be mis-tagged, and AcoustID can return low-confidence matches. Use `--no-mbid-filter` on `find-dupes` if you suspect MBID filtering is hiding valid duplicates.
- **Not all music is in AcoustID** — obscure/independent releases may not be in the database.

**Setup (only needed for AcoustID queries on untagged files):**

```bash
export ACOUSTID_API_KEY=your_key_here
dupsonic identify
```

Register for a free key at https://acoustid.org/new-application.

### `exclude` / `include` — Manage false positives

```bash
dupsonic exclude file1.flac file2.mp3    # hide files from duplicate results
dupsonic include file1.flac              # re-include a file
dupsonic include --all                   # clear all exclusions
```

Excluded files won't appear in any future `find-dupes` results. Useful for files that are acoustically similar but intentionally kept (e.g., a live version vs studio version).

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

### Benchmark (2025 files, FLAC/MP3 collection)

| | dupsonic (15s) | dupsonic (120s) | soundalike (15s) |
|---|---|---|---|
| **Scan** | **~6s** | 36s | 2m 38s |
| **Find dupes** | 0.04s | 0.04s | (included in scan) |
| **Total** | **~6s** | **36s** | **2m 38s** |
| **Duplicates found** | 33 groups | 33 groups | 32 groups |

- At the same fingerprint length (15s), dupsonic is **~25× faster** than soundalike
- At 120s (default, 8× more audio), dupsonic is still **4× faster** than soundalike at 15s
- Finding duplicates is decoupled from scanning — rerun `find-dupes` with different thresholds instantly

The speedup comes from in-process audio decoding (no external `fpcalc` process per file), parallel workers across all CPU cores, and chromaprint-next's optimized fingerprint algorithm.

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

No external dependencies — everything is compiled into a single static binary:
- **[chromaprint-next](https://lib.rs/crates/chromaprint-next)** — acoustic fingerprinting (pure Rust, bit-identical to C Chromaprint, faster than the C reference)
- **[Symphonia](https://github.com/pdeljanov/Symphonia)** — audio decoding for all major formats (pure Rust, no FFmpeg)
- **SQLite** — fingerprint cache (bundled, compiled from source)

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
- **After Picard**: use `dupsonic identify` to leverage MBIDs that Picard wrote to your tags
- **As Picard's fingerprinter**: use `dupsonic fpcalc` as a faster drop-in replacement for fpcalc

### Using dupsonic as Picard's fingerprinter

dupsonic can replace Chromaprint's `fpcalc` for fingerprinting in Picard. When a file has already been scanned by dupsonic, the fingerprint is returned instantly from cache (no audio decoding needed).

**Setup:**

1. Create a wrapper script:

   **Linux/macOS** — save as `~/bin/dupsonic-fpcalc` and make executable:
   ```bash
   #!/bin/sh
   exec dupsonic fpcalc "$@"
   ```

   **Windows** — save as `dupsonic-fpcalc.bat`:
   ```batch
   @echo off
   dupsonic fpcalc %*
   ```

2. In Picard, go to *Options → Fingerprinting* and set the fpcalc path to your wrapper script.

Picard calls `<fpcalc> -json -length 120 <file>` — dupsonic accepts the same arguments and produces compatible JSON output.

**Tip:** Run `dupsonic scan ~/Music` first to pre-populate the cache. Then Picard's fingerprint lookups will be instant for all scanned files.

## Database location

The fingerprint cache is stored at a platform-specific location:
- **Linux**: `~/.local/share/dupsonic/cache.db`
- **macOS**: `~/Library/Application Support/dupsonic/cache.db`
- **Windows**: `AppData\Roaming\dupsonic\cache.db`

Override with `--db <path>`.

## License

GPL-2.0-or-later
