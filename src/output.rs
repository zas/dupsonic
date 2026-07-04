//! Output formatting for duplicate results.
//!
//! Supports human-readable tables, JSON (for tool integration), and JSON Lines
//! (one object per group, for streaming consumption).

use anyhow::Result;
use serde::Serialize;
use std::path::Path;
use std::str::FromStr;

use crate::database::Database;
use crate::matcher::{DuplicateFile, DuplicateGroup, MatchKind};
use crate::tags;

/// Output format for duplicate results.
#[derive(Debug, Clone, Copy, Default)]
pub enum Format {
    /// Human-readable table output
    #[default]
    Human,
    /// JSON output (for Picard plugin integration)
    Json,
    /// JSON Lines (one JSON object per duplicate group)
    Jsonl,
}

impl FromStr for Format {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "human" | "text" | "table" => Ok(Format::Human),
            "json" => Ok(Format::Json),
            "jsonl" | "ndjson" => Ok(Format::Jsonl),
            _ => Err(format!(
                "Unknown format '{}'. Expected: human, json, jsonl",
                s
            )),
        }
    }
}

/// Compact JSON output (default).
#[derive(Serialize)]
struct JsonGroup {
    id: String,
    similarity: f64,
    files: Vec<JsonFile>,
}

#[derive(Serialize)]
struct JsonFile {
    path: String,
    duration_secs: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    match_kind: Option<String>,
}

/// Detailed JSON output (with --details).
#[derive(Serialize)]
struct DetailedJsonGroup {
    id: String,
    similarity: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    recording_mbid: Option<String>,
    files: Vec<DetailedJsonFile>,
}

#[derive(Serialize)]
struct DetailedJsonFile {
    path: String,
    duration_secs: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    match_kind: Option<String>,
    format: String,
    size_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    sample_rate: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bits_per_sample: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    channels: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bitrate_kbps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recording_mbid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    acoustid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tags: Option<FileTags>,
}

#[derive(Serialize)]
struct FileTags {
    #[serde(skip_serializing_if = "Option::is_none")]
    artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    album: Option<String>,
}

/// Print duplicate groups in the specified format.
pub fn print_results(
    groups: &[DuplicateGroup],
    format: Format,
    details: bool,
    db: Option<&Database>,
) -> Result<()> {
    if groups.is_empty() {
        match format {
            Format::Human => println!("No duplicates found."),
            Format::Json => println!("[]"),
            Format::Jsonl => {} // no output
        }
        return Ok(());
    }

    if details {
        match format {
            Format::Human => print_human_detailed(groups, db),
            Format::Json => print_json_detailed(groups, db)?,
            Format::Jsonl => print_jsonl_detailed(groups, db)?,
        }
    } else {
        match format {
            Format::Human => print_human(groups),
            Format::Json => print_json(groups)?,
            Format::Jsonl => print_jsonl(groups)?,
        }
    }

    Ok(())
}

fn print_human(groups: &[DuplicateGroup]) {
    for group in groups {
        println!(
            "── Duplicate Group {} ({} files, {:.0}% similar) ──",
            &group.id[..8],
            group.files.len(),
            group.similarity * 100.0,
        );
        for file in &group.files {
            let duration = format_duration(file.duration_secs);
            let label = format_score_label(file);
            if label.is_empty() {
                println!("  {} ({})", file.path.display(), duration);
            } else {
                println!("  {} {} ({})", label, file.path.display(), duration);
            }
        }
        println!();
    }

    let total_dupes: usize = groups.iter().map(|g| g.files.len() - 1).sum();
    println!(
        "Summary: {} duplicate groups, {} redundant files",
        groups.len(),
        total_dupes
    );
}

