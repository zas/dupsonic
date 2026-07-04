//! LSH-based duplicate detection and grouping.
//!
//! Uses Locality-Sensitive Hashing (banding on raw sub-fingerprints) to find
//! candidate duplicate pairs in O(n) time, then verifies candidates with
//! bit-error-rate comparison and groups them transitively using Union-Find.

use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::database::Database;
use crate::fingerprint::{compare_fingerprints, durations_compatible};

/// A group of files that are acoustic duplicates of each other.
#[derive(Debug, Clone)]
pub struct DuplicateGroup {
    /// Files in this group, sorted by quality (best first)
    pub files: Vec<DuplicateFile>,
    /// Similarity score between files in this group (0.0 to 1.0)
    pub similarity: f64,
}

/// How closely two files match (only meaningful for 100% fingerprint matches).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    /// Files are byte-for-byte identical (same hash).
    ExactCopy,
    /// Same audio content but different metadata, container, or encoding.
    SameAudio,
    /// Similarity-based match (score < 1.0).
    Similar,
}

/// A single file within a duplicate group.
#[derive(Debug, Clone)]
pub struct DuplicateFile {
    /// Absolute path to the audio file.
    pub path: std::path::PathBuf,
    /// Full duration of the audio in seconds.
    pub duration_secs: f64,
    /// Classification of the match (exact copy, same audio, or similar).
    pub match_kind: MatchKind,
}

// --- LSH Configuration ---
// We use banding on the raw u32 sub-fingerprints.
// Each band is a contiguous block of sub-fingerprints that we hash together.
// Two fingerprints that share at least one band hash are candidate pairs.
//
// With B bands of R rows each:
// - P(candidate | similarity s) ≈ 1 - (1 - s^R)^B
//
// Chromaprint sub-fingerprints are highly correlated (overlapping audio frames),
// so we use small bands (R=2) with many bands (B=50) to ensure high recall.
// - For s=0.9, R=2, B=50: P ≈ 1.0 (all true matches found)
// - For s=0.5, R=2, B=50: P ≈ 1.0 (more candidates, but verification is fast)
//
// The bucket size cap (max 1000) prevents pathological blowup from common patterns.

/// Number of rows per band (sub-fingerprints hashed together per band)
const LSH_ROWS_PER_BAND: usize = 2;

/// Number of bands
const LSH_NUM_BANDS: usize = 50;

/// Find groups of duplicate files based on fingerprint similarity.
///
/// Uses Locality-Sensitive Hashing (LSH) to find candidate pairs in O(n),
/// then verifies candidates with full fingerprint comparison.
/// This scales to 100k+ file collections.
///
/// When band hashes are pre-computed in the DB (from scan), uses SQL-based
/// candidate generation which requires minimal RAM. Falls back to in-memory
/// LSH for backward compatibility.
pub fn find_duplicates(
    db: &Database,
    threshold: f64,
    same_tree: bool,
) -> Result<Vec<DuplicateGroup>> {
    find_duplicates_from_db(db, threshold, same_tree)
}

