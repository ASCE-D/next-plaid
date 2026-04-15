use std::io::Read as IoRead;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use tokio::sync::{Mutex, RwLock};
use tower::ServiceBuilder;
use tower_http::cors::CorsLayer;
use axum::error_handling::HandleErrorLayer;

use colgrep::{
    ensure_model, get_index_dir_for_project, get_vector_index_path, SearchResult, Searcher,
    UnitType,
};

pub struct ServeConfig {
    pub port: u16,
    pub host: String,
    pub index: PathBuf,
    pub model: Option<String>,
    pub sessions: usize,
    pub timeout: u64,
    pub max_concurrent: usize,
    pub alpha: f32,
    pub no_hybrid_search: bool,
    pub no_prewarm: bool,
    pub quantized: bool,
    pub force_cpu: bool,
}

struct AppState {
    searcher: RwLock<Option<Arc<Searcher>>>,
    error_reason: RwLock<Option<String>>,
    model_name: String,
    project_root: PathBuf,
    quantized: bool,
    alpha: f32,
    no_hybrid_search: bool,
    start_time: Instant,
    last_reload: RwLock<DateTime<Utc>>,
    reload_mutex: Mutex<()>,
}

fn format_result(r: &SearchResult) -> serde_json::Value {
    serde_json::json!({
        "file": r.unit.file.display().to_string(),
        "line": r.unit.line,
        "end_line": r.unit.end_line,
        "score": (r.score * 1000.0).round() / 1000.0,
        "unit_type": serde_json::to_value(&r.unit.unit_type).unwrap_or_default(),
        "signature": r.unit.signature,
        "code": r.unit.code,
        "language": serde_json::to_value(&r.unit.language).unwrap_or_default(),
    })
}

async fn health_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let searcher = state.searcher.read().await;
    let last_reload = state.last_reload.read().await;
    let uptime = state.start_time.elapsed().as_secs();

    if let Some(ref s) = *searcher {
        let doc_count = s.num_documents();
        (
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "ready",
                "index_doc_count": doc_count,
                "model": state.model_name,
                "uptime_seconds": uptime,
                "last_reload": last_reload.to_rfc3339(),
            })),
        )
    } else {
        let reason = state
            .error_reason
            .read()
            .await
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": "error",
                "reason": reason,
            })),
        )
    }
}

#[derive(Deserialize)]
struct SearchRequest {
    query: Option<String>,
    top_k: Option<usize>,
    alpha: Option<f32>,
    target_paths: Option<Vec<String>>,
    include_patterns: Option<Vec<String>>,
    exclude_patterns: Option<Vec<String>>,
    exclude_dirs: Option<Vec<String>>,
    text_pattern: Option<String>,
    code_only: Option<bool>,
}

