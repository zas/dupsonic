use anyhow::Result;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;
use tracing::info;

use crate::database::Database;
use crate::fingerprint::{compare_fingerprints, durations_compatible};

/// A group of files that are acoustic duplicates of each other.
#[derive(Debug, Clone)]
pub struct DuplicateGroup {
    /// Files in this group, sorted by path
    pub files: Vec<DuplicateFile>,
}

/// A single file within a duplicate group.
#[derive(Debug, Clone)]
pub struct DuplicateFile {
    pub path: std::path::PathBuf,
    pub duration_secs: f64,
    /// Similarity score to the group's reference file (first file)
    pub score: f64,
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
    // Try DB-based candidate generation first (low memory)
    if db.has_band_hashes()? {
        return find_duplicates_from_db(db, threshold, same_tree);
    }

    // Fallback: in-memory LSH (backward compat with old DBs without band_hashes)
    find_duplicates_in_memory(db, threshold, same_tree)
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
            let mut files: Vec<DuplicateFile> = members
                .iter()
                .map(|path| {
                    let fp = db.load_fingerprint(path).ok().flatten();
                    let score = match (&ref_fp, &fp) {
                        (Some(r), Some(f)) if path != &members[0] => {
                            compare_fingerprints(&r.fingerprint, &f.fingerprint)
                        }
                        _ => 1.0,
                    };
                    let duration = fp.map(|f| f.duration_secs).unwrap_or(0.0);
                    DuplicateFile {
                        path: path.clone(),
                        duration_secs: duration,
                        score,
                    }
                })
                .collect();
            files.sort_by(|a, b| a.path.cmp(&b.path));
            DuplicateGroup { files }
        })
        .collect();

    groups.sort_by_key(|g| std::cmp::Reverse(g.files.len()));

    println!("Found {} duplicate groups", groups.len());
    Ok(groups)
}

/// In-memory LSH candidate generation (original approach, for backward compatibility).
fn find_duplicates_in_memory(
    db: &Database,
    threshold: f64,
    same_tree: bool,
) -> Result<Vec<DuplicateGroup>> {
    let all_fps = db.load_all_fingerprints()?;
    let n = all_fps.len();
    info!("Loaded {} fingerprints", n);

    if n < 2 {
        println!("Not enough fingerprinted files to compare.");
        return Ok(Vec::new());
    }

    // Phase 1: Generate candidate pairs using LSH banding
    println!("Phase 1: Finding candidate pairs (LSH)...");
    let candidates = generate_candidates_lsh(&all_fps, same_tree);
    println!(
        "  Found {} candidate pairs (from {} possible, {:.2}% reduction)",
        candidates.len(),
        n * (n - 1) / 2,
        (1.0 - candidates.len() as f64 / (n * (n - 1) / 2) as f64) * 100.0
    );

    if candidates.is_empty() {
        println!("No duplicates found.");
        return Ok(Vec::new());
    }

    // Phase 2: Verify candidates with full fingerprint comparison + duration check
    println!("Phase 2: Verifying {} candidates...", candidates.len());
    let pb = ProgressBar::new(candidates.len() as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template(
                "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta})",
            )
            .expect("valid template")
            .progress_chars("█▓░"),
    );

    let uf = Mutex::new(UnionFind::new(n));

    candidates.par_iter().for_each(|&(i, j)| {
        // Full fingerprint comparison (duration already filtered during candidate generation)
        let score = compare_fingerprints(&all_fps[i].fingerprint, &all_fps[j].fingerprint);
        if score >= threshold {
            uf.lock().unwrap().union(i, j);
        }
        pb.inc(1);
    });

    pb.finish_and_clear();

    // Phase 3: Collect groups from Union-Find
    let mut uf = uf.into_inner().unwrap();
    let mut groups_map: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let root = uf.find(i);
        groups_map.entry(root).or_default().push(i);
    }

    // Build result, only keep groups with 2+ files
    let mut groups: Vec<DuplicateGroup> = groups_map
        .into_values()
        .filter(|members| members.len() > 1)
        .map(|members| {
            let reference_fp = &all_fps[members[0]].fingerprint;
            let mut files: Vec<DuplicateFile> = members
                .iter()
                .map(|&idx| {
                    let score = if idx == members[0] {
                        1.0
                    } else {
                        compare_fingerprints(&all_fps[idx].fingerprint, reference_fp)
                    };
                    DuplicateFile {
                        path: all_fps[idx].path.clone(),
                        duration_secs: all_fps[idx].duration_secs,
                        score,
                    }
                })
                .collect();
            files.sort_by(|a, b| a.path.cmp(&b.path));
            DuplicateGroup { files }
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
            matches.push(DuplicateFile {
                path: fp.path.clone(),
                duration_secs: fp.duration_secs,
                score,
            });
        }
    }

    if matches.is_empty() {
        return Ok(Vec::new());
    }

    // Add the target file itself as the reference
    matches.insert(
        0,
        DuplicateFile {
            path: target,
            duration_secs: target_fp.duration_secs,
            score: 1.0,
        },
    );

    matches.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(vec![DuplicateGroup { files: matches }])
}

/// Generate candidate pairs using Locality-Sensitive Hashing (banding technique).
///
/// The fingerprint is divided into bands of consecutive sub-fingerprints.
/// For each band, we hash the sub-fingerprints together. If two fingerprints
/// share a band hash, they become a candidate pair.
///
/// This is O(n * num_bands) ≈ O(n) instead of O(n²).
fn generate_candidates_lsh(
    fps: &[crate::database::FileFingerprint],
    same_tree: bool,
) -> Vec<(usize, usize)> {
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
