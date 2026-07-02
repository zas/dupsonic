//! Keep strategy for selecting which file to preserve in a duplicate group.

use anyhow::{bail, Result};
use regex::Regex;
use std::path::Path;
use std::str::FromStr;

use crate::matcher::DuplicateFile;
use crate::tags;

/// Strategy for deciding which file to keep in a duplicate group.
#[derive(Debug, Clone)]
pub enum KeepStrategy {
    /// Keep highest quality (lossless > lossy, higher sample rate, higher bit depth)
    Best,
    /// Keep files matching one of the given extensions
    Ext(Vec<String>),
    /// Keep files whose path matches the regex
    Regex(Regex),
    /// Keep the largest file
    Largest,
    /// Keep the smallest file
    Smallest,
    /// Keep the newest file (most recent mtime)
    Newest,
    /// Keep the oldest file
    Oldest,
}

impl FromStr for KeepStrategy {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        if s.eq_ignore_ascii_case("best") {
            Ok(KeepStrategy::Best)
        } else if s.eq_ignore_ascii_case("largest") {
            Ok(KeepStrategy::Largest)
        } else if s.eq_ignore_ascii_case("smallest") {
            Ok(KeepStrategy::Smallest)
        } else if s.eq_ignore_ascii_case("newest") {
            Ok(KeepStrategy::Newest)
        } else if s.eq_ignore_ascii_case("oldest") {
            Ok(KeepStrategy::Oldest)
        } else if let Some(exts) = s.strip_prefix("ext:") {
            let extensions: Vec<String> =
                exts.split(',').map(|e| e.trim().to_lowercase()).collect();
            if extensions.is_empty() || extensions.iter().any(|e| e.is_empty()) {
                bail!("Invalid ext: strategy, expected ext:flac or ext:flac,wav");
            }
            Ok(KeepStrategy::Ext(extensions))
        } else if let Some(pattern) = s.strip_prefix("iregex:") {
            // Case-insensitive regex
            let pattern = format!("(?i){}", pattern);
            let re = Regex::new(&pattern)
                .map_err(|e| anyhow::anyhow!("Invalid regex pattern '{}': {}", pattern, e))?;
            Ok(KeepStrategy::Regex(re))
        } else if let Some(pattern) = s.strip_prefix("regex:") {
            // Case-sensitive regex
            let re = Regex::new(pattern)
                .map_err(|e| anyhow::anyhow!("Invalid regex pattern '{}': {}", pattern, e))?;
            Ok(KeepStrategy::Regex(re))
        } else {
            bail!(
                "Unknown keep strategy '{}'. Expected: best, largest, smallest, newest, oldest, ext:<exts>, regex:<pattern>, iregex:<pattern>",
                s
            );
        }
    }
}

/// Lossless audio extensions.
const LOSSLESS_EXTENSIONS: &[&str] = &["flac", "wav", "aiff", "aif", "ape", "wv", "alac"];

/// Select the index of the file to keep in a group.
///
/// Returns `None` if no file matches the strategy (group should be skipped).
pub fn select_keeper(files: &[DuplicateFile], strategy: &KeepStrategy) -> Option<usize> {
    if files.is_empty() {
        return None;
    }

    match strategy {
        KeepStrategy::Best => select_best(files),
        KeepStrategy::Ext(exts) => {
            // Among files matching the extension, pick the best quality
            let matching: Vec<usize> = files
                .iter()
                .enumerate()
                .filter(|(_, f)| {
                    f.path
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(|e| exts.contains(&e.to_lowercase()))
                        .unwrap_or(false)
                })
                .map(|(i, _)| i)
                .collect();

            if matching.is_empty() {
                None
            } else if matching.len() == 1 {
                Some(matching[0])
            } else {
                // Multiple matches — pick the best among them
                let subset: Vec<DuplicateFile> =
                    matching.iter().map(|&i| files[i].clone()).collect();
                select_best(&subset).map(|i| matching[i])
            }
        }
        KeepStrategy::Regex(re) => {
            let matching: Vec<usize> = files
                .iter()
                .enumerate()
                .filter(|(_, f)| re.is_match(&f.path.to_string_lossy()))
                .map(|(i, _)| i)
                .collect();

            if matching.is_empty() {
                None
            } else if matching.len() == 1 {
                Some(matching[0])
            } else {
                let subset: Vec<DuplicateFile> =
                    matching.iter().map(|&i| files[i].clone()).collect();
                select_best(&subset).map(|i| matching[i])
            }
        }
        KeepStrategy::Largest => files
            .iter()
            .enumerate()
            .max_by_key(|(_, f)| file_size(&f.path))
            .map(|(i, _)| i),
        KeepStrategy::Smallest => files
            .iter()
            .enumerate()
            .min_by_key(|(_, f)| file_size(&f.path))
            .map(|(i, _)| i),
        KeepStrategy::Newest => files
            .iter()
            .enumerate()
            .max_by_key(|(_, f)| file_mtime(&f.path))
            .map(|(i, _)| i),
        KeepStrategy::Oldest => files
            .iter()
            .enumerate()
            .min_by_key(|(_, f)| file_mtime(&f.path))
            .map(|(i, _)| i),
    }
}

/// Select the best quality file from a group.
fn select_best(files: &[DuplicateFile]) -> Option<usize> {
    files
        .iter()
        .enumerate()
        .max_by_key(|(_, f)| quality_score(f))
        .map(|(i, _)| i)
}

/// Compute a quality score for ranking. Higher is better.
fn quality_score(file: &DuplicateFile) -> (u8, u32, u32, u64) {
    let ext = file
        .path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    // Tier: lossless = 1, lossy = 0
    let lossless: u8 = if LOSSLESS_EXTENSIONS.contains(&ext.as_str()) {
        1
    } else {
        0
    };

    // Get audio info for sample rate and bit depth
    let (sample_rate, bits_per_sample) = tags::read_audio_info(&file.path)
        .ok()
        .flatten()
        .map(|info| {
            (
                info.sample_rate.unwrap_or(0),
                info.bits_per_sample.unwrap_or(0),
            )
        })
        .unwrap_or((0, 0));

    let size = file_size(&file.path);

    // Sort by: lossless > sample_rate > bit_depth > file_size
    (lossless, sample_rate, bits_per_sample, size)
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn file_mtime(path: &Path) -> i64 {
    use std::time::UNIX_EPOCH;
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