/// DB-based candidate generation: uses pre-computed band hashes in SQLite.
/// Only loads fingerprints for candidate pairs, not the entire collection.
fn find_duplicates_from_db(
    db: &Database,
    threshold: f64,
    same_tree: bool,
) -> Result<Vec<DuplicateGroup>> {
    println!("Phase 1: Finding candidate pairs (DB)...");
    let mut candidates = db.find_candidates_from_bands()?;

    // Apply duration + same_tree filters
    candidates.retain(|(p1, p2)| {
        if same_tree && !shares_root(p1, p2) {
            return false;
        }
        true
    });

    println!("  Found {} candidate pairs", candidates.len());

    if candidates.is_empty() {
        println!("No duplicates found.");
        return Ok(Vec::new());
    }

    // Phase 2: Load only candidate fingerprints and verify
    println!("Phase 2: Verifying {} candidates...", candidates.len());

    let mut uf_map: HashMap<PathBuf, usize> = HashMap::new();
    let mut uf_idx = 0;
    let mut uf = UnionFind::new(candidates.len() * 2); // upper bound

    for (p1, p2) in &candidates {
        let idx1 = *uf_map.entry(p1.clone()).or_insert_with(|| {
            let i = uf_idx;
            uf_idx += 1;
            i
        });
        let idx2 = *uf_map.entry(p2.clone()).or_insert_with(|| {
            let i = uf_idx;
            uf_idx += 1;
            i
        });

        // Load fingerprints on demand
        let fp1 = match db.load_fingerprint(p1)? {
            Some(f) => f,
            None => continue,
        };
        let fp2 = match db.load_fingerprint(p2)? {
            Some(f) => f,
            None => continue,
        };

        // Duration filter
        if !durations_compatible(fp1.duration_secs, fp2.duration_secs) {
            continue;
        }

        let score = compare_fingerprints(&fp1.fingerprint, &fp2.fingerprint);
        if score >= threshold {
            uf.union(idx1, idx2);
        }
    }

    // Build groups from union-find
    let mut groups_map: HashMap<usize, Vec<PathBuf>> = HashMap::new();
    for (path, &idx) in &uf_map {
        let root = uf.find(idx);
        groups_map.entry(root).or_default().push(path.clone());
    }

    let mut groups: Vec<DuplicateGroup> = groups_map
        .into_values()
        .filter(|members| members.len() > 1)
        .map(|members| {
            let ref_fp = db.load_fingerprint(&members[0]).ok().flatten();

            // Compute scores for non-reference files
            let mut max_score = 0.0_f64;
            let mut files: Vec<DuplicateFile> = members
                .iter()
                .map(|path| {
                    let fp = db.load_fingerprint(path).ok().flatten();
                    let score = match (&ref_fp, &fp) {
                        (Some(r), Some(f)) if path != &members[0] => {
                            compare_fingerprints(&r.fingerprint, &f.fingerprint)
                        }
                        _ => 0.0, // reference — not counted
                    };
                    if path != &members[0] {
                        max_score = max_score.max(score);
                    }
                    let duration = fp.map(|f| f.duration_secs).unwrap_or(0.0);
                    DuplicateFile {
                        path: path.clone(),
                        duration_secs: duration,
                        match_kind: MatchKind::Similar,
                    }
                })
                .collect();
            files.sort_by(|a, b| a.path.cmp(&b.path));
            DuplicateGroup {
                files,
                similarity: max_score,
            }
        })
        .collect();

    groups.sort_by_key(|g| std::cmp::Reverse(g.files.len()));

    println!("Found {} duplicate groups", groups.len());
    Ok(groups)
}

/// Filter out duplicate groups where resolved MBIDs prove they're different recordings.
///
/// A group is removed if ALL files have a recording MBID and they don't all match.
/// Groups where some files lack MBIDs are kept (benefit of the doubt).
pub fn filter_by_mbids(groups: Vec<DuplicateGroup>, db: &Database) -> Vec<DuplicateGroup> {
    let before = groups.len();
    let filtered: Vec<DuplicateGroup> = groups
        .into_iter()
        .filter(|group| {
            let mbids: Vec<Option<String>> = group
                .files
                .iter()
                .map(|f| db.get_recording_mbid(&f.path).ok().flatten())
                .collect();

            // If any file lacks an MBID, we can't disprove — keep the group
            if mbids.iter().any(|m| m.is_none()) {
                return true;
            }

            // All files have MBIDs — check if they all match
            let first = mbids[0].as_ref().unwrap();
            mbids.iter().all(|m| m.as_ref().unwrap() == first)
        })
        .collect();

    let removed = before - filtered.len();
    if removed > 0 {
        println!(
            "  Removed {} group(s) with different recording MBIDs",
            removed
        );
    }

    filtered
}

