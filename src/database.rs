//! SQLite-backed fingerprint cache with change detection.
//!
//! Stores acoustic fingerprints alongside file metadata (size, mtime) so that
//! subsequent scans only re-fingerprint new or modified files. The database also
//! tracks excluded files, MusicBrainz recording IDs, and scan failures.

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::fingerprint::FingerprintResult;

/// Statistics about the database contents.
#[derive(Debug)]
pub struct Stats {
    /// Total number of files tracked in the database.
    pub total_files: u64,
    /// Files with a successfully computed fingerprint.
    pub fingerprinted: u64,
    /// Files that failed fingerprinting (decode errors, etc.).
    pub failed: u64,
    /// Files whose on-disk metadata no longer matches the cached entry.
    pub stale: u64,
}

/// A thread-safe wrapper around a SQLite database for storing fingerprints.
pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    /// Open (or create) the database at the given path.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("Failed to open DB: {}", path.display()))?;

        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA busy_timeout = 5000;
            PRAGMA cache_size = -64000;
            PRAGMA temp_store = MEMORY;

            CREATE TABLE IF NOT EXISTS files (
                id INTEGER PRIMARY KEY,
                path TEXT NOT NULL UNIQUE,
                size INTEGER NOT NULL,
                mtime_secs INTEGER NOT NULL,
                duration_secs REAL,
                fingerprint BLOB,
                fingerprint_length INTEGER,
                error TEXT,
                scanned_at TEXT NOT NULL DEFAULT (datetime('now')),
                recording_mbid TEXT,
                acoustid TEXT,
                resolved_at TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_files_path ON files(path);

            CREATE TABLE IF NOT EXISTS band_hashes (
                file_id INTEGER NOT NULL,
                band_idx INTEGER NOT NULL,
                hash INTEGER NOT NULL,
                PRIMARY KEY (file_id, band_idx)
            );

            CREATE INDEX IF NOT EXISTS idx_band_hashes_lookup
                ON band_hashes(hash, band_idx);
            ",
        )?;

        // Migrate existing databases that lack the new columns
        let has_recording_mbid: bool = conn
            .prepare("SELECT recording_mbid FROM files LIMIT 0")
            .is_ok();
        if !has_recording_mbid {
            conn.execute_batch(
                "
                ALTER TABLE files ADD COLUMN recording_mbid TEXT;
                ALTER TABLE files ADD COLUMN acoustid TEXT;
                ALTER TABLE files ADD COLUMN resolved_at TEXT;
                ",
            )?;
        }

        let has_fingerprint_length: bool = conn
            .prepare("SELECT fingerprint_length FROM files LIMIT 0")
            .is_ok();
        if !has_fingerprint_length {
            conn.execute_batch("ALTER TABLE files ADD COLUMN fingerprint_length INTEGER;")?;
        }

        let has_excluded: bool = conn.prepare("SELECT excluded FROM files LIMIT 0").is_ok();
        if !has_excluded {
            conn.execute_batch("ALTER TABLE files ADD COLUMN excluded INTEGER DEFAULT 0;")?;
        }

        // Create indexes that depend on migrated columns
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_files_excluded ON files(excluded) WHERE excluded = 1;",
        )?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Check if a file has already been fingerprinted and hasn't changed.
    /// Also checks that the fingerprint was generated with the same length setting.
    pub fn is_current(&self, path: &Path, fingerprint_length: u64) -> Result<bool> {
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => return Ok(false),
        };

        let conn = self.conn.lock().unwrap();
        let result: Option<(i64, i64, Option<i64>)> = conn
            .query_row(
                "SELECT size, mtime_secs, fingerprint_length FROM files WHERE path = ?1 AND fingerprint IS NOT NULL",
                params![path.to_string_lossy().as_ref()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .ok();

        match result {
            Some((size, mtime, stored_length)) => {
                let file_size = meta.len() as i64;
                let file_mtime = file_mtime_secs(&meta);
                let length_matches = stored_length
                    .map(|l| l == fingerprint_length as i64)
                    .unwrap_or(false);
                Ok(size == file_size && mtime == file_mtime && length_matches)
            }
            None => Ok(false),
        }
    }

    /// Load all cached file metadata in one query for efficient batch filtering.
    /// Returns a map of path -> (size, mtime_secs, fingerprint_length).
    pub fn load_cached_metadata(
        &self,
    ) -> Result<std::collections::HashMap<PathBuf, (i64, i64, Option<i64>)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT path, size, mtime_secs, fingerprint_length FROM files WHERE fingerprint IS NOT NULL",
        )?;

        let mut map = std::collections::HashMap::new();
        let rows = stmt.query_map([], |row| {
            let path: String = row.get(0)?;
            let size: i64 = row.get(1)?;
            let mtime: i64 = row.get(2)?;
            let fp_length: Option<i64> = row.get(3)?;
            Ok((path, size, mtime, fp_length))
        })?;

        for row in rows {
            if let Ok((path, size, mtime, fp_length)) = row {
                map.insert(PathBuf::from(path), (size, mtime, fp_length));
            }
        }

        Ok(map)
    }

    /// Store a successful fingerprint result.
    pub fn store_fingerprint(
        &self,
        path: &Path,
        result: &FingerprintResult,
        fingerprint_length: u64,
    ) -> Result<()> {
        let meta = std::fs::metadata(path)?;
        let fp_bytes = fingerprint_to_bytes(&result.fingerprint);

        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO files (path, size, mtime_secs, duration_secs, fingerprint, fingerprint_length, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
            params![
                path.to_string_lossy().as_ref(),
                meta.len() as i64,
                file_mtime_secs(&meta),
                result.duration_secs,
                fp_bytes,
                fingerprint_length as i64,
            ],
        )?;
        Ok(())
    }

    /// Store a fingerprinting error for a file.
    pub fn store_error(&self, path: &Path, error: &str) -> Result<()> {
        let (size, mtime) = match std::fs::metadata(path) {
            Ok(meta) => (meta.len() as i64, file_mtime_secs(&meta)),
            Err(_) => (0, 0), // File gone between scan and store; use zeros
        };

        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO files (path, size, mtime_secs, duration_secs, fingerprint, error)
             VALUES (?1, ?2, ?3, NULL, NULL, ?4)",
            params![
                path.to_string_lossy().as_ref(),
                size,
                mtime,
                error,
            ],
        )?;
        Ok(())
    }

    /// Load all fingerprinted files from the database (excluding excluded files).
    pub fn load_all_fingerprints(&self) -> Result<Vec<FileFingerprint>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT path, duration_secs, fingerprint FROM files
             WHERE fingerprint IS NOT NULL AND COALESCE(excluded, 0) = 0",
        )?;

        let results = stmt
            .query_map([], |row| {
                let path: String = row.get(0)?;
                let duration_secs: f64 = row.get(1)?;
                let fp_blob: Vec<u8> = row.get(2)?;
                Ok(FileFingerprint {
                    path: PathBuf::from(path),
                    duration_secs,
                    fingerprint: bytes_to_fingerprint(&fp_blob),
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        Ok(results)
    }

    /// Get database statistics.
    pub fn stats(&self) -> Result<Stats> {
        let conn = self.conn.lock().unwrap();

        let total_files: i64 =
            conn.query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))?;
        let fingerprinted: i64 = conn.query_row(
            "SELECT COUNT(*) FROM files WHERE fingerprint IS NOT NULL",
            [],
            |row| row.get(0),
        )?;
        let failed: i64 = conn.query_row(
            "SELECT COUNT(*) FROM files WHERE error IS NOT NULL",
            [],
            |row| row.get(0),
        )?;

        // Count files that no longer exist on disk
        let mut stmt = conn.prepare("SELECT path FROM files")?;
        let stale = stmt
            .query_map([], |row| {
                let path: String = row.get(0)?;
                Ok(path)
            })?
            .filter_map(|r| r.ok())
            .filter(|p| !Path::new(p).exists())
            .count() as u64;

        Ok(Stats {
            total_files: total_files as u64,
            fingerprinted: fingerprinted as u64,
            failed: failed as u64,
            stale,
        })
    }

    /// Remove entries for files that no longer exist.
    pub fn clean_stale(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();

        let mut stmt = conn.prepare("SELECT id, path FROM files")?;
        let stale_ids: Vec<i64> = stmt
            .query_map([], |row| {
                let id: i64 = row.get(0)?;
                let path: String = row.get(1)?;
                Ok((id, path))
            })?
            .filter_map(|r| r.ok())
            .filter(|(_, p)| !Path::new(p).exists())
            .map(|(id, _)| id)
            .collect();

        let count = stale_ids.len();
        delete_ids_in_batches(&conn, &stale_ids)?;

        Ok(count)
    }

    /// Remove entries whose paths match any of the given gitignore-style patterns.
    pub fn clean_matching(&self, patterns: &[String]) -> Result<usize> {
        use globset::{Glob, GlobSetBuilder};

        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            let glob = Glob::new(pattern)
                .or_else(|_| Glob::new(&format!("**/{}", pattern)))
                .map_err(|e| anyhow::anyhow!("Invalid pattern '{}': {}", pattern, e))?;
            builder.add(glob);
        }
        let set = builder.build()?;

        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id, path FROM files")?;
        let matching_ids: Vec<i64> = stmt
            .query_map([], |row| {
                let id: i64 = row.get(0)?;
                let path: String = row.get(1)?;
                Ok((id, path))
            })?
            .filter_map(|r| r.ok())
            .filter(|(_, p)| set.is_match(p))
            .map(|(id, _)| id)
            .collect();

        let count = matching_ids.len();
        delete_ids_in_batches(&conn, &matching_ids)?;

        Ok(count)
    }

    /// Store a recording MBID (from file tags or AcoustID lookup) for a file.
    pub fn store_recording_mbid(
        &self,
        path: &Path,
        recording_mbid: &str,
        acoustid: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE files SET recording_mbid = ?1, acoustid = ?2, resolved_at = datetime('now')
             WHERE path = ?3",
            params![recording_mbid, acoustid, path.to_string_lossy().as_ref()],
        )?;
        Ok(())
    }

    /// Load files that have fingerprints but no recording MBID yet.
    pub fn load_unresolved(&self) -> Result<Vec<FileFingerprint>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT path, duration_secs, fingerprint FROM files
             WHERE fingerprint IS NOT NULL AND recording_mbid IS NULL",
        )?;

        let results = stmt
            .query_map([], |row| {
                let path: String = row.get(0)?;
                let duration_secs: f64 = row.get(1)?;
                let fp_blob: Vec<u8> = row.get(2)?;
                Ok(FileFingerprint {
                    path: PathBuf::from(path),
                    duration_secs,
                    fingerprint: bytes_to_fingerprint(&fp_blob),
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        Ok(results)
    }

    /// Get the recording MBID for a file, if resolved.
    pub fn get_recording_mbid(&self, path: &Path) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let result = conn
            .query_row(
                "SELECT recording_mbid FROM files WHERE path = ?1",
                params![path.to_string_lossy().as_ref()],
                |row| row.get(0),
            )
            .ok();
        Ok(result)
    }

    /// Get the AcoustID for a file, if resolved.
    pub fn get_acoustid(&self, path: &Path) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let result = conn
            .query_row(
                "SELECT acoustid FROM files WHERE path = ?1",
                params![path.to_string_lossy().as_ref()],
                |row| row.get(0),
            )
            .ok();
        Ok(result)
    }

    /// Store pre-computed band hashes for a file.
    pub fn store_band_hashes(&self, path: &Path, band_hashes: &[u64]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let file_id: Option<i64> = conn
            .query_row(
                "SELECT id FROM files WHERE path = ?1",
                params![path.to_string_lossy().as_ref()],
                |row| row.get(0),
            )
            .ok();

        let file_id = match file_id {
            Some(id) => id,
            None => return Ok(()), // File not in DB yet
        };

        conn.execute_batch("BEGIN")?;

        // Delete old hashes
        conn.execute(
            "DELETE FROM band_hashes WHERE file_id = ?1",
            params![file_id],
        )?;

        // Insert new hashes
        let mut stmt =
            conn.prepare("INSERT INTO band_hashes (file_id, band_idx, hash) VALUES (?1, ?2, ?3)")?;
        for (idx, &hash) in band_hashes.iter().enumerate() {
            stmt.execute(params![file_id, idx as i64, hash as i64])?;
        }

        conn.execute_batch("COMMIT")?;

        Ok(())
    }

    /// Find candidate duplicate pairs using pre-computed band hashes in the DB.
    /// Returns pairs of (path1, path2) that share at least one band hash.
    pub fn find_candidates_from_bands(&self) -> Result<Vec<(PathBuf, PathBuf)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT f1.path, f2.path
             FROM band_hashes b1
             JOIN band_hashes b2 ON b1.hash = b2.hash AND b1.band_idx = b2.band_idx
             JOIN files f1 ON b1.file_id = f1.id
             JOIN files f2 ON b2.file_id = f2.id
             WHERE b1.file_id < b2.file_id
               AND COALESCE(f1.excluded, 0) = 0
               AND COALESCE(f2.excluded, 0) = 0",
        )?;

        let results = stmt
            .query_map([], |row| {
                let p1: String = row.get(0)?;
                let p2: String = row.get(1)?;
                Ok((PathBuf::from(p1), PathBuf::from(p2)))
            })?
            .filter_map(|r| r.ok())
            .collect();

        Ok(results)
    }

    /// Check if band hashes are populated for the collection.
    pub fn has_band_hashes(&self) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM band_hashes", [], |row| row.get(0))?;
        Ok(count > 0)
    }

    /// Load fingerprint for a specific file path.
    pub fn load_fingerprint(&self, path: &Path) -> Result<Option<FileFingerprint>> {
        let conn = self.conn.lock().unwrap();
        let result = conn
            .query_row(
                "SELECT path, duration_secs, fingerprint FROM files WHERE path = ?1 AND fingerprint IS NOT NULL",
                params![path.to_string_lossy().as_ref()],
                |row| {
                    let path: String = row.get(0)?;
                    let duration_secs: f64 = row.get(1)?;
                    let fp_blob: Vec<u8> = row.get(2)?;
                    Ok(FileFingerprint {
                        path: PathBuf::from(path),
                        duration_secs,
                        fingerprint: bytes_to_fingerprint(&fp_blob),
                    })
                },
            )
            .ok();
        Ok(result)
    }

    /// Exclude a file from duplicate results.
    pub fn exclude_file(&self, path: &Path) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let rows = conn.execute(
            "UPDATE files SET excluded = 1 WHERE path = ?1",
            params![path.to_string_lossy().as_ref()],
        )?;
        Ok(rows > 0)
    }

    /// Re-include a previously excluded file.
    pub fn include_file(&self, path: &Path) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let rows = conn.execute(
            "UPDATE files SET excluded = 0 WHERE path = ?1",
            params![path.to_string_lossy().as_ref()],
        )?;
        Ok(rows > 0)
    }

    /// List all excluded files.
    pub fn list_excluded(&self) -> Result<Vec<PathBuf>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT path FROM files WHERE excluded = 1")?;
        let results = stmt
            .query_map([], |row| {
                let path: String = row.get(0)?;
                Ok(PathBuf::from(path))
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(results)
    }
}

