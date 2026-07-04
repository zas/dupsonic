# dupsonic

Find duplicate audio files by how they **sound**, not by filename or tags.

dupsonic uses acoustic fingerprinting to detect duplicates regardless of format, bitrate, or metadata — the same MP3 and FLAC of a track will be matched, even if their tags differ completely.

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

Windows: download `dupsonic-windows-x86_64.zip` from the [releases page](https://github.com/zas/dupsonic/releases), extract `dupsonic.exe`, and place it somewhere in your PATH (e.g., `C:\Users\<you>\bin\`).

## Usage

### Find duplicates (2 commands)

```bash
dupsonic scan ~/Music       # fingerprint your library (only once, ~6s for 2000 files)
dupsonic find-dupes         # show duplicate groups
```

That's it. Output:

```
── Duplicate Group a6bacb6d (2 files, 97% similar) ──
  ~/Music/Artist/Album/track.flac (3:18)
  ~/Music/Downloads/track.mp3 (3:18)

── Duplicate Group 759f5320 (2 files, 97% similar) ──
  ~/Music/Artist/Album/song.flac (2:52)
  ~/Music/Old/song.ogg (2:52)

Summary: 2 duplicate groups, 2 redundant files
```

Files are sorted by quality (best first) — the top file is the one `--keep best` would preserve.

### See quality details

```bash
dupsonic find-dupes --details
```

```
── Duplicate Group a6bacb6d (2 files, 97% similar) ──
  ~/Music/Artist/Album/track.flac (3:18, FLAC, 48kHz/24bit ~1606kbps 38.1 MB)
  ~/Music/Downloads/track.mp3 (3:18, MP3, 44kHz ~178kbps 3.7 MB)
```

### Duplicate classification

When files have 100% identical fingerprints, dupsonic further classifies them using cached SHA-256 hashes:

```
── Duplicate Group 3f8c21ab (3 files, 100% similar) ──
  [exact copy] ~/Music/Artist/track.flac (3:18)
  [exact copy] ~/Music/Backup/track.flac (3:18)
  [same audio] ~/Music/Artist/track_retagged.flac (3:18)
```

- **exact copy** — byte-for-byte identical files (safe to delete)
- **same audio** — same audio stream, different tags/metadata

### Remove duplicates

```bash
# Preview what would happen (safe, default)
dupsonic find-dupes --exec "mv {} /tmp/dupes/" --keep best

# Actually do it
dupsonic find-dupes --exec "mv {} /tmp/dupes/" --keep best --apply
```

`--keep best` preserves the highest quality file (lossless > lossy, higher sample rate/bit depth). Other strategies: `ext:flac`, `largest`, `newest`, `regex:<pattern>`. See [full reference](#find-dupes---exec--act-on-duplicates) below.

> **Tip:** Prefer `mv` or `trash-put` over `rm` — you can always delete moved files later once you've verified the results.

### Reduce false positives (optional)

If your files are tagged by [Picard](https://picard.musicbrainz.org/), you can eliminate false positives (e.g., a track vs its remix) using MusicBrainz recording IDs:

```bash
dupsonic identify           # reads MBIDs from tags (instant, no API key needed)
dupsonic find-dupes         # now filters out groups with different MBIDs
```

---

## Why dupsonic?

Existing tools fail at this:

- **Czkawka, dupeGuru** — only compare metadata or file hashes. Same song in FLAC and MP3? Not detected.
- **Duplicate Cleaner** — claims audio comparison but [struggles with cross-format matching](https://community.metabrainz.org/t/extremely-large-music-collection-needs-advice-on-what-dedupe-program-to-use/608781).
- **Manual comparison** — impossible with 10k+ files.

dupsonic fingerprints the actual audio (using [Chromaprint](https://acoustid.org/chromaprint), the same technology behind MusicBrainz Picard's track identification) and compares the fingerprints to find duplicates.

## Command reference

### `scan` — Fingerprint your library

```bash
dupsonic scan ~/Music                    # scan a directory (recursive)
dupsonic scan ~/Music /mnt/external      # scan multiple directories
dupsonic scan track.flac other.mp3       # scan specific files
dupsonic scan ~/Music/*.flac             # shell glob expansion
dupsonic scan -j 8 ~/Music              # use 8 parallel workers
dupsonic scan --length 15 ~/Music       # fast scan (15s, like soundalike)
dupsonic scan --length 300 ~/Music      # for podcasts/audiobooks
dupsonic scan --ignore "*.m4p" ~/Music  # skip files matching pattern
dupsonic scan -i "**/Podcasts/**" -i "**/Audiobooks/**" ~/Music
dupsonic scan --force ~/Music           # re-fingerprint everything
```

Fingerprints are cached — subsequent scans only process new or modified files. Changing `--length` automatically re-scans affected files.

**`.dupsonic-ignore` file:** Place a `.dupsonic-ignore` file in your music directory to permanently skip certain files (gitignore syntax, one pattern per line):

```
# .dupsonic-ignore
*.m4p
**/Podcasts/**
**/Audiobooks/**
```

Patterns from `.dupsonic-ignore` are combined with `--ignore` CLI flags.

### `find-dupes` — Find duplicates

```bash
dupsonic find-dupes                      # default 80% similarity threshold
dupsonic find-dupes --threshold 0.9      # stricter matching
dupsonic find-dupes --details            # show format, bitrate, tags
dupsonic find-dupes --for ~/track.flac   # find dupes of one specific file
dupsonic find-dupes --same-tree          # only compare within same directory tree
dupsonic find-dupes --no-mbid-filter     # ignore MBIDs, show raw acoustic matches
dupsonic find-dupes --format json        # JSON output (for scripting)
```

### `find-dupes --exec` — Act on duplicates

```bash
# Preview (default is dry-run)
dupsonic find-dupes --exec "mv {} /tmp/dupes/" --keep best

# Execute
dupsonic find-dupes --exec "mv {} /tmp/dupes/" --keep best --apply
dupsonic find-dupes --exec "trash-put {}" --keep ext:flac --apply
```

> **⚠️ Avoid destructive commands like `rm`.** Use `mv` to move duplicates to a staging folder, review them, then delete manually. Or use `trash-put` (Linux) / `trash` (macOS) to send to the system trash.

**`--keep` strategies:**

| Strategy | Keeps |
|----------|-------|
| `best` (default) | Highest quality: lossless > lossy, then sample rate, bit depth, file size |
| `ext:flac,wav` | Files matching extension(s) (case-insensitive) |
| `regex:<pattern>` | Files whose path matches regex (case-sensitive) |
| `iregex:<pattern>` | Same, case-insensitive |
| `largest` / `smallest` | By file size |
| `newest` / `oldest` | By modification time |

**Safety:** `--apply` is required to execute. Without it, only shows the plan. Groups are skipped when no file matches the keep strategy.

### `identify` — Eliminate false positives via MusicBrainz

```bash
dupsonic identify                        # resolve files in duplicate groups
dupsonic identify --all                  # resolve all files in database
```

**Optional** — only useful when acoustically similar tracks are different recordings (remixes, alternate versions). Reads recording MBIDs from file tags (`MUSICBRAINZ_TRACKID`). After running, `find-dupes` automatically filters groups with different MBIDs.

**Do I need an AcoustID API key?**

- **If your files are tagged by Picard: NO.** MBIDs are read from tags instantly.
- **If you have untagged files:** register at https://acoustid.org/new-application, then set `ACOUSTID_API_KEY=your_key` or pass `--api-key`.

### `exclude` / `include` — Manage exceptions

```bash
dupsonic exclude file1.flac file2.mp3    # hide from duplicate results
dupsonic include file1.flac              # re-include
dupsonic include --all                   # clear all exclusions
```

### `clean-cache` — Remove database entries

```bash
dupsonic clean-cache                     # remove entries for deleted files
dupsonic clean-cache "**/Podcasts/**"    # remove entries matching patterns
dupsonic clean-cache "*.wma" "*.m4p"    # remove by extension
```

Without arguments, removes entries for files that no longer exist on disk. With gitignore-style patterns, removes matching entries regardless of whether the files still exist.

### `status`

```bash
dupsonic status                          # show database stats
```

## Configuration

### Database location

dupsonic stores its fingerprint cache in a SQLite database:

- **Linux**: `~/.local/share/dupsonic/cache.db`
- **macOS**: `~/Library/Application Support/dupsonic/cache.db`
- **Windows**: `AppData\Roaming\dupsonic\cache.db`

Override with `--db <path>` or the `DUPSONIC_DB` environment variable:

```bash
export DUPSONIC_DB=/mnt/shared/dupsonic.db
dupsonic scan ~/Music
dupsonic find-dupes
```

The `--db` flag takes precedence over the environment variable.

## Output formats

**Human** (default), **JSON** (`--format json`), **JSON Lines** (`--format jsonl`).

With `--details`, JSON includes: `format`, `size_bytes`, `sample_rate`, `bits_per_sample`, `channels`, `bitrate_kbps`, `recording_mbid`, `acoustid`, and `tags` (artist/title/album).

JSON structure:
```json
[
  {
    "id": "a6bacb6d...",
    "similarity": 0.97,
    "files": [
      { "path": "/music/track.flac", "duration_secs": 198.5 },
      { "path": "/music/track.mp3", "duration_secs": 198.3, "match_kind": "same_audio" }
    ]
  }
]
```

The `match_kind` field appears only for 100% matches: `"exact_copy"` or `"same_audio"`.

Progress messages go to stderr, so JSON output can be piped directly:
```bash
dupsonic find-dupes --format json 2>/dev/null | jq '.[] | .id'
```

## Performance

### Benchmark (2025 files, FLAC/MP3 collection)

| | dupsonic (15s) | dupsonic (120s) | soundalike (15s) |
|---|---|---|---|
| **Scan** | **~6s** | 36s | 2m 38s |
| **Find dupes** | 0.04s | 0.04s | (included in scan) |
| **Total** | **~6s** | **36s** | **2m 38s** |
| **Duplicates found** | 33 groups | 33 groups | 32 groups |

Designed for 100k+ file collections: parallel scanning, incremental cache, LSH-based O(n) matching, batched database writes.

## How it works

1. **Scan** — decode audio, generate Chromaprint fingerprint (first 120s) + measure full duration
2. **Cache** — store fingerprints in SQLite with file size/mtime for change detection
3. **Find candidates** — LSH banding finds candidate pairs in O(n) time
4. **Filter** — reject candidates with incompatible durations
5. **Verify** — bit-error-rate fingerprint comparison
6. **Group** — Union-Find groups duplicates transitively
7. **Classify** — for 100% matches, compute file/audio hashes to distinguish exact copies from same-audio
8. **Identify** (optional) — filter by MusicBrainz Recording IDs

## Supported formats

MP3, FLAC, OGG/Vorbis, Opus, WAV, M4A/AAC, WMA, AIFF, APE, WavPack, Musepack, WebM/MP4 audio.

## Works with Picard

- **Before Picard**: find and remove duplicates so Picard doesn't process them
- **After Picard**: `dupsonic identify` leverages MBIDs from tags to filter false positives
- **As Picard's fingerprinter**: use `dupsonic fpcalc` as a faster drop-in for fpcalc (returns from cache instantly)

To use as Picard's fingerprinter, create a wrapper script (`~/bin/dupsonic-fpcalc`):
```bash
#!/bin/sh
exec dupsonic fpcalc "$@"
```
Then set this as the fpcalc path in Picard's *Options → Fingerprinting*. Run `dupsonic scan ~/Music` first to pre-populate the cache.

## Similar projects

**[soundalike](https://codeberg.org/derat/soundalike)** by Daniel Erat — a mature Go tool using Chromaprint. Lightweight, has built-in move/delete commands. Requires external `fpcalc`, defaults to 15s fingerprints, no MusicBrainz integration.

## Building from source

Requires Rust 1.95+:
```bash
git clone https://github.com/zas/dupsonic
cd dupsonic
cargo build --release
ln -sf ../../hooks/pre-commit .git/hooks/pre-commit  # install pre-commit checks
```

Single static binary, no external dependencies:
- **[chromaprint-next](https://lib.rs/crates/chromaprint-next)** — acoustic fingerprinting (pure Rust, faster than C reference)
- **[Symphonia](https://github.com/pdeljanov/Symphonia)** — audio decoding (pure Rust, no FFmpeg)
- **SQLite** — fingerprint cache (bundled)

## Shell completions

```bash
# Bash
mkdir -p ~/.local/share/bash-completion/completions
dupsonic completions bash > ~/.local/share/bash-completion/completions/dupsonic

# Zsh (add 'fpath=(~/.zfunc $fpath)' and 'autoload -Uz compinit && compinit' to .zshrc)
mkdir -p ~/.zfunc
dupsonic completions zsh > ~/.zfunc/_dupsonic

# Zsh with oh-my-zsh
mkdir -p ~/.oh-my-zsh/completions
dupsonic completions zsh > ~/.oh-my-zsh/completions/_dupsonic

# Fish
mkdir -p ~/.config/fish/completions
dupsonic completions fish > ~/.config/fish/completions/dupsonic.fish

# PowerShell
dupsonic completions powershell > dupsonic.ps1
```

## License

GPL-2.0-or-later