fn print_human_detailed(groups: &[DuplicateGroup], db: Option<&Database>) {
    for group in groups {
        println!(
            "── Duplicate Group {} ({} files, {:.0}% similar) ──",
            &group.id[..8],
            group.files.len(),
            group.similarity * 100.0,
        );
        for file in &group.files {
            let duration = format_duration(file.duration_secs);
            let size = file_size(&file.path);
            let ext = file_extension(&file.path);
            let mbid = db.and_then(|d| d.get_recording_mbid(&file.path).ok().flatten());
            let audio_info = tags::read_audio_info(&file.path).ok().flatten();

            let audio_detail = if let Some(ref info) = audio_info {
                let rate = info
                    .sample_rate
                    .map(|r| format!("{}kHz", r / 1000))
                    .unwrap_or_default();
                let bits = info
                    .bits_per_sample
                    .map(|b| format!("/{}bit", b))
                    .unwrap_or_default();
                let bitrate = info
                    .bitrate_kbps
                    .map(|b| format!(" ~{}kbps", b))
                    .unwrap_or_default();
                format!(" {}{}{}", rate, bits, bitrate)
            } else {
                String::new()
            };

            let label = format_score_label(file);
            if label.is_empty() {
                println!(
                    "  {} ({}, {},{} {})",
                    file.path.display(),
                    duration,
                    ext.to_uppercase(),
                    audio_detail,
                    format_size(size),
                );
            } else {
                println!(
                    "  {} {} ({}, {},{} {})",
                    label,
                    file.path.display(),
                    duration,
                    ext.to_uppercase(),
                    audio_detail,
                    format_size(size),
                );
            }
            if let Some(mbid) = mbid {
                println!("         MBID: {}", mbid);
            }
        }
        println!();
    }

    let total_dupes: usize = groups.iter().map(|g| g.files.len() - 1).sum();
    println!(
        "Summary: {} duplicate groups, {} redundant files",
        groups.len(),
        total_dupes
    );
}

fn build_detailed_group(group: &DuplicateGroup, db: Option<&Database>) -> DetailedJsonGroup {
    let files: Vec<DetailedJsonFile> = group
        .files
        .iter()
        .map(|f| {
            let (recording_mbid, acoustid) = db
                .map(|d| {
                    let mbid = d.get_recording_mbid(&f.path).ok().flatten();
                    let aid = get_acoustid(d, &f.path);
                    (mbid, aid)
                })
                .unwrap_or((None, None));

            let file_tags = tags::read_basic_tags(&f.path).ok().flatten();
            let audio_info = tags::read_audio_info(&f.path).ok().flatten();

            DetailedJsonFile {
                path: f.path.to_string_lossy().into_owned(),
                duration_secs: f.duration_secs,
                match_kind: match_kind_str(f.match_kind),
                format: file_extension(&f.path),
                size_bytes: file_size(&f.path),
                sample_rate: audio_info.as_ref().and_then(|a| a.sample_rate),
                bits_per_sample: audio_info.as_ref().and_then(|a| a.bits_per_sample),
                channels: audio_info.as_ref().and_then(|a| a.channels),
                bitrate_kbps: audio_info.as_ref().and_then(|a| a.bitrate_kbps),
                recording_mbid,
                acoustid,
                tags: file_tags.map(|t| FileTags {
                    artist: t.artist,
                    title: t.title,
                    album: t.album,
                }),
            }
        })
        .collect();

    // If all files share the same MBID, promote it to group level
    let group_mbid = {
        let mbids: Vec<&str> = files
            .iter()
            .filter_map(|f| f.recording_mbid.as_deref())
            .collect();
        if mbids.len() == files.len() && !mbids.is_empty() && mbids.iter().all(|m| *m == mbids[0]) {
            Some(mbids[0].to_string())
        } else {
            None
        }
    };

    DetailedJsonGroup {
        id: group.id.clone(),
        similarity: group.similarity,
        recording_mbid: group_mbid,
        files,
    }
}

fn print_json_detailed(groups: &[DuplicateGroup], db: Option<&Database>) -> Result<()> {
    let json_groups: Vec<DetailedJsonGroup> =
        groups.iter().map(|g| build_detailed_group(g, db)).collect();

    println!("{}", serde_json::to_string_pretty(&json_groups)?);
    Ok(())
}

fn print_jsonl_detailed(groups: &[DuplicateGroup], db: Option<&Database>) -> Result<()> {
    for group in groups {
        let json_group = build_detailed_group(group, db);
        println!("{}", serde_json::to_string(&json_group)?);
    }
    Ok(())
}