/// Classify 100% fingerprint matches as exact copies or same-audio.
///
/// Uses cached hashes from the database (computed on first need):
/// - **file_sha256**: detects byte-identical copies (same file, different path)
/// - **audio_sha256**: detects same audio stream with different tags/metadata
///
/// Hashes are computed lazily — only for files in 100% match groups — and
/// cached in the DB for future runs.
pub fn classify_matches(groups: &mut [DuplicateGroup], db: &Database) {
    use std::collections::HashMap;

    // Load any existing cached hashes
    let mut hashes = db.load_hashes().unwrap_or_default();

    for group in groups.iter_mut() {
        // Only classify groups with 100% fingerprint similarity
        if group.similarity < 1.0 {
            continue;
        }

        let perfect_indices: Vec<usize> = (0..group.files.len()).collect();

        // Ensure hashes are computed for all perfect-match files
        for &idx in &perfect_indices {
            let path = &group.files[idx].path;
            let needs_compute = match hashes.get(path) {
                Some((fh, ah)) => fh.is_none() || ah.is_none(),
                None => true,
            };
            if needs_compute {
                let file_hash = crate::hash::file_sha256(path).unwrap_or_default();
                let audio_hash = crate::hash::audio_sha256(path).unwrap_or_default();
                let _ = db.store_hashes(path, &file_hash, &audio_hash);
                hashes.insert(
                    path.clone(),
                    (Some(file_hash), Some(audio_hash)),
                );
            }
        }

        // Get file hashes
        let file_hashes: Vec<&str> = perfect_indices
            .iter()
            .map(|&idx| {
                hashes
                    .get(&group.files[idx].path)
                    .and_then(|(fh, _)| fh.as_deref())
                    .unwrap_or("")
            })
            .collect();

        // Get audio hashes
        let audio_hashes: Vec<&str> = perfect_indices
            .iter()
            .map(|&idx| {
                hashes
                    .get(&group.files[idx].path)
                    .and_then(|(_, ah)| ah.as_deref())
                    .unwrap_or("")
            })
            .collect();

        // Classify: check file hash first, then audio hash
        let all_same_file = !file_hashes[0].is_empty()
            && file_hashes.iter().all(|h| *h == file_hashes[0]);

        if all_same_file {
            for &idx in &perfect_indices {
                group.files[idx].match_kind = MatchKind::ExactCopy;
            }
            continue;
        }

        // Group by file hash to find byte-identical subgroups
        let mut fh_groups: HashMap<&str, Vec<usize>> = HashMap::new();
        for (pos, &idx) in perfect_indices.iter().enumerate() {
            if !file_hashes[pos].is_empty() {
                fh_groups.entry(file_hashes[pos]).or_default().push(idx);
            }
        }
        for (_, indices) in &fh_groups {
            if indices.len() > 1 {
                for &idx in indices {
                    group.files[idx].match_kind = MatchKind::ExactCopy;
                }
            }
        }

        // For remaining unclassified files, check audio hash
        let all_same_audio = !audio_hashes[0].is_empty()
            && audio_hashes.iter().all(|h| *h == audio_hashes[0]);

        if all_same_audio {
            for &idx in &perfect_indices {
                if group.files[idx].match_kind != MatchKind::ExactCopy {
                    group.files[idx].match_kind = MatchKind::SameAudio;
                }
            }
        } else {
            // Group by audio hash
            let mut ah_groups: HashMap<&str, Vec<usize>> = HashMap::new();
            for (pos, &idx) in perfect_indices.iter().enumerate() {
                if !audio_hashes[pos].is_empty() {
                    ah_groups.entry(audio_hashes[pos]).or_default().push(idx);
                }
            }
            for (_, indices) in &ah_groups {
                if indices.len() > 1 {
                    for &idx in indices {
                        if group.files[idx].match_kind != MatchKind::ExactCopy {
                            group.files[idx].match_kind = MatchKind::SameAudio;
                        }
                    }
                }
            }
        }
    }
}

