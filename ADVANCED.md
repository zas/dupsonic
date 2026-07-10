# Advanced usage

## Command reference

### `scan` — Fingerprint your library

```bash
dupsonic scan ~/Music                    # scan a directory (recursive)
dupsonic scan ~/Music /mnt/external      # scan multiple directories
dupsonic scan                            # re-scan all previously scanned paths
dupsonic scan track.flac other.mp3       # scan specific files
dupsonic scan -j 8 ~/Music              # use 8 parallel workers
dupsonic scan --length 15 ~/Music       # fast scan (15s fingerprints)
dupsonic scan --length 300 ~/Music      # for podcasts/audiobooks
dupsonic scan --ignore "*.m4p" ~/Music  # skip files matching pattern
dupsonic scan -i "**/Podcasts/**" -i "**/Audiobooks/**" ~/Music
dupsonic scan --force ~/Music           # re-fingerprint everything
dupsonic scan --list                     # show stored scan paths
dupsonic scan --remove ~/Old             # remove a path from stored set
```

Fingerprints are cached — subsequent scans only process new or modified files. Changing `--length` automatically re-scans affected files.

Scan paths are remembered: running `dupsonic scan` with no arguments re-scans all previously scanned directories. On first run with no arguments, dupsonic detects the platform's default music directory and asks for confirmation.

#### `.dupsonic-ignore` file

Place a `.dupsonic-ignore` file in your music directory to permanently skip certain files (gitignore syntax, one pattern per line):

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
dupsonic find-dupes --details            # show format, bitrate, size
dupsonic find-dupes --for ~/track.flac   # find dupes of one specific file
dupsonic find-dupes --same-tree          # only compare within same directory tree
dupsonic find-dupes --no-mbid-filter     # ignore MBIDs, show raw acoustic matches
dupsonic find-dupes --format json        # JSON output (for scripting)
dupsonic find-dupes --format jsonl       # JSON Lines (one group per line)
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

#### `--keep` strategies

| Strategy | Keeps |
|----------|-------|
| `best` (default) | Highest quality: lossless > lossy, then sample rate, bit depth, file size |
| `ext:flac,wav` | Files matching extension(s) (case-insensitive) |
| `regex:<pattern>` | Files whose path matches regex (case-sensitive) |
| `iregex:<pattern>` | Same, case-insensitive |
| `largest` / `smallest` | By file size |
| `newest` / `oldest` | By modification time |

`--apply` is required to execute commands. Without it, only shows the plan. Groups are skipped when no file matches the keep strategy.

### `identify` — Eliminate false positives via MusicBrainz

```bash
dupsonic identify                        # resolve files in duplicate groups only
dupsonic identify --all                  # resolve all files in database
```

Reads recording MBIDs from file tags (`MUSICBRAINZ_TRACKID`). After running, `find-dupes` automatically filters groups where MBIDs prove the files are different recordings.

**Do I need an AcoustID API key?**

- **If your files are tagged by Picard: no.** MBIDs are read from tags instantly.
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

| Platform | Default path |
|----------|-------------|
| Linux | `~/.local/share/dupsonic/cache.db` |
| macOS | `~/Library/Application Support/dupsonic/cache.db` |
| Windows | `AppData\Roaming\dupsonic\cache.db` |

Override with `--db <path>` or the `DUPSONIC_DB` environment variable:

```bash
export DUPSONIC_DB=/mnt/shared/dupsonic.db
dupsonic scan ~/Music
dupsonic find-dupes
```

Priority: `--db` flag > `DUPSONIC_DB` env var > platform default.

## Duplicate classification

When files have 100% identical fingerprints, dupsonic computes SHA-256 hashes to further classify them:

```
── Duplicate Group 3f8c21ab (3 files, 100% similar) ──
  [exact copy] ~/Music/Artist/track.flac (3:18)
  [exact copy] ~/Music/Backup/track.flac (3:18)
  [same audio] ~/Music/Artist/track_retagged.flac (3:18)
```

| Label | Meaning | How detected |
|-------|---------|--------------|
| **exact copy** | Byte-for-byte identical files | Same file SHA-256 |
| **same audio** | Same audio stream, different tags/metadata | Same audio-stream SHA-256, different file SHA-256 |

Hashes are computed lazily (only for 100% matches) and cached in the database.

## Output formats

Three formats: **human** (default), **json**, **jsonl** (JSON Lines).

### JSON structure

```json
[
  {
    "id": "a6bacb6d0b0d38d9...",
    "similarity": 0.97,
    "files": [
      { "path": "/music/track.flac", "duration_secs": 198.5 },
      { "path": "/music/track.mp3", "duration_secs": 198.3, "match_kind": "same_audio" }
    ]
  }
]
```