fn print_json(groups: &[DuplicateGroup]) -> Result<()> {
    let json_groups: Vec<JsonGroup> = groups
        .iter()
        .map(|g| JsonGroup {
            id: g.id.clone(),
            similarity: g.similarity,
            files: g
                .files
                .iter()
                .map(|f| JsonFile {
                    path: f.path.to_string_lossy().into_owned(),
                    duration_secs: f.duration_secs,
                    match_kind: match_kind_str(f.match_kind),
                })
                .collect(),
        })
        .collect();

    println!("{}", serde_json::to_string_pretty(&json_groups)?);
    Ok(())
}

fn print_jsonl(groups: &[DuplicateGroup]) -> Result<()> {
    for group in groups {
        let json_group = JsonGroup {
            id: group.id.clone(),
            similarity: group.similarity,
            files: group
                .files
                .iter()
                .map(|f| JsonFile {
                    path: f.path.to_string_lossy().into_owned(),
                    duration_secs: f.duration_secs,
                    match_kind: match_kind_str(f.match_kind),
                })
                .collect(),
        };
        println!("{}", serde_json::to_string(&json_group)?);
    }
    Ok(())
}

fn format_duration(secs: f64) -> String {
    let total_secs = secs as u64;
    let minutes = total_secs / 60;
    let seconds = total_secs % 60;
    format!("{}:{:02}", minutes, seconds)
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn file_extension(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("unknown")
        .to_lowercase()
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

fn format_score_label(file: &DuplicateFile) -> String {
    match file.match_kind {
        MatchKind::ExactCopy => "[exact copy]".to_string(),
        MatchKind::SameAudio => "[same audio]".to_string(),
        MatchKind::Similar => String::new(),
    }
}

fn match_kind_str(kind: MatchKind) -> Option<String> {
    match kind {
        MatchKind::ExactCopy => Some("exact_copy".to_string()),
        MatchKind::SameAudio => Some("same_audio".to_string()),
        MatchKind::Similar => None,
    }
}

fn get_acoustid(db: &Database, path: &Path) -> Option<String> {
    // Query the acoustid column directly
    db.get_acoustid(path).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::{DuplicateFile, DuplicateGroup};
    use std::path::PathBuf;

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(0.0), "0:00");
        assert_eq!(format_duration(59.0), "0:59");
        assert_eq!(format_duration(60.0), "1:00");
        assert_eq!(format_duration(61.5), "1:01");
        assert_eq!(format_duration(222.0), "3:42");
        assert_eq!(format_duration(3661.0), "61:01");
    }

    #[test]
    fn test_format_parse() {
        assert!(matches!("human".parse::<Format>(), Ok(Format::Human)));
        assert!(matches!("json".parse::<Format>(), Ok(Format::Json)));
        assert!(matches!("jsonl".parse::<Format>(), Ok(Format::Jsonl)));
        assert!(matches!("ndjson".parse::<Format>(), Ok(Format::Jsonl)));
        assert!(matches!("text".parse::<Format>(), Ok(Format::Human)));
        assert!("invalid".parse::<Format>().is_err());
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1024), "1 KB");
        assert_eq!(format_size(1_048_576), "1.0 MB");
        assert_eq!(format_size(34_567_890), "33.0 MB");
    }

    #[test]
    fn test_json_output_structure() {
        let groups = vec![DuplicateGroup {
            id: "abcd1234".to_string(),
            similarity: 0.95,
            files: vec![
                DuplicateFile {
                    path: PathBuf::from("/music/a.flac"),
                    duration_secs: 222.0,
                    match_kind: crate::matcher::MatchKind::Similar,
                },
                DuplicateFile {
                    path: PathBuf::from("/music/b.mp3"),
                    duration_secs: 221.0,
                    match_kind: crate::matcher::MatchKind::Similar,
                },
            ],
        }];

        // Test that compact output works without db
        print_results(&groups, Format::Json, false, None).unwrap();
    }

    #[test]
    fn test_empty_results() {
        print_results(&[], Format::Human, false, None).unwrap();
        print_results(&[], Format::Json, false, None).unwrap();
        print_results(&[], Format::Jsonl, false, None).unwrap();
    }
}