/// Find duplicates of a specific file.
///
/// Fingerprints the target file (or uses cached fingerprint), then compares it
/// against all fingerprints in the database. This is O(n) — much faster than
/// the full LSH pipeline when you only care about one file.
pub fn find_duplicates_for(
    db: &Database,
    target: &std::path::Path,
    threshold: f64,
) -> Result<Vec<DuplicateGroup>> {
    use crate::fingerprint::{fingerprint_file, DEFAULT_FINGERPRINT_DURATION_SECS};

    // Get or compute the target's fingerprint
    let target = target
        .canonicalize()
        .unwrap_or_else(|_| target.to_path_buf());

    let target_fp = if let Some(cached) = db
        .load_all_fingerprints()?
        .into_iter()
        .find(|f| f.path == target)
    {
        cached
    } else {
        // Not in DB yet — fingerprint it now
        println!("Fingerprinting {}...", target.display());
        let result = fingerprint_file(&target, DEFAULT_FINGERPRINT_DURATION_SECS)?;
        db.store_fingerprint(&target, &result, DEFAULT_FINGERPRINT_DURATION_SECS)?;
        crate::database::FileFingerprint {
            path: target.clone(),
            duration_secs: result.duration_secs,
            fingerprint: result.fingerprint,
        }
    };

    // Compare against all other fingerprints in the DB
    let all_fps = db.load_all_fingerprints()?;
    let mut matches: Vec<DuplicateFile> = Vec::new();
    let mut max_score = 0.0_f64;

    for fp in &all_fps {
        if fp.path == target {
            continue;
        }

        // Duration filter
        if !durations_compatible(target_fp.duration_secs, fp.duration_secs) {
            continue;
        }

        // Fingerprint comparison
        let score = compare_fingerprints(&target_fp.fingerprint, &fp.fingerprint);
        if score >= threshold {
            max_score = max_score.max(score);
            matches.push(DuplicateFile {
                path: fp.path.clone(),
                duration_secs: fp.duration_secs,
                match_kind: MatchKind::Similar,
            });
        }
    }

    if matches.is_empty() {
        return Ok(Vec::new());
    }

    // Add the target file itself
    matches.insert(
        0,
        DuplicateFile {
            path: target,
            duration_secs: target_fp.duration_secs,
            match_kind: MatchKind::Similar,
        },
    );

    Ok(vec![DuplicateGroup {
        files: matches,
        similarity: max_score,
    }])
}

/// Generate candidate pairs using Locality-Sensitive Hashing (banding technique).
///
/// The fingerprint is divided into bands of consecutive sub-fingerprints.
/// For each band, we hash the sub-fingerprints together. If two fingerprints
/// share a band hash, they become a candidate pair.
///
/// This is O(n * num_bands) ≈ O(n) instead of O(n²).
#[cfg(test)]
fn generate_candidates_lsh(
    fps: &[crate::database::FileFingerprint],
    same_tree: bool,
) -> Vec<(usize, usize)> {
    use std::collections::HashSet;
    // For each band, build a hash table mapping band_hash -> list of fingerprint indices
    let mut candidate_set: HashSet<(usize, usize)> = HashSet::new();

    for band_idx in 0..LSH_NUM_BANDS {
        let mut band_buckets: HashMap<u64, Vec<usize>> = HashMap::new();
        let band_start = band_idx * LSH_ROWS_PER_BAND;

        for (fp_idx, fp) in fps.iter().enumerate() {
            if fp.fingerprint.len() < band_start + LSH_ROWS_PER_BAND {
                continue; // Fingerprint too short for this band
            }

            let band_hash = hash_band(&fp.fingerprint[band_start..band_start + LSH_ROWS_PER_BAND]);
            band_buckets.entry(band_hash).or_default().push(fp_idx);
        }

        // All fingerprints in the same bucket are candidate pairs
        for bucket in band_buckets.values() {
            if bucket.len() < 2 || bucket.len() > 1000 {
                // Skip very large buckets (likely noise/silence) to prevent O(n²) blowup
                continue;
            }
            for (idx_a, &i) in bucket.iter().enumerate() {
                for &j in &bucket[idx_a + 1..] {
                    let pair = if i < j { (i, j) } else { (j, i) };

                    // Pre-filter: duration compatibility (cheap check before adding to set)
                    if !durations_compatible(fps[i].duration_secs, fps[j].duration_secs) {
                        continue;
                    }

                    // Pre-filter: same-tree constraint
                    if same_tree && !shares_root(&fps[i].path, &fps[j].path) {
                        continue;
                    }

                    candidate_set.insert(pair);
                }
            }
        }
    }

    candidate_set.into_iter().collect()
}