async fn search_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SearchRequest>,
) -> impl IntoResponse {
    // Validate query
    let query = match req.query {
        Some(ref q) if !q.trim().is_empty() => q.trim().to_string(),
        Some(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "query must not be empty"})),
            );
        }
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "missing required field: query"})),
            );
        }
    };

    let top_k = req.top_k.unwrap_or(10).min(1000);
    let alpha = req.alpha.unwrap_or(state.alpha);

    if !(0.0..=1.0).contains(&alpha) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "alpha must be between 0.0 and 1.0"})),
        );
    }

    let code_only = req.code_only.unwrap_or(false);

    // Check searcher is available
    let searcher = {
        let guard = state.searcher.read().await;
        match &*guard {
            Some(s) => Arc::clone(s),
            None => {
                let reason = state
                    .error_reason
                    .read()
                    .await
                    .clone()
                    .unwrap_or_else(|| "searcher not loaded".to_string());
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({"error": reason})),
                );
            }
        }
    };

    let target_paths = req.target_paths.clone();
    let include_patterns = req.include_patterns.clone();
    let exclude_patterns = req.exclude_patterns.clone();
    let exclude_dirs = req.exclude_dirs.clone();
    let text_pattern = req.text_pattern.clone();
    let no_hybrid = state.no_hybrid_search;

    let search_start = Instant::now();

    // Build subset and run search in spawn_blocking (SQLite + ONNX are blocking)
    let result = tokio::task::spawn_blocking(move || -> Result<Vec<SearchResult>> {
        // Build subset from filters
        let mut subset_ids: Option<Vec<i64>> = None;

        let mut apply_filter = |ids: Vec<i64>| {
            subset_ids = Some(match subset_ids.take() {
                Some(existing) => {
                    let set: std::collections::HashSet<i64> =
                        ids.into_iter().collect();
                    existing.into_iter().filter(|id| set.contains(id)).collect()
                }
                None => ids,
            });
        };

        if let Some(ref paths) = target_paths {
            if !paths.is_empty() {
                for p in paths {
                    let ids = searcher.filter_by_path_prefix(Path::new(p))?;
                    apply_filter(ids);
                }
            }
        }

        if let Some(ref patterns) = include_patterns {
            if !patterns.is_empty() {
                let ids = searcher.filter_by_file_patterns(patterns)?;
                apply_filter(ids);
            }
        }

        if let Some(ref patterns) = exclude_patterns {
            if !patterns.is_empty() {
                let exclude_ids = searcher.filter_exclude_by_patterns(patterns)?;
                apply_filter(exclude_ids);
            }
        }

        if let Some(ref dirs) = exclude_dirs {
            if !dirs.is_empty() {
                let exclude_ids = searcher.filter_exclude_by_dirs(dirs)?;
                apply_filter(exclude_ids);
            }
        }

        if let Some(ref pattern) = text_pattern {
            if !pattern.is_empty() {
                let ids = searcher.filter_by_text_pattern_with_options(
                    pattern, false, false, false,
                )?;
                apply_filter(ids);
            }
        }

        let subset_slice = subset_ids.as_deref();

        // Run the search
        let use_hybrid = alpha < 1.0 && !no_hybrid;
        if use_hybrid {
            searcher.search_hybrid(&query, top_k, subset_slice, alpha)
        } else {
            searcher.search(&query, top_k, subset_slice)
        }
    })
    .await;

    let search_time_ms = search_start.elapsed().as_millis() as u64;

    match result {
        Ok(Ok(mut results)) => {
            if code_only {
                results.retain(|r| {
                    !matches!(r.unit.unit_type, UnitType::Document | UnitType::RawCode)
                });
            }

            let doc_count = {
                let guard = state.searcher.read().await;
                guard.as_ref().map(|s| s.num_documents()).unwrap_or(0)
            };

            let hybrid_mode = alpha < 1.0 && !state.no_hybrid_search;

            let formatted: Vec<serde_json::Value> =
                results.iter().map(format_result).collect();

            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "results": formatted,
                    "search_time_ms": search_time_ms,
                    "index_doc_count": doc_count,
                    "hybrid_mode": hybrid_mode,
                })),
            )
        }
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("search failed: {e}")})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("task failed: {e}")})),
        ),
    }
}

async fn reload_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let _guard = state.reload_mutex.lock().await;

    let project_root = state.project_root.clone();
    let model_name = state.model_name.clone();
    let quantized = state.quantized;

    let reload_start = Instant::now();

    let result = tokio::task::spawn_blocking(move || -> Result<Arc<Searcher>> {
        let model_path = ensure_model(Some(&model_name), true)?;
        let searcher = Searcher::load_with_quantized(&project_root, &model_path, quantized)?;
        Ok(Arc::new(searcher))
    })
    .await;

    let reload_time_ms = reload_start.elapsed().as_millis() as u64;

    match result {
        Ok(Ok(new_searcher)) => {
            let doc_count = new_searcher.num_documents();
            *state.searcher.write().await = Some(new_searcher);
            *state.error_reason.write().await = None;
            *state.last_reload.write().await = Utc::now();
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "status": "reloaded",
                    "doc_count": doc_count,
                    "reload_time_ms": reload_time_ms,
                })),
            )
        }
        Ok(Err(e)) => {
            let reason = format!("reload failed: {e}");
            *state.error_reason.write().await = Some(reason.clone());
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": reason})),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("reload task failed: {e}")})),
        ),
    }
}

fn prewarm(project_root: &Path) {
    let index_dir = match get_index_dir_for_project(project_root) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[warn] prewarm: could not resolve index dir: {e}");
            return;
        }
    };
    let vector_dir = get_vector_index_path(&index_dir);

    for filename in &["merged_codes.npy", "merged_residuals.npy"] {
        let path = vector_dir.join(filename);
        if !path.exists() {
            continue;
        }
        match std::fs::File::open(&path) {
            Ok(mut f) => {
                let mut buf = [0u8; 65536];
                loop {
                    match f.read(&mut buf) {
                        Ok(0) => break,
                        Ok(_) => continue,
                        Err(e) => {
                            eprintln!("[warn] prewarm: error reading {filename}: {e}");
                            break;
                        }
                    }
                }
                eprintln!("[info] prewarm: read {filename}");
            }
            Err(e) => {
                eprintln!("[warn] prewarm: could not open {filename}: {e}");
            }
        }
    }
}

