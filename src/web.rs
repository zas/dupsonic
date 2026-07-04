//! Minimal web UI for dupsonic.
//!
//! Provides a single-page web interface for scanning, viewing duplicates,
//! and acting on them. Designed for headless servers (NAS, Raspberry Pi).

use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Json};
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::database::Database;
use crate::matcher::{self, DuplicateGroup, MatchKind};

/// Shared application state.
struct AppState {
    db: Database,
    db_path: PathBuf,
    /// Current scan status.
    scan_status: Mutex<ScanStatus>,
    /// Cached duplicate groups from last find-dupes run.
    dupes: Mutex<Vec<DuplicateGroup>>,
}

#[derive(Debug, Clone, Serialize)]
struct ScanStatus {
    scanning: bool,
    message: String,
}

impl Default for ScanStatus {
    fn default() -> Self {
        Self {
            scanning: false,
            message: "Idle".to_string(),
        }
    }
}

/// Start the web server.
pub async fn serve(db: Database, db_path: PathBuf, bind: &str) -> anyhow::Result<()> {
    let state = Arc::new(AppState {
        db,
        db_path,
        scan_status: Mutex::new(ScanStatus::default()),
        dupes: Mutex::new(Vec::new()),
    });

    let app = Router::new()
        .route("/", get(index_html))
        .route("/api/status", get(api_status))
        .route("/api/scan", post(api_scan))
        .route("/api/dupes", get(api_dupes))
        .route("/api/action", post(api_action))
        .route("/api/restore", post(api_restore))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind).await?;
    eprintln!("Web UI available at http://{}", bind);
    eprintln!("Press Ctrl+C to stop.");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install signal handler");
    eprintln!("\nShutting down.");
}

// --- API Endpoints ---

async fn api_status(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let stats = state.db.stats().unwrap_or(crate::database::Stats {
        total_files: 0,
        fingerprinted: 0,
        failed: 0,
        stale: 0,
    });
    let scan_status = state.scan_status.lock().unwrap().clone();
    let dupes_count = state.dupes.lock().unwrap().len();
    let scan_paths: Vec<String> = state
        .db
        .load_scan_paths()
        .unwrap_or_default()
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();

    Json(serde_json::json!({
        "database": state.db_path.to_string_lossy(),
        "total_files": stats.total_files,
        "fingerprinted": stats.fingerprinted,
        "failed": stats.failed,
        "scanning": scan_status.scanning,
        "scan_message": scan_status.message,
        "duplicate_groups": dupes_count,
        "scan_paths": scan_paths,
    }))
}

#[derive(Deserialize)]
struct ScanRequest {
    paths: Vec<String>,
    #[serde(default = "default_jobs")]
    jobs: usize,
    #[serde(default = "default_length")]
    length: u64,
    #[serde(default)]
    force: bool,
}

fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

fn default_length() -> u64 {
    120
}

async fn api_scan(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ScanRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // Check if already scanning
    {
        let status = state.scan_status.lock().unwrap();
        if status.scanning {
            return Err((StatusCode::CONFLICT, "Scan already in progress".to_string()));
        }
    }

    // Use provided paths, or fall back to stored paths
    let paths: Vec<PathBuf> = if req.paths.is_empty() {
        state.db.load_scan_paths().unwrap_or_default()
    } else {
        req.paths.iter().map(PathBuf::from).collect()
    };

    if paths.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "No paths specified and no previously scanned paths found".to_string(),
        ));
    }

    // Validate paths
    for path in &paths {
        if !path.exists() {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("Path does not exist: {}", path.display()),
            ));
        }
    }

    // Start scan in background thread
    let state_clone = state.clone();
    let length = req.length;
    let jobs = req.jobs;
    let force = req.force;

    std::thread::spawn(move || {
        {
            let mut status = state_clone.scan_status.lock().unwrap();
            status.scanning = true;
            status.message = format!("Scanning {} path(s)...", paths.len());
        }

        let result = crate::scanner::scan(&state_clone.db, &paths, jobs, length, &[], force, true);

        {
            let mut status = state_clone.scan_status.lock().unwrap();
            status.scanning = false;
            match result {
                Ok(()) => status.message = "Scan complete".to_string(),
                Err(e) => status.message = format!("Scan failed: {}", e),
            }
        }
    });

    Ok(Json(serde_json::json!({ "status": "scan_started" })))
}

#[derive(Deserialize)]
struct DupesQuery {
    #[serde(default = "default_threshold")]
    threshold: f64,
}

fn default_threshold() -> f64 {
    0.8
}