/// Compute band hashes for a fingerprint (for DB pre-computation during scan).
pub fn compute_band_hashes(fingerprint: &[u32]) -> Vec<u64> {
    let mut hashes = Vec::with_capacity(LSH_NUM_BANDS);
    for band_idx in 0..LSH_NUM_BANDS {
        let band_start = band_idx * LSH_ROWS_PER_BAND;
        if fingerprint.len() < band_start + LSH_ROWS_PER_BAND {
            break;
        }
        hashes.push(hash_band(
            &fingerprint[band_start..band_start + LSH_ROWS_PER_BAND],
        ));
    }
    hashes
}

/// Hash a band (slice of sub-fingerprints) into a single u64 bucket key.
/// Uses FNV-1a for speed (we don't need cryptographic strength).
fn hash_band(band: &[u32]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325; // FNV offset basis
    for &value in band {
        for byte in value.to_le_bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x100000001b3); // FNV prime
        }
    }
    hash
}

/// Check if two paths share a common root directory.
fn shares_root(a: &std::path::Path, b: &std::path::Path) -> bool {
    // Find the first real directory component (skip root "/")
    let a_components: Vec<_> = a.components().collect();
    let b_components: Vec<_> = b.components().collect();

    // Need at least 2 components (root + first dir)
    if a_components.len() < 2 || b_components.len() < 2 {
        return false;
    }

    // Compare up to the first 2 non-root components
    a_components.iter().take(2).eq(b_components.iter().take(2))
}

