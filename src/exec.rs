//! Execute commands on duplicate files (the non-kept ones).

use anyhow::Result;
use std::process::Command;

use crate::keep::{select_keeper, KeepStrategy};
use crate::matcher::DuplicateGroup;

/// Run `--exec` on duplicate groups.
///
/// For each group, selects the file to keep based on the strategy,
/// then runs the command on all other files.
///
/// If `apply` is false, only shows what would happen (dry run).
pub fn run(
    groups: &[DuplicateGroup],
    strategy: &KeepStrategy,
    command: &str,
    apply: bool,
) -> Result<()> {
    if groups.is_empty() {
        println!("No duplicates found.");
        return Ok(());
    }

    let mut total_exec = 0;
    let mut total_skipped = 0;
    let mut errors = 0;

    for (i, group) in groups.iter().enumerate() {
        let keeper_idx = match select_keeper(&group.files, strategy) {
            Some(idx) => idx,
            None => {
                println!("Group {}: SKIP (no file matches --keep strategy)", i + 1);
                total_skipped += 1;
                continue;
            }
        };

        println!("Group {}:", i + 1);
        for (j, file) in group.files.iter().enumerate() {
            if j == keeper_idx {
                println!("  KEEP  {}", file.path.display());
            } else {
                let quoted = match shell_quote(&file.path.to_string_lossy()) {
                    Some(q) => q,
                    None => {
                        eprintln!(
                            "  SKIP  {} (path contains control characters)",
                            file.path.display()
                        );
                        errors += 1;
                        continue;
                    }
                };
                let expanded_cmd = command.replace("{}", &quoted);
                if apply {
                    println!("  EXEC  {}", expanded_cmd);
                    match execute_command(&expanded_cmd) {
                        Ok(true) => {
                            total_exec += 1;
                        }
                        Ok(false) => {
                            eprintln!("  ERROR: command failed for {}", file.path.display());
                            errors += 1;
                        }
                        Err(e) => {
                            eprintln!("  ERROR: {}", e);
                            errors += 1;
                        }
                    }
                } else {
                    println!("  EXEC  {} (dry-run)", expanded_cmd);
                    total_exec += 1;
                }
            }
        }
    }

    println!();
    if apply {
        println!(
            "Done: {} executed, {} skipped, {} errors",
            total_exec, total_skipped, errors
        );
    } else {
        println!(
            "Dry run: would execute on {} files ({} groups skipped)",
            total_exec, total_skipped
        );
        println!("Run with --apply to apply.");
    }

    Ok(())
}

/// Execute a shell command. Returns Ok(true) if exit code is 0.
fn execute_command(cmd: &str) -> Result<bool> {
    let status = if cfg!(target_os = "windows") {
        Command::new("cmd").args(["/C", cmd]).status()?
    } else {
        Command::new("sh").args(["-c", cmd]).status()?
    };
    Ok(status.success())
}

/// Shell-quote a path for safe interpolation into a shell command.
/// Returns None if the path contains control characters (newlines, etc.)
/// that could cause surprising shell behavior.
fn shell_quote(path: &str) -> Option<String> {
    // Reject paths with control characters (newlines, tabs, null bytes, etc.)
    // These are extremely rare in real music files and could cause unexpected
    // command behavior even with proper quoting.
    if path.chars().any(|c| c.is_control()) {
        return None;
    }
    // Use single quotes, escaping any single quotes within the path
    Some(format!("'{}'", path.replace('\'', "'\\''")))
}
