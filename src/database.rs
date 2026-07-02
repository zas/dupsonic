use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::fingerprint::FingerprintResult;

/// Statistics about the database contents.
#[derive(Debug)]
pub struct Stats {
    pub total_files: u64,
    pub fingerprinted: u64,
    pub failed: u64,
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
        for id in &stale_ids {
            conn.execute("DELETE FROM files WHERE id = ?1", params![id])?;
        }

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
    pub path: PathBuf,
    pub duration_secs: f64,
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

fn file_mtime_secs(meta: &std::fs::Metadata) -> i64 {
    use std::time::UNIX_EPOCH;
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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
}