/// Union-Find (Disjoint Set Union) data structure with path compression and union by rank.
pub struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl UnionFind {
    /// Create a new Union-Find structure with `n` disjoint elements.
    pub fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            rank: vec![0; n],
        }
    }

    /// Find with path compression (halving).
    pub fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            // Path halving: point to grandparent
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }

    /// Merge the sets containing `x` and `y` (union by rank).
    pub fn union(&mut self, x: usize, y: usize) {
        let rx = self.find(x);
        let ry = self.find(y);
        if rx == ry {
            return;
        }
        match self.rank[rx].cmp(&self.rank[ry]) {
            std::cmp::Ordering::Less => self.parent[rx] = ry,
            std::cmp::Ordering::Greater => self.parent[ry] = rx,
            std::cmp::Ordering::Equal => {
                self.parent[ry] = rx;
                self.rank[rx] += 1;
            }
        }
    }

    /// Return `true` if `x` and `y` are in the same set.
    pub fn connected(&mut self, x: usize, y: usize) -> bool {
        self.find(x) == self.find(y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::FileFingerprint;
    use std::path::PathBuf;

    #[test]
    fn test_union_find_basic() {
        let mut uf = UnionFind::new(5);
        assert!(!uf.connected(0, 1));
        uf.union(0, 1);
        assert!(uf.connected(0, 1));
        assert!(!uf.connected(0, 2));
    }

    #[test]
    fn test_union_find_transitive() {
        let mut uf = UnionFind::new(5);
        uf.union(0, 1);
        uf.union(1, 2);
        // 0, 1, 2 should all be connected transitively
        assert!(uf.connected(0, 2));
        assert!(!uf.connected(0, 3));
    }

    #[test]
    fn test_union_find_all_connected() {
        let mut uf = UnionFind::new(4);
        uf.union(0, 1);
        uf.union(2, 3);
        uf.union(1, 2);
        // All should be in same group
        assert!(uf.connected(0, 3));
    }

    #[test]
    fn test_lsh_identical_fingerprints_are_candidates() {
        let fp = vec![0x12345678u32; 200];
        let fps = vec![
            FileFingerprint {
                path: PathBuf::from("/music/a.flac"),
                duration_secs: 180.0,
                fingerprint: fp.clone(),
            },
            FileFingerprint {
                path: PathBuf::from("/music/b.mp3"),
                duration_secs: 180.5,
                fingerprint: fp.clone(),
            },
        ];

        let candidates = generate_candidates_lsh(&fps, false);
        assert_eq!(candidates.len(), 1);
        assert!(candidates.contains(&(0, 1)));
    }

    #[test]
    fn test_lsh_very_different_fingerprints_not_candidates() {
        let fp1 = vec![0x00000000u32; 200];
        let fp2 = vec![0xFFFFFFFFu32; 200];
        let fps = vec![
            FileFingerprint {
                path: PathBuf::from("/music/a.flac"),
                duration_secs: 180.0,
                fingerprint: fp1,
            },
            FileFingerprint {
                path: PathBuf::from("/music/b.mp3"),
                duration_secs: 180.0,
                fingerprint: fp2,
            },
        ];

        let candidates = generate_candidates_lsh(&fps, false);
        assert!(candidates.is_empty());
    }

    #[test]
    fn test_lsh_duration_filter_rejects_different_lengths() {
        let fp = vec![0x12345678u32; 200];
        let fps = vec![
            FileFingerprint {
                path: PathBuf::from("/music/a.flac"),
                duration_secs: 180.0, // 3 minutes
                fingerprint: fp.clone(),
            },
            FileFingerprint {
                path: PathBuf::from("/music/b.mp3"),
                duration_secs: 420.0, // 7 minutes
                fingerprint: fp.clone(),
            },
        ];

        let candidates = generate_candidates_lsh(&fps, false);
        // Should be rejected by duration filter even though fingerprints match
        assert!(candidates.is_empty());
    }

    #[test]
    fn test_lsh_same_tree_filter() {
        let fp = vec![0x12345678u32; 200];
        let fps = vec![
            FileFingerprint {
                path: PathBuf::from("/home/user/Music/a.flac"),
                duration_secs: 180.0,
                fingerprint: fp.clone(),
            },
            FileFingerprint {
                path: PathBuf::from("/mnt/external/b.mp3"),
                duration_secs: 180.0,
                fingerprint: fp.clone(),
            },
        ];

        let with_filter = generate_candidates_lsh(&fps, true);
        let without_filter = generate_candidates_lsh(&fps, false);
        assert!(with_filter.is_empty());
        assert_eq!(without_filter.len(), 1);
    }

    #[test]
    fn test_hash_band_deterministic() {
        let band = [0x12345678, 0xABCDEF01, 0x00FF00FF, 0xDEADBEEF];
        let h1 = hash_band(&band);
        let h2 = hash_band(&band);
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_hash_band_different_inputs() {
        let band1 = [0x12345678, 0xABCDEF01, 0x00FF00FF, 0xDEADBEEF];
        let band2 = [0x12345679, 0xABCDEF01, 0x00FF00FF, 0xDEADBEEF]; // one bit different
        assert_ne!(hash_band(&band1), hash_band(&band2));
    }

    #[test]
    fn test_shares_root_same_tree() {
        assert!(shares_root(
            &PathBuf::from("/home/user/Music/Artist/track.flac"),
            &PathBuf::from("/home/user/Music/Other/track.mp3"),
        ));
    }

    #[test]
    fn test_shares_root_different_tree() {
        assert!(!shares_root(
            &PathBuf::from("/home/user/Music/track.flac"),
            &PathBuf::from("/mnt/external/track.mp3"),
        ));
    }
}