- `id` — stable group identifier (SHA-256 of file paths + metadata)
- `similarity` — fingerprint similarity score (0.0–1.0)
- `match_kind` — only present for 100% matches: `"exact_copy"` or `"same_audio"`

With `--details`, each file also includes: `format`, `size_bytes`, `sample_rate`, `bits_per_sample`, `channels`, `bitrate_kbps`, `recording_mbid`, `acoustid`, `tags`.

Progress messages go to stderr, so JSON can be piped directly:

```bash
dupsonic find-dupes --format json 2>/dev/null | jq '.[].id'
```

## Works with Picard

- **Before Picard**: find and remove duplicates so Picard doesn't process them twice
- **After Picard**: `dupsonic identify` leverages MBIDs from tags to filter false positives
- **As Picard's fingerprinter**: `dupsonic fpcalc` is a drop-in replacement for fpcalc that returns cached fingerprints instantly

To use as Picard's fingerprinter, create a wrapper script:

```bash
#!/bin/sh
# ~/bin/dupsonic-fpcalc
exec dupsonic fpcalc "$@"
```

Set this as the fpcalc path in Picard's *Options → Fingerprinting*. Run `dupsonic scan ~/Music` first to pre-populate the cache.

## Web UI

For headless servers (NAS, Raspberry Pi, OpenMediaVault), dupsonic includes a built-in web interface:

```bash
dupsonic serve                          # http://127.0.0.1:8080 (localhost only)
dupsonic serve --bind 0.0.0.0:8080      # expose on all interfaces
dupsonic serve --bind 0.0.0.0:8080 --allow-ip 192.168.1.0/24   # LAN only
```

### Access control

By default, the server binds to **localhost only** (`127.0.0.1:8080`). To expose it on the network, use `--bind`:

```bash
dupsonic serve --bind 0.0.0.0:8080
```

To restrict which IPs can connect, use `--allow-ip` (repeatable, supports CIDR notation for both IPv4 and IPv6):

```bash
dupsonic serve --bind 0.0.0.0:8080 --allow-ip 192.168.1.0/24
dupsonic serve --bind 0.0.0.0:8080 --allow-ip 10.0.0.5 --allow-ip 10.0.0.6
dupsonic serve --bind [::]:8080 --allow-ip fd00::/8 --allow-ip ::1
```

When `--allow-ip` is set, connections from non-matching IPs receive HTTP 403 Forbidden. When not set, all connections to the bound address are permitted.

Environment variables are also supported:

```bash
export DUPSONIC_BIND=0.0.0.0:8080
export DUPSONIC_ALLOW_IP=192.168.1.0/24,10.0.0.0/8
dupsonic serve
```

| Flag | Env var | Default | Description |
|------|---------|---------|-------------|
| `--bind` | `DUPSONIC_BIND` | `127.0.0.1:8080` | Address and port to listen on |
| `--allow-ip` | `DUPSONIC_ALLOW_IP` | *(none — allow all)* | Comma-separated IPs or CIDR ranges to allow |

### Features

- **Status dashboard** — file count, scan state
- **Scan** — enter a path or re-scan stored paths (pre-filled from previous scans)
- **Find duplicates** — view all groups with similarity %, classification badges, quality details (format, sample rate, bit depth, bitrate, size)
- **Delete** — moves files to system trash (FreeDesktop on Linux, native on macOS/Windows)
- **Restore** — undo a delete (restores from system trash)
- **Exclude** — hide files from future results

Files are sorted by quality (best first). All files can be deleted — it's the user's choice.

### API endpoints

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/status` | Database stats, scan state, stored paths |
| POST | `/api/scan` | Start background scan (body: `{"paths": [...]}` or `{}` for stored) |
| GET | `/api/dupes` | Find and return duplicate groups with details |
| POST | `/api/action` | Trash or exclude a file |
| POST | `/api/restore` | Restore a trashed file |

## Shell completions

```bash
# Bash
mkdir -p ~/.local/share/bash-completion/completions
dupsonic completions bash > ~/.local/share/bash-completion/completions/dupsonic

# Zsh
mkdir -p ~/.zfunc
dupsonic completions zsh > ~/.zfunc/_dupsonic
# Add to .zshrc: fpath=(~/.zfunc $fpath) && autoload -Uz compinit && compinit

# Fish
mkdir -p ~/.config/fish/completions
dupsonic completions fish > ~/.config/fish/completions/dupsonic.fish

# PowerShell
dupsonic completions powershell > dupsonic.ps1
```