/// A file with its fingerprint loaded from the database.
#[derive(Debug, Clone)]
pub struct FileFingerprint {
    /// Absolute path to the audio file.
    pub path: PathBuf,
    /// Full duration of the audio in seconds.
    pub duration_secs: f64,
    /// Raw Chromaprint fingerprint as a vector of `u32` sub-fingerprints.
    pub fingerprint: Vec<u32>,
}

fn fingerprint_to_bytes(fp: &[u32]) -> Vec<u8> {
    fp.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn bytes_to_fingerprint(bytes: &[u8]) -> Vec<u32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

/// Compute file modification time as seconds since UNIX epoch.
pub fn file_mtime_secs(meta: &std::fs::Metadata) -> i64 {
    use std::time::UNIX_EPOCH;
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Maximum number of IDs to delete per transaction.
const DELETE_BATCH_SIZE: usize = 1000;

/// Delete file entries and their band hashes in chunked transactions.
fn delete_ids_in_batches(conn: &Connection, ids: &[i64]) -> Result<()> {
    for chunk in ids.chunks(DELETE_BATCH_SIZE) {
        conn.execute_batch("BEGIN")?;
        for id in chunk {
            conn.execute("DELETE FROM band_hashes WHERE file_id = ?1", params![id])?;
            conn.execute("DELETE FROM files WHERE id = ?1", params![id])?;
        }
        conn.execute_batch("COMMIT")?;
    }
    Ok(())
}

/// Metadata captured from the filesystem at scan time, to be stored alongside the fingerprint.
#[derive(Debug, Clone)]
pub struct FileMeta {
    /// File size in bytes.
    pub size: i64,
    /// File modification time as seconds since UNIX epoch.
    pub mtime_secs: i64,
}

impl FileMeta {
    /// Capture metadata for a file.
    pub fn from_path(path: &Path) -> Option<Self> {
        std::fs::metadata(path).ok().map(|meta| Self {
            size: meta.len() as i64,
            mtime_secs: file_mtime_secs(&meta),
        })
    }
}

/// A scan result ready to be stored in the database.
#[derive(Debug)]
pub enum ScanResult {
    /// Successful fingerprint with file metadata and band hashes.
    Success {
        /// Absolute path to the audio file.
        path: PathBuf,
        /// File metadata (size, mtime) at scan time.
        meta: FileMeta,
        /// Full duration of the audio in seconds.
        duration_secs: f64,
        /// Chromaprint sub-fingerprint array.
        fingerprint: Vec<u32>,
        /// Duration in seconds used for fingerprinting.
        fingerprint_length: u64,
        /// Pre-computed LSH band hashes.
        band_hashes: Vec<u64>,
    },
    /// Fingerprinting failed for a file.
    Error {
        /// Absolute path to the audio file.
        path: PathBuf,
        /// File metadata if available (may be None if file disappeared).
        meta: Option<FileMeta>,
        /// Error message describing the failure.
        error: String,
    },
}

impl Database {
    /// Store a batch of scan results in a single transaction.
    pub fn store_batch(&self, results: &[ScanResult]) -> Result<()> {
        if results.is_empty() {
            return Ok(());
        }

        let conn = self.conn.lock().unwrap();
        conn.execute_batch("BEGIN")?;

        let mut fp_stmt = conn.prepare_cached(
            "INSERT OR REPLACE INTO files (path, size, mtime_secs, duration_secs, fingerprint, fingerprint_length, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
        )?;
        let mut err_stmt = conn.prepare_cached(
            "INSERT OR REPLACE INTO files (path, size, mtime_secs, duration_secs, fingerprint, error)
             VALUES (?1, ?2, ?3, NULL, NULL, ?4)",
        )?;
        let mut band_del_stmt =
            conn.prepare_cached("DELETE FROM band_hashes WHERE file_id = ?1")?;
        let mut band_ins_stmt = conn.prepare_cached(
            "INSERT INTO band_hashes (file_id, band_idx, hash) VALUES (?1, ?2, ?3)",
        )?;

        for result in results {
            match result {
                ScanResult::Success {
                    path,
                    meta,
                    duration_secs,
                    fingerprint,
                    fingerprint_length,
                    band_hashes,
                } => {
                    let fp_bytes = fingerprint_to_bytes(fingerprint);
                    fp_stmt.execute(params![
                        path.to_string_lossy().as_ref(),
                        meta.size,
                        meta.mtime_secs,
                        duration_secs,
                        fp_bytes,
                        *fingerprint_length as i64,
                    ])?;

                    // INSERT OR REPLACE deletes + re-inserts, so last_insert_rowid is valid
                    let file_id = conn.last_insert_rowid();

                    band_del_stmt.execute(params![file_id])?;
                    for (idx, &hash) in band_hashes.iter().enumerate() {
                        band_ins_stmt.execute(params![file_id, idx as i64, hash as i64])?;
                    }
                }
                ScanResult::Error { path, meta, error } => {
                    let (size, mtime) = meta
                        .as_ref()
                        .map(|m| (m.size, m.mtime_secs))
                        .unwrap_or((0, 0));
                    err_stmt.execute(params![
                        path.to_string_lossy().as_ref(),
                        size,
                        mtime,
                        error,
                    ])?;
                }
            }
        }

        conn.execute_batch("COMMIT")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn temp_db() -> (Database, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Database::open(&db_path).unwrap();
        (db, dir)
    }

    #[test]
    fn test_open_creates_database() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("new.db");
        assert!(!db_path.exists());
        let _db = Database::open(&db_path).unwrap();
        assert!(db_path.exists());
    }

    #[test]
    fn test_store_and_load_fingerprint() {
        let (db, _dir) = temp_db();

        // Create a temp audio file to store a fingerprint for
        let mut tmp = NamedTempFile::new().unwrap();
        writeln!(tmp, "fake audio data").unwrap();
        let path = tmp.path().to_path_buf();

        let result = crate::fingerprint::FingerprintResult {
            fingerprint: vec![0x12345678, 0xABCDEF01, 0xDEADBEEF],
            duration_secs: 180.5,
        };

        db.store_fingerprint(&path, &result, 120).unwrap();

        let loaded = db.load_all_fingerprints().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].path, path);
        assert_eq!(loaded[0].duration_secs, 180.5);
        assert_eq!(
            loaded[0].fingerprint,
            vec![0x12345678, 0xABCDEF01, 0xDEADBEEF]
        );
    }

    #[test]
    fn test_is_current_unchanged_file() {
        let (db, _dir) = temp_db();

        let mut tmp = NamedTempFile::new().unwrap();
        writeln!(tmp, "fake audio data").unwrap();
        let path = tmp.path().to_path_buf();

        let result = crate::fingerprint::FingerprintResult {
            fingerprint: vec![0x11111111],
            duration_secs: 60.0,
        };

        db.store_fingerprint(&path, &result, 120).unwrap();

        // File hasn't changed, should be current
        assert!(db.is_current(&path, 120).unwrap());
    }

    #[test]
    fn test_is_current_nonexistent_file() {
        let (db, _dir) = temp_db();
        let path = PathBuf::from("/nonexistent/file.mp3");
        assert!(!db.is_current(&path, 120).unwrap());
    }

    #[test]
    fn test_store_error() {
        let (db, _dir) = temp_db();

        let mut tmp = NamedTempFile::new().unwrap();
        writeln!(tmp, "bad data").unwrap();
        let path = tmp.path().to_path_buf();

        db.store_error(&path, "decode failed").unwrap();

        // Should not appear in fingerprinted list
        let loaded = db.load_all_fingerprints().unwrap();
        assert_eq!(loaded.len(), 0);

        // But should count in stats
        let stats = db.stats().unwrap();
        assert_eq!(stats.failed, 1);
        assert_eq!(stats.fingerprinted, 0);
    }

    #[test]
    fn test_stats() {
        let (db, _dir) = temp_db();

        let mut tmp1 = NamedTempFile::new().unwrap();
        writeln!(tmp1, "audio1").unwrap();
        let mut tmp2 = NamedTempFile::new().unwrap();
        writeln!(tmp2, "audio2").unwrap();

        let fp = crate::fingerprint::FingerprintResult {
            fingerprint: vec![0xAAAAAAAA],
            duration_secs: 120.0,
        };

        db.store_fingerprint(tmp1.path(), &fp, 120).unwrap();
        db.store_error(tmp2.path(), "failed").unwrap();

        let stats = db.stats().unwrap();
        assert_eq!(stats.total_files, 2);
        assert_eq!(stats.fingerprinted, 1);
        assert_eq!(stats.failed, 1);
    }

    #[test]
    fn test_clean_stale() {
        let (db, _dir) = temp_db();

        // Store a fingerprint for a file that will be deleted
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let fp = crate::fingerprint::FingerprintResult {
            fingerprint: vec![0xBBBBBBBB],
            duration_secs: 90.0,
        };
        db.store_fingerprint(&path, &fp, 120).unwrap();

        // File still exists
        assert_eq!(db.clean_stale().unwrap(), 0);

        // Delete the file
        drop(tmp);
        assert!(!path.exists());

        // Now it should be cleaned
        assert_eq!(db.clean_stale().unwrap(), 1);
        assert_eq!(db.load_all_fingerprints().unwrap().len(), 0);
    }

    #[test]
    fn test_fingerprint_bytes_roundtrip() {
        let original = vec![0x00000000, 0xFFFFFFFF, 0x12345678, 0xABCDEF01];
        let bytes = fingerprint_to_bytes(&original);
        let roundtripped = bytes_to_fingerprint(&bytes);
        assert_eq!(original, roundtripped);
    }

    #[test]
    fn test_replace_on_rescan() {
        let (db, _dir) = temp_db();

        let mut tmp = NamedTempFile::new().unwrap();
        writeln!(tmp, "audio").unwrap();
        let path = tmp.path().to_path_buf();

        let fp1 = crate::fingerprint::FingerprintResult {
            fingerprint: vec![0x11111111],
            duration_secs: 60.0,
        };
        db.store_fingerprint(&path, &fp1, 120).unwrap();

        // Re-store with different fingerprint (simulating rescan)
        let fp2 = crate::fingerprint::FingerprintResult {
            fingerprint: vec![0x22222222],
            duration_secs: 120.0,
        };
        db.store_fingerprint(&path, &fp2, 120).unwrap();

        let loaded = db.load_all_fingerprints().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].fingerprint, vec![0x22222222]);
        assert_eq!(loaded[0].duration_secs, 120.0);
    }

    #[test]
    fn test_clean_matching() {
        let (db, _dir) = temp_db();

        // Insert entries with specific paths directly into the DB
        let conn = db.conn.lock().unwrap();
        let paths = [
            "/home/user/Music/Artist/album/track.flac",
            "/home/user/Music/Podcasts/episode1.mp3",
            "/home/user/Music/Podcasts/episode2.mp3",
            "/home/user/Music/Artist/single.mp3",
            "/home/user/Music/Audiobooks/chapter1.m4a",
        ];
        for path in &paths {
            conn.execute(
                "INSERT INTO files (path, size, mtime_secs, duration_secs, fingerprint)
                 VALUES (?1, 1000, 0, 180.0, X'DEADBEEF')",
                params![path],
            )
            .unwrap();
        }
        drop(conn);

        // Remove Podcasts entries
        let removed = db
            .clean_matching(&["**/Podcasts/**".to_string()])
            .unwrap();
        assert_eq!(removed, 2);

        // Remove .mp3 files
        let removed = db.clean_matching(&["*.mp3".to_string()]).unwrap();
        assert_eq!(removed, 1); // only single.mp3 left after Podcasts removal

        // Remaining: track.flac and chapter1.m4a
        let remaining = db.load_all_fingerprints().unwrap();
        assert_eq!(remaining.len(), 2);

        // Remove with multiple patterns
        let removed = db
            .clean_matching(&["*.flac".to_string(), "**/Audiobooks/**".to_string()])
            .unwrap();
        assert_eq!(removed, 2);
        assert_eq!(db.load_all_fingerprints().unwrap().len(), 0);
    }
}
