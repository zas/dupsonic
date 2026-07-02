use anyhow::Result;
use clap::{Parser, Subcommand};
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
    },

    /// Show scan status and database statistics
    Status,

    /// Remove entries for files that no longer exist
    CleanCache,
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
        Commands::Scan { paths, jobs, force } => {
            scanner::scan(&db, &paths, jobs, force)?;
        }
        Commands::FindDupes {
            threshold,
            same_tree,
            format,
            r#for,
        } => {
            let groups = if let Some(ref target) = r#for {
                matcher::find_duplicates_for(&db, target, threshold)?
            } else {
                matcher::find_duplicates(&db, threshold, same_tree)?
            };
            output::print_results(&groups, format)?;
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
    }

    Ok(())
}
