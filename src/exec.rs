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
/// If `confirm` is false, only shows what would happen (dry run).
pub fn run(
    groups: &[DuplicateGroup],
    strategy: &KeepStrategy,
    command: &str,
    confirm: bool,
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
                let expanded_cmd =
                    command.replace("{}", &shell_quote(&file.path.to_string_lossy()));
                if confirm {
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
    if confirm {
        println!(
            "Done: {} executed, {} skipped, {} errors",
            total_exec, total_skipped, errors
        );
    } else {
        println!(
            "Dry run: would execute on {} files ({} groups skipped)",
            total_exec, total_skipped
        );
        println!("Run with --confirm to apply.");
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
fn shell_quote(path: &str) -> String {
    // Use single quotes, escaping any single quotes within the path
    format!("'{}'", path.replace('\'', "'\\''"))
}
