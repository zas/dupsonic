use anyhow::Result;
use serde::Serialize;
use std::str::FromStr;

use crate::matcher::DuplicateGroup;

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

/// Serializable representation of results for JSON output.
#[derive(Serialize)]
struct JsonGroup {
    group_id: usize,
    files: Vec<JsonFile>,
}

#[derive(Serialize)]
struct JsonFile {
    path: String,
    duration_secs: f64,
    similarity: f64,
}

/// Print duplicate groups in the specified format.
pub fn print_results(groups: &[DuplicateGroup], format: Format) -> Result<()> {
    if groups.is_empty() {
        match format {
            Format::Human => println!("No duplicates found."),
            Format::Json => println!("[]"),
            Format::Jsonl => {} // no output
        }
        return Ok(());
    }

    match format {
        Format::Human => print_human(groups),
        Format::Json => print_json(groups)?,
        Format::Jsonl => print_jsonl(groups)?,
    }

    Ok(())
}

fn print_human(groups: &[DuplicateGroup]) {
    for (i, group) in groups.iter().enumerate() {
        println!("── Duplicate Group {} ({} files) ──", i + 1, group.files.len());
        for file in &group.files {
            let duration = format_duration(file.duration_secs);
            println!(
                "  [{:.0}%] {} ({})",
                file.score * 100.0,
                file.path.display(),
                duration,
            );
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

fn print_json(groups: &[DuplicateGroup]) -> Result<()> {
    let json_groups: Vec<JsonGroup> = groups
        .iter()
        .enumerate()
        .map(|(i, g)| JsonGroup {
            group_id: i + 1,
            files: g
                .files
                .iter()
                .map(|f| JsonFile {
                    path: f.path.to_string_lossy().into_owned(),
                    duration_secs: f.duration_secs,
                    similarity: f.score,
                })
                .collect(),
        })
        .collect();

    println!("{}", serde_json::to_string_pretty(&json_groups)?);
    Ok(())
}

fn print_jsonl(groups: &[DuplicateGroup]) -> Result<()> {
    for (i, group) in groups.iter().enumerate() {
        let json_group = JsonGroup {
            group_id: i + 1,
            files: group
                .files
                .iter()
                .map(|f| JsonFile {
                    path: f.path.to_string_lossy().into_owned(),
                    duration_secs: f.duration_secs,
                    similarity: f.score,
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
    fn test_json_output_structure() {
        let groups = vec![DuplicateGroup {
            files: vec![
                DuplicateFile {
                    path: PathBuf::from("/music/a.flac"),
                    duration_secs: 222.0,
                    score: 1.0,
                },
                DuplicateFile {
                    path: PathBuf::from("/music/b.mp3"),
                    duration_secs: 221.0,
                    score: 0.95,
                },
            ],
        }];

        let json_groups: Vec<JsonGroup> = groups
            .iter()
            .enumerate()
            .map(|(i, g)| JsonGroup {
                group_id: i + 1,
                files: g
                    .files
                    .iter()
                    .map(|f| JsonFile {
                        path: f.path.to_string_lossy().into_owned(),
                        duration_secs: f.duration_secs,
                        similarity: f.score,
                    })
                    .collect(),
            })
            .collect();

        let json_str = serde_json::to_string(&json_groups).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        assert!(parsed.is_array());
        let arr = parsed.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["group_id"], 1);
        assert_eq!(arr[0]["files"].as_array().unwrap().len(), 2);
        assert_eq!(arr[0]["files"][0]["similarity"], 1.0);
        assert_eq!(arr[0]["files"][1]["similarity"], 0.95);
    }

    #[test]
    fn test_empty_results() {
        // Should not panic
        print_results(&[], Format::Human).unwrap();
        print_results(&[], Format::Json).unwrap();
        print_results(&[], Format::Jsonl).unwrap();
    }
}
