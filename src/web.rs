//! Minimal web UI for dupsonic.
//!
//! Provides a single-page web interface for scanning, viewing duplicates,
//! and acting on them. Designed for headless servers (NAS, Raspberry Pi).

use axum::extract::{ConnectInfo, Query, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};
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
pub async fn serve(
    db: Database,
    db_path: PathBuf,
    bind: &str,
    allowed_ips: &[IpNet],
) -> anyhow::Result<()> {
    let state = Arc::new(AppState {
        db,
        db_path,
        scan_status: Mutex::new(ScanStatus::default()),
        dupes: Mutex::new(Vec::new()),
    });

    let allowed_ips: Arc<Vec<IpNet>> = Arc::new(allowed_ips.to_vec());
    let allowed_ips_for_middleware = allowed_ips.clone();

    let app = Router::new()
        .route("/", get(index_html))
        .route("/api/status", get(api_status))
        .route("/api/scan", post(api_scan))
        .route("/api/dupes", get(api_dupes))
        .route("/api/action", post(api_action))
        .route("/api/restore", post(api_restore))
        .layer(middleware::from_fn(move |req, next| {
            let allowed = allowed_ips_for_middleware.clone();
            ip_access_control(req, next, allowed)
        }))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind).await?;
    eprintln!("Web UI available at http://{}", bind);
    if allowed_ips.is_empty() {
        eprintln!("Access: unrestricted (use --allow-ip to restrict)");
    } else {
        eprintln!(
            "Access restricted to: {}",
            allowed_ips
                .iter()
                .map(|ip| ip.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    eprintln!("Press Ctrl+C to stop.");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
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

/// Middleware that restricts access based on client IP address.
/// If the allow list is empty, all connections are permitted.
/// Handles IPv4-mapped IPv6 addresses (::ffff:x.x.x.x).
async fn ip_access_control(
    req: axum::extract::Request,
    next: Next,
    allowed_ips: Arc<Vec<IpNet>>,
) -> Response {
    // If no restrictions configured, allow all
    if allowed_ips.is_empty() {
        return next.run(req).await;
    }

    // Extract the client IP from ConnectInfo
    let client_ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip());

    let Some(client_ip) = client_ip else {
        return (StatusCode::FORBIDDEN, "Access denied").into_response();
    };

    // Normalize IPv4-mapped IPv6 addresses (::ffff:192.168.1.1 → 192.168.1.1)
    let normalized_ip = match client_ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => client_ip,
        },
        _ => client_ip,
    };

    // Check if the client IP is in any of the allowed ranges
    let allowed = allowed_ips.iter().any(|net| net.contains(&normalized_ip));

    if allowed {
        next.run(req).await
    } else {
        (StatusCode::FORBIDDEN, "Access denied").into_response()
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    use tower::ServiceExt;

    /// Build a minimal router with the IP middleware for testing.
    fn test_app(allowed_ips: Vec<IpNet>) -> Router {
        let allowed = Arc::new(allowed_ips);

        Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(middleware::from_fn(move |req, next| {
                let allowed = allowed.clone();
                ip_access_control(req, next, allowed)
            }))
    }

    /// Create a request with ConnectInfo set to the given IP.
    fn request_from_ip(ip: IpAddr) -> Request<Body> {
        let mut req = Request::builder().uri("/").body(Body::empty()).unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::new(ip, 12345)));
        req
    }

    #[tokio::test]
    async fn test_empty_allow_list_permits_all() {
        let app = test_app(vec![]);
        let req = request_from_ip(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_allowed_ip_permitted() {
        let allowed: IpNet = "192.168.1.0/24".parse().unwrap();
        let app = test_app(vec![allowed]);
        let req = request_from_ip(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_denied_ip_gets_403() {
        let allowed: IpNet = "192.168.1.0/24".parse().unwrap();
        let app = test_app(vec![allowed]);
        let req = request_from_ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_exact_ip_allowed() {
        // Single IP (parsed as /32)
        let allowed: IpNet = "10.0.0.5/32".parse().unwrap();
        let app = test_app(vec![allowed]);

        let req = request_from_ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_exact_ip_denied_other() {
        let allowed: IpNet = "10.0.0.5/32".parse().unwrap();
        let app = test_app(vec![allowed]);

        let req = request_from_ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 6)));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_ipv6_allowed() {
        let allowed: IpNet = "fd00::/8".parse().unwrap();
        let app = test_app(vec![allowed]);

        let req = request_from_ip(IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1)));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_ipv6_denied() {
        let allowed: IpNet = "fd00::/8".parse().unwrap();
        let app = test_app(vec![allowed]);

        let req = request_from_ip(IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_ipv4_mapped_ipv6_normalized() {
        // Client connects as ::ffff:192.168.1.50 (IPv4-mapped IPv6)
        // Allow list has 192.168.1.0/24 (IPv4)
        let allowed: IpNet = "192.168.1.0/24".parse().unwrap();
        let app = test_app(vec![allowed]);

        // ::ffff:192.168.1.50
        let v4_mapped = Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0xc0a8, 0x0132);
        let req = request_from_ip(IpAddr::V6(v4_mapped));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_multiple_ranges_any_match() {
        let allowed = vec![
            "10.0.0.0/8".parse::<IpNet>().unwrap(),
            "172.16.0.0/12".parse::<IpNet>().unwrap(),
            "192.168.0.0/16".parse::<IpNet>().unwrap(),
        ];
        let app = test_app(allowed);

        let req = request_from_ip(IpAddr::V4(Ipv4Addr::new(172, 20, 5, 1)));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_multiple_ranges_none_match() {
        let allowed = vec![
            "10.0.0.0/8".parse::<IpNet>().unwrap(),
            "192.168.0.0/16".parse::<IpNet>().unwrap(),
        ];
        let app = test_app(allowed);

        let req = request_from_ip(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }
}
