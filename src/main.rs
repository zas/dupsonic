use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use directories::ProjectDirs;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

use dupsonic::{database, matcher, output, scanner};

#[derive(Parser)]
#[command(
    name = "dupsonic",
    about = "Find duplicate audio files using acoustic fingerprinting",
    version
)]
struct Cli {
    /// Path to the fingerprint cache database
    #[arg(long, env = "DUPSONIC_DB")]
    db: Option<PathBuf>,

    /// Verbosity level (-v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Suppress progress output
    #[arg(short, long)]
    quiet: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Scan directories and fingerprint audio files
    Scan {
        /// Directories to scan for audio files (omit to re-scan previously scanned paths)
        paths: Vec<PathBuf>,

        /// Number of parallel workers for fingerprinting
        #[arg(short = 'j', long, default_value_t = num_cpus())]
        jobs: usize,

        /// Max audio duration in seconds to fingerprint (default 120)
        #[arg(short, long, default_value_t = 120)]
        length: u64,

        /// Ignore paths matching gitignore-style patterns (repeatable)
        #[arg(short, long)]
        ignore: Vec<String>,

        /// Force re-fingerprinting of already-scanned files
        #[arg(long)]
        force: bool,

        /// Remove paths from stored scan paths instead of scanning
        #[arg(long)]
        remove: bool,

        /// List stored scan paths
        #[arg(long)]
        list: bool,
    },

    /// Find duplicate audio files from the fingerprint database
    FindDupes {
        /// Minimum similarity score (0.0 to 1.0) to consider files as duplicates
        #[arg(short, long, default_value_t = 0.8)]
        threshold: f64,

        /// Only compare files within the same directory tree
        #[arg(long)]
        same_tree: bool,

        /// Output format
        #[arg(short, long, default_value = "human")]
        format: output::Format,

        /// Find duplicates of a specific file only
        #[arg(long, value_name = "PATH")]
        r#for: Option<PathBuf>,

        /// Include file details in output (size, format, tags, MBIDs)
        #[arg(long)]
        details: bool,

        /// Command to run on duplicate files ({} = file path)
        #[arg(long, value_name = "COMMAND")]
        exec: Option<String>,

        /// Strategy to decide which file to keep (default: best)
        #[arg(long, default_value = "best")]
        keep: String,

        /// Actually execute --exec (default is dry-run)
        #[arg(long)]
        apply: bool,

        /// Don't filter groups by recording MBIDs
        #[arg(long)]
        no_mbid_filter: bool,
    },

    /// Show scan status and database statistics
    Status,

    /// Remove entries for files that no longer exist, or matching patterns
    CleanCache {
        /// Gitignore-style patterns to match paths to remove (e.g. "**/Podcasts/**")
        #[arg()]
        patterns: Vec<String>,
    },

    /// Identify recordings via MusicBrainz tags and AcoustID lookup
    Identify {
        /// AcoustID API key (or set ACOUSTID_API_KEY env var)
        #[arg(long, env = "ACOUSTID_API_KEY")]
        api_key: Option<String>,

        /// Identify all files, not just those in duplicate groups
        #[arg(long)]
        all: bool,

        /// Minimum similarity threshold for duplicate detection
        #[arg(short, long, default_value_t = 0.8)]
        threshold: f64,
    },

    /// Exclude files from duplicate results
    Exclude {
        /// Files to exclude
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },

    /// Re-include previously excluded files
    Include {
        /// Files to re-include (or --all to clear all exclusions)
        #[arg(required_unless_present = "all")]
        paths: Vec<PathBuf>,

        /// Clear all exclusions
        #[arg(long)]
        all: bool,
    },

