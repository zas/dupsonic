# Contributing to dupsonic

## Building from source

Requires Rust 1.95+:

```bash
git clone https://github.com/zas/dupsonic
cd dupsonic
cargo build --release
```

Single static binary with no external dependencies:

| Crate | Purpose |
|-------|---------|
| [chromaprint-next](https://lib.rs/crates/chromaprint-next) | Acoustic fingerprinting (pure Rust, bit-identical to C reference) |
| [Symphonia](https://github.com/pdeljanov/Symphonia) | Audio decoding (pure Rust, no FFmpeg) |
| [rusqlite](https://lib.rs/crates/rusqlite) | SQLite fingerprint cache (bundled) |
| [sha2](https://lib.rs/crates/sha2) | File and audio-stream hashing |

## Development setup

Install the pre-commit hook to catch issues before CI:

```bash
ln -sf ../../hooks/pre-commit .git/hooks/pre-commit
```

This runs `cargo fmt --check` and `cargo clippy -- -D warnings` on every commit.

## Running tests

```bash
cargo test                    # all tests (~1.5s)
cargo test --lib              # unit tests only
cargo test --test integration_test  # integration tests only
```

Tests use pre-generated audio fixtures in `tests/fixtures/classify/` (FLAC/MP3 files generated with ffmpeg). No audio generation happens at test time.

### Regenerating fixtures

If you need to regenerate the test fixtures:

```bash
cd tests/fixtures/classify
ffmpeg -y -f lavfi -i "sine=frequency=440:duration=10:sample_rate=48000" -c:a flac -sample_fmt s32 hires.flac
ffmpeg -y -f lavfi -i "sine=frequency=440:duration=10:sample_rate=44100" -c:a flac -sample_fmt s16 lowres.flac
ffmpeg -y -f lavfi -i "sine=frequency=440:duration=10:sample_rate=44100" -c:a libmp3lame -b:a 128k lossy.mp3
ffmpeg -y -f lavfi -i "sine=frequency=440:duration=10:sample_rate=44100" -c:a flac -sample_fmt s16 -metadata artist="Different Artist" -metadata title="Different Title" retagged.flac
cp lowres.flac exact_copy.flac
ffmpeg -y -f lavfi -i "sine=frequency=261.6:duration=10:sample_rate=44100" -f lavfi -i "sine=frequency=329.6:duration=10:sample_rate=44100" -f lavfi -i "sine=frequency=392.0:duration=10:sample_rate=44100" -filter_complex amix=inputs=3 -c:a flac -sample_fmt s16 chord.flac
ffmpeg -y -f lavfi -i "sine=frequency=440:duration=10:sample_rate=44100" -c:a flac -sample_fmt s16 short.flac
ffmpeg -y -f lavfi -i "sine=frequency=440:duration=60:sample_rate=44100" -c:a flac -sample_fmt s16 long.flac
```

## Architecture

```
scan ─► fingerprint ─► cache (SQLite)
                            │
find-dupes:                 ▼
           find candidates (LSH band hashes, O(n))
                            │
                            ▼
                   filter (duration compatibility)
                            │
                            ▼
                   verify (bit-error-rate comparison)
                            │
                            ▼
                   group (Union-Find)
                            │
                            ▼
                   classify (SHA-256 file + audio hashes)
                            │
                            ▼
                   identify (optional: MusicBrainz MBID filter)
                            │
                            ▼
                        output
```

### Modules

| Module | Responsibility |
|--------|---------------|
| `scanner` | File discovery, parallel fingerprinting, channel-based DB batching |
| `fingerprint` | Audio decoding (Symphonia) + Chromaprint fingerprint generation |
| `database` | SQLite schema, migrations, batch writes, metadata/hash caching |
| `matcher` | LSH candidate generation, fingerprint verification, Union-Find grouping |
| `hash` | File SHA-256 and audio-stream SHA-256 (packet-level, skips metadata) |
| `keep` | Quality scoring and keep-strategy selection |
| `output` | Human, JSON, JSONL formatting |
| `exec` | Shell command execution on duplicate files |
| `identify` | MusicBrainz MBID resolution from tags and AcoustID API |
| `tags` | Metadata reading (artist, title, album, audio format info) |
| `acoustid` | AcoustID API client with rate limiting |
| `fpcalc` | Drop-in fpcalc replacement (reads from cache) |

### Key design decisions

- **Parallel scan, batched writes**: Workers fingerprint files via Rayon, send results through a channel to a single DB writer thread that batches 64 results per transaction.
- **LSH for O(n) candidate generation**: Band hashes are pre-computed during scan and stored in SQLite. `find-dupes` uses a SQL self-join on band hashes to find candidates without loading all fingerprints into memory.
- **Lazy hash computation**: File and audio SHA-256 hashes are only computed for files in 100% fingerprint match groups, then cached in the DB.
- **Stable group IDs**: Groups are identified by a SHA-256 of their files' (path, size, mtime), sorted by ID for deterministic output ordering.
- **Progress on stderr**: All progress/status messages go to stderr, keeping stdout clean for JSON piping.

## Code style

- `cargo fmt` formatting (enforced by pre-commit hook)
- `cargo clippy -- -D warnings` (enforced by CI and pre-commit hook)
- `#![warn(missing_docs)]` on the library crate
- Public API documented with `///` doc comments

## License

GPL-2.0-or-later
