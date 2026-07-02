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
    #[arg(long)]
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
        /// Directories to scan for audio files
        #[arg(required = true)]
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

    /// Remove entries for files that no longer exist
    CleanCache,

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
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter)),
        )
        .init();

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
        } => {
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
            let groups = if let Some(ref target) = r#for {
                matcher::find_duplicates_for(&db, target, threshold)?
            } else {
                matcher::find_duplicates(&db, threshold, same_tree)?
            };

            // Filter out groups where MBIDs prove they're different recordings
            let groups = if no_mbid_filter {
                groups
            } else {
                matcher::filter_by_mbids(groups, &db)
            };

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
        Commands::CleanCache => {
            let removed = db.clean_stale()?;
            println!("Removed {} stale entries", removed);
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
    }

    Ok(())
}