    /// Drop-in replacement for fpcalc (compatible with Picard)
    #[command(name = "fpcalc")]
    Fpcalc {
        /// Audio file to fingerprint
        file: PathBuf,

        /// Output as JSON (default, matches fpcalc -json)
        #[arg(short, long, default_value_t = true)]
        json: bool,

        /// Max audio duration in seconds to fingerprint
        #[arg(short, long, default_value_t = 120)]
        length: u64,
    },

    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },

    /// Start web UI server
    Serve {
        /// Address to bind to
        #[arg(short, long, default_value = "0.0.0.0:8080")]
        bind: String,
    },
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// Get the default database path using platform-appropriate data directories:
/// - Linux: ~/.local/share/dupsonic/cache.db
/// - macOS: ~/Library/Application Support/dupsonic/cache.db
/// - Windows: C:\Users\<user>\AppData\Roaming\dupsonic\cache.db
fn default_db_path() -> PathBuf {
    if let Some(proj_dirs) = ProjectDirs::from("org", "metabrainz", "dupsonic") {
        proj_dirs.data_dir().join("cache.db")
    } else {
        // Fallback if home dir can't be determined
        PathBuf::from("dupsonic-cache.db")
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let filter = match cli.verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    // Demote symphonia's internal tracing to debug level. Its messages
    // (e.g. "probe reached EOF at 4096 bytes") lack file context and confuse users.
    // We emit our own warnings with filename and user-friendly explanations instead.
    let filter_str = format!("{filter},symphonia_core=debug,symphonia_format_ogg=debug");
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&filter_str)),
        )
        .init();

    let custom_db = cli.db.is_some();
    let db_path = cli.db.unwrap_or_else(default_db_path);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let db = database::Database::open(&db_path)?;

    match cli.command {
        Commands::Scan {
            paths,
            jobs,
            length,
            ignore,
            force,
            remove,
            list,
        } => {
            if list {
                let stored = db.load_scan_paths()?;
                if stored.is_empty() {
                    println!("No stored scan paths.");
                } else {
                    println!("Stored scan paths:");
                    for p in &stored {
                        println!("  {}", p.display());
                    }
                }
                return Ok(());
            }

            if remove {
                if paths.is_empty() {
                    eprintln!("Specify paths to remove: dupsonic scan --remove <path>");
                    std::process::exit(1);
                }
                for path in &paths {
                    if db.remove_scan_path(path)? {
                        println!("Removed: {}", path.display());
                    } else {
                        println!("Not found: {}", path.display());
                    }
                }
                return Ok(());
            }

            let paths = if paths.is_empty() {
                let stored = db.load_scan_paths()?;
                if stored.is_empty() {
                    // Fall back to platform-specific music directory
                    if let Some(music_dir) = directories::UserDirs::new()
                        .and_then(|u| u.audio_dir().map(|p| p.to_path_buf()))
                    {
                        if music_dir.exists() {
                            eprintln!(
                                "No stored paths. Found default music directory: {}",
                                music_dir.display()
                            );
                            eprint!("Scan it? [Y/n] ");
                            let mut answer = String::new();
                            std::io::stdin().read_line(&mut answer).ok();
                            let answer = answer.trim().to_lowercase();
                            if answer.is_empty() || answer == "y" || answer == "yes" {
                                vec![music_dir]
                            } else {
                                std::process::exit(0);
                            }
                        } else {
                            eprintln!(
                                "No paths specified and default music directory does not exist."
                            );
                            eprintln!();
                            eprintln!("Usage: dupsonic scan <path-to-music>");
                            std::process::exit(1);
                        }
                    } else {
                        eprintln!("No paths specified and no previously scanned paths found.");
                        eprintln!();
                        eprintln!("Usage: dupsonic scan <path-to-music>");
                        std::process::exit(1);
                    }
                } else {
                    if !cli.quiet {
                        eprintln!("Re-scanning {} stored path(s):", stored.len());
                        for p in &stored {
                            eprintln!("  {}", p.display());
                        }
                    }
                    stored
                }
            } else {
                paths
            };
            scanner::scan(&db, &paths, jobs, length, &ignore, force, cli.quiet)?;
        }
        Commands::FindDupes {
            threshold,
            same_tree,
            format,
            r#for,
            details,
            exec,
            keep,
            apply,
            no_mbid_filter,
        } => {
            // Check if there are enough fingerprints before running the matcher
            if r#for.is_none() {
                let stats = db.stats()?;
                if stats.fingerprinted < 2 {
                    if stats.fingerprinted == 0 {
                        eprintln!("No files have been scanned yet.");
                        eprintln!();
                        eprintln!("Run a scan first to fingerprint your audio library:");
                        eprintln!();
                        eprintln!("  dupsonic scan <path-to-music>");
                    } else {
                        eprintln!(
                            "Only {} file has been scanned — need at least 2 to find duplicates.",
                            stats.fingerprinted
                        );
                        eprintln!();
                        eprintln!("Scan more files:");
                        eprintln!();
                        eprintln!("  dupsonic scan <path-to-music>");
                    }
                    if custom_db {
                        eprintln!();
                        eprintln!(
                            "Note: using custom database '{}'. Make sure you use the same --db path when scanning.",
                            db_path.display()
                        );
                    }
                    std::process::exit(1);
                }
            }

            let groups = if let Some(ref target) = r#for {
                matcher::find_duplicates_for(&db, target, threshold)?
            } else {
                matcher::find_duplicates(&db, threshold, same_tree)?
            };

            // Filter out groups where MBIDs prove they're different recordings
            let mut groups = if no_mbid_filter {
                groups
            } else {
                matcher::filter_by_mbids(groups, &db)
            };

            // Classify 100% fingerprint matches as exact copies or same-audio
            matcher::classify_matches(&mut groups, &db);

            if let Some(ref cmd) = exec {
                let strategy: dupsonic::keep::KeepStrategy = keep.parse()?;
                dupsonic::exec::run(&groups, &strategy, cmd, apply)?;
            } else {
                let db_ref = if details { Some(&db) } else { None };
                output::print_results(&groups, format, details, db_ref)?;
            }
        }
        Commands::Status => {
            let stats = db.stats()?;
            println!("Database: {}", db_path.display());
            println!("  Total files: {}", stats.total_files);
            println!("  Fingerprinted: {}", stats.fingerprinted);
            println!("  Failed: {}", stats.failed);
            println!("  Stale (file missing): {}", stats.stale);
        }
        Commands::CleanCache { patterns } => {
            if patterns.is_empty() {
                let removed = db.clean_stale()?;
                println!("Removed {} stale entries", removed);
            } else {
                let removed = db.clean_matching(&patterns)?;
                println!("Removed {} entries matching patterns", removed);
            }
        }
        Commands::Identify {
            api_key,
            all,
            threshold,
        } => {
            dupsonic::identify::run(&db, api_key.as_deref(), !all, threshold)?;
        }
        Commands::Fpcalc {
            file,
            json: _,
            length,
        } => {
            dupsonic::fpcalc::run(&db, &file, length)?;
        }
        Commands::Exclude { paths } => {
            for path in &paths {
                let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
                if db.exclude_file(&canonical)? {
                    println!("Excluded: {}", canonical.display());
                } else {
                    println!("Not found in database: {}", path.display());
                }
            }
        }
        Commands::Include { paths, all } => {
            if all {
                let excluded = db.list_excluded()?;
                for path in &excluded {
                    db.include_file(path)?;
                }
                println!("Re-included {} files", excluded.len());
            } else {
                for path in &paths {
                    let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
                    if db.include_file(&canonical)? {
                        println!("Re-included: {}", canonical.display());
                    } else {
                        println!("Not found in database: {}", path.display());
                    }
                }
            }
        }
        Commands::Completions { shell } => {
            let mut cmd = Cli::command();
            clap_complete::generate(shell, &mut cmd, "dupsonic", &mut std::io::stdout());
            return Ok(());
        }
        Commands::Serve { bind } => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(dupsonic::web::serve(db, db_path, &bind))?;
        }
    }

    Ok(())
}