async fn api_dupes(
    State(state): State<Arc<AppState>>,
    Query(query): Query<DupesQuery>,
) -> Json<serde_json::Value> {
    let mut groups =
        matcher::find_duplicates(&state.db, query.threshold, false).unwrap_or_default();

    // Filter by MBIDs
    groups = matcher::filter_by_mbids(groups, &state.db);

    // Classify 100% matches
    matcher::classify_matches(&mut groups, &state.db);

    // Build response
    let response: Vec<serde_json::Value> = groups
        .iter()
        .map(|g| {
            serde_json::json!({
                "id": g.id,
                "similarity": g.similarity,
                "files": g.files.iter().map(|f| {
                    let size = std::fs::metadata(&f.path).map(|m| m.len()).unwrap_or(0);
                    let ext = f.path.extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("?")
                        .to_uppercase();
                    let audio_info = crate::tags::read_audio_info(&f.path).ok().flatten();
                    let sample_rate = audio_info.as_ref().and_then(|a| a.sample_rate);
                    let bits_per_sample = audio_info.as_ref().and_then(|a| a.bits_per_sample);
                    let bitrate_kbps = audio_info.as_ref().and_then(|a| a.bitrate_kbps);

                    let mut file = serde_json::json!({
                        "path": f.path.to_string_lossy(),
                        "duration_secs": f.duration_secs,
                        "size": size,
                        "format": ext,
                    });
                    if let Some(sr) = sample_rate { file["sample_rate"] = sr.into(); }
                    if let Some(bps) = bits_per_sample { file["bits_per_sample"] = bps.into(); }
                    if let Some(br) = bitrate_kbps { file["bitrate_kbps"] = br.into(); }
                    match f.match_kind {
                        MatchKind::ExactCopy => { file["match_kind"] = "exact_copy".into(); }
                        MatchKind::SameAudio => { file["match_kind"] = "same_audio".into(); }
                        MatchKind::Similar => {}
                    }
                    file
                }).collect::<Vec<_>>(),
            })
        })
        .collect();

    // Cache for actions
    *state.dupes.lock().unwrap() = groups;

    Json(serde_json::json!(response))
}

#[derive(Deserialize)]
struct ActionRequest {
    group_id: String,
    action: String,
    /// Index of file to act on (0-based, within the group)
    file_index: usize,
}

async fn api_action(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ActionRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let dupes = state.dupes.lock().unwrap();
    let group = dupes.iter().find(|g| g.id == req.group_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("Group {} not found", req.group_id),
        )
    })?;

    if req.file_index >= group.files.len() {
        return Err((StatusCode::BAD_REQUEST, "Invalid file index".to_string()));
    }

    let file_path = &group.files[req.file_index].path;

    match req.action.as_str() {
        "delete" => {
            if !file_path.exists() {
                return Err((
                    StatusCode::NOT_FOUND,
                    format!("File not found: {}", file_path.display()),
                ));
            }
            // Use system trash (recoverable)
            trash::delete(file_path).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to trash: {}", e),
                )
            })?;
            Ok(Json(serde_json::json!({
                "status": "trashed",
                "path": file_path.to_string_lossy(),
            })))
        }
        "exclude" => {
            state.db.exclude_file(file_path).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to exclude: {}", e),
                )
            })?;
            Ok(Json(serde_json::json!({
                "status": "excluded",
                "path": file_path.to_string_lossy(),
            })))
        }
        _ => Err((
            StatusCode::BAD_REQUEST,
            format!("Unknown action: {}. Use: delete, exclude", req.action),
        )),
    }
}

#[derive(Deserialize)]
struct RestoreRequest {
    path: String,
}

async fn api_restore(
    Json(req): Json<RestoreRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    restore_from_trash(&req.path)
}

#[cfg(all(
    unix,
    not(target_os = "macos"),
    not(target_os = "ios"),
    not(target_os = "android")
))]
fn restore_from_trash(path: &str) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let original_path = std::path::PathBuf::from(path);

    let trash_items = trash::os_limited::list().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to list trash: {}", e),
        )
    })?;

    let matching: Vec<_> = trash_items
        .into_iter()
        .filter(|item| item.original_path() == original_path)
        .collect();

    if matching.is_empty() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("Not found in trash: {}", path),
        ));
    }

    trash::os_limited::restore_all(matching).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to restore: {}", e),
        )
    })?;

    Ok(Json(serde_json::json!({
        "status": "restored",
        "path": path,
    })))
}

#[cfg(not(all(
    unix,
    not(target_os = "macos"),
    not(target_os = "ios"),
    not(target_os = "android")
)))]
fn restore_from_trash(path: &str) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    Err((
        StatusCode::NOT_IMPLEMENTED,
        format!(
            "Restore is not supported on this platform. Restore '{}' manually from your system trash.",
            path
        ),
    ))
}

// --- Static HTML ---

async fn index_html() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        Html(include_str!("web_ui.html")),
    )
}