pub fn cmd_serve(config: ServeConfig) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(config.sessions)
        .build()?;

    rt.block_on(async move {
        if config.force_cpu {
            std::env::set_var("COLGREP_FORCE_CPU", "1");
        }

        let model_name = config
            .model
            .clone()
            .unwrap_or_else(|| colgrep::DEFAULT_MODEL.to_string());

        let project_root = config.index.clone();
        let quantized = config.quantized;

        // Load searcher
        eprintln!(
            "[info] loading index from {} ...",
            project_root.display()
        );

        let (searcher, error_reason) = match tokio::task::spawn_blocking({
            let model_name = model_name.clone();
            let project_root = project_root.clone();
            move || -> Result<Searcher> {
                let model_path = ensure_model(Some(&model_name), true)?;
                Searcher::load_with_quantized(&project_root, &model_path, quantized)
            }
        })
        .await
        {
            Ok(Ok(s)) => {
                eprintln!(
                    "[info] index loaded: {} documents",
                    s.num_documents()
                );
                (Some(Arc::new(s)), None)
            }
            Ok(Err(e)) => {
                eprintln!("[error] failed to load index: {e}");
                (None, Some(format!("{e}")))
            }
            Err(e) => {
                eprintln!("[error] load task panicked: {e}");
                (None, Some(format!("load task panicked: {e}")))
            }
        };

        // Pre-warm mmap pages
        if !config.no_prewarm && searcher.is_some() {
            let pr = project_root.clone();
            let _ = tokio::task::spawn_blocking(move || prewarm(&pr)).await;
        }

        let now = Utc::now();
        let state = Arc::new(AppState {
            searcher: RwLock::new(searcher),
            error_reason: RwLock::new(error_reason),
            model_name,
            project_root,
            quantized,
            alpha: config.alpha,
            no_hybrid_search: config.no_hybrid_search,
            start_time: Instant::now(),
            last_reload: RwLock::new(now),
            reload_mutex: Mutex::new(()),
        });

        let app = Router::new()
            .route("/health", get(health_handler))
            .route("/search", post(search_handler))
            .route("/reload", post(reload_handler))
            .layer(
                ServiceBuilder::new()
                    .layer(CorsLayer::permissive())
                    .layer(HandleErrorLayer::new(|_: tower::BoxError| async {
                        StatusCode::REQUEST_TIMEOUT
                    }))
                    .layer(tower::timeout::TimeoutLayer::new(
                        std::time::Duration::from_secs(config.timeout),
                    ))
                    .layer(tower::limit::ConcurrencyLimitLayer::new(
                        config.max_concurrent,
                    )),
            )
            .with_state(state);

        let addr = format!("{}:{}", config.host, config.port);
        eprintln!("[info] listening on {addr}");

        let listener = tokio::net::TcpListener::bind(&addr).await?;

        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                tokio::signal::ctrl_c()
                    .await
                    .expect("failed to listen for Ctrl+C");
                eprintln!("\n[info] shutting down...");
            })
            .await?;

        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn make_unhealthy_state() -> Arc<AppState> {
        Arc::new(AppState {
            searcher: RwLock::new(None),
            error_reason: RwLock::new(Some("test: no index loaded".to_string())),
            model_name: "test-model".to_string(),
            project_root: PathBuf::from("/tmp/nonexistent"),
            quantized: false,
            alpha: 0.7,
            no_hybrid_search: false,
            start_time: Instant::now(),
            last_reload: RwLock::new(Utc::now()),
            reload_mutex: Mutex::new(()),
        })
    }

    fn make_router(state: Arc<AppState>) -> Router {
        Router::new()
            .route("/health", get(health_handler))
            .route("/search", post(search_handler))
            .route("/reload", post(reload_handler))
            .with_state(state)
    }

    #[tokio::test]
    async fn test_health_returns_503_before_init() {
        let state = make_unhealthy_state();
        let app = make_router(state);

        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "error");
        assert!(json["reason"].as_str().unwrap().contains("no index loaded"));
    }

    #[tokio::test]
    async fn test_search_returns_503_when_unhealthy() {
        let state = make_unhealthy_state();
        let app = make_router(state);

        let req = Request::builder()
            .method("POST")
            .uri("/search")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({"query": "test"}).to_string(),
            ))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_search_invalid_request_returns_400() {
        let state = make_unhealthy_state();
        // Put a dummy value to bypass 503 check — but since searcher is None,
        // validation happens first for missing query.
        let app = make_router(state);

        let req = Request::builder()
            .method("POST")
            .uri("/search")
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["error"].as_str().unwrap().contains("query"));
    }

    #[tokio::test]
    async fn test_search_empty_query_returns_400() {
        let state = make_unhealthy_state();
        let app = make_router(state);

        let req = Request::builder()
            .method("POST")
            .uri("/search")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({"query": ""}).to_string(),
            ))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["error"].as_str().unwrap().contains("empty"));
    }
}
