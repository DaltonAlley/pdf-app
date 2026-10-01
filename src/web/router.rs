use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::{
    body::Body,
    extract::{DefaultBodyLimit, MatchedPath, State},
    http::{header::CACHE_CONTROL, HeaderValue, Method, Request},
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
    Router,
};
use tower_http::{
    services::{ServeDir, ServeFile},
    trace::{DefaultOnResponse, TraceLayer},
};
use tracing::Level;

use super::megabytes_to_bytes;
use crate::{error::AppResult, AppError, AppState};
const MAX_LAYOUT_JSON_BYTES: usize = 64 * 1024;

/// Absolute, startup-validated directory containing the CSR frontend bundle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrontendDist(PathBuf);

impl FrontendDist {
    /// Validates an explicit frontend distribution directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the path is relative, is not a directory, or does
    /// not contain the regular `app.html` entrypoint produced by cargo-leptos.
    pub fn validate(path: impl Into<PathBuf>) -> AppResult<Self> {
        let path = path.into();
        if !path.is_absolute() {
            return Err(AppError::Internal(format!(
                "frontend distribution path must be absolute; got `{}`",
                path.display()
            )));
        }
        if !path.is_dir() {
            return Err(AppError::Internal(format!(
                "frontend distribution directory does not exist: `{}`",
                path.display()
            )));
        }
        let entrypoint = path.join("app.html");
        if !entrypoint.is_file() {
            return Err(AppError::Internal(format!(
                "frontend distribution entrypoint does not exist: `{}`",
                entrypoint.display()
            )));
        }
        Ok(Self(path))
    }

    /// Build-checkout default used for local development and in-process tests.
    pub fn development_default_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("frontend/dist")
    }

    fn development_default_unchecked() -> Self {
        Self(Self::development_default_path())
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn entrypoint(&self) -> PathBuf {
        self.0.join("app.html")
    }
}

/// Builds the application router.
///
/// `max_upload_mb` limits each multipart request when present. Passing `None`
/// disables Axum's default body limit; operation-specific limits still apply.
/// Background jobs expose status and download routes under `/jobs`; cancelling a
/// running job is cooperative, while cancelling a completed job releases its
/// retained download.
///
/// # Errors
///
/// Returns an error when the configured upload limit cannot be represented in
/// bytes.
pub fn app(state: Arc<AppState>, max_upload_mb: Option<usize>) -> AppResult<Router> {
    app_with_frontend_dist(
        state,
        max_upload_mb,
        FrontendDist::development_default_unchecked(),
    )
}

/// Builds the application router with an explicit validated frontend bundle.
///
/// # Errors
///
/// Returns an error when the configured upload limit cannot be represented in
/// bytes.
pub fn app_with_frontend_dist(
    state: Arc<AppState>,
    max_upload_mb: Option<usize>,
    frontend_dist: FrontendDist,
) -> AppResult<Router> {
    let frontend_entry = ServeFile::new(frontend_dist.entrypoint());
    let frontend_assets = ServeDir::new(frontend_dist.path());
    // Watched development builds may leave release sidecars behind. Only
    // production builds serve the compressed assets generated during release.
    #[cfg(not(debug_assertions))]
    let frontend_assets = frontend_assets.precompressed_gzip();
    let multipart_routes = Router::new()
        .route("/pdf/inspect", post(super::pdf_ops::inspect))
        .route("/convert", post(super::convert::convert))
        .route("/merge", post(super::pdf_ops::merge))
        .route("/split", post(super::pdf_ops::split))
        .route("/gang-up/analyze", post(super::gang_up::analyze))
        .route("/gang-up/export", post(super::gang_up::export_pdf))
        .route("/jobs", post(super::jobs::create))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            limit_multipart_uploads,
        ));
    let multipart_routes = match max_upload_mb {
        Some(max_upload_mb) => multipart_routes.layer(DefaultBodyLimit::max(megabytes_to_bytes(
            "MAX_UPLOAD_MB",
            max_upload_mb,
        )?)),
        None => multipart_routes.layer(DefaultBodyLimit::disable()),
    };
    let impose_intake_route = Router::new()
        .route("/gang-up/sources", post(super::gang_up::prepare_source))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            limit_multipart_uploads,
        ))
        .layer(DefaultBodyLimit::disable());
    let operation_routes = Router::new()
        .route(
            "/gang-up/layout",
            post(super::gang_up::layout).layer(DefaultBodyLimit::max(MAX_LAYOUT_JSON_BYTES)),
        )
        .route(
            "/gang-up/presets",
            get(super::gang_up_catalog::presets)
                .post(super::gang_up_catalog::create_preset)
                .layer(DefaultBodyLimit::max(MAX_LAYOUT_JSON_BYTES)),
        )
        .route(
            "/gang-up/presets/{id}",
            axum::routing::put(super::gang_up_catalog::update_preset)
                .delete(super::gang_up_catalog::delete_preset)
                .layer(DefaultBodyLimit::max(MAX_LAYOUT_JSON_BYTES)),
        )
        .route(
            "/gang-up/recent-jobs",
            get(super::gang_up_catalog::recent_jobs)
                .post(super::gang_up_catalog::create_recent_job)
                .layer(DefaultBodyLimit::max(MAX_LAYOUT_JSON_BYTES)),
        )
        .route(
            "/gang-up/recent-jobs/{id}",
            axum::routing::delete(super::gang_up_catalog::delete_recent_job),
        )
        .route(
            "/gang-up/export-history",
            get(super::gang_up_catalog::export_history)
                .post(super::gang_up_catalog::create_export_history_record)
                .layer(DefaultBodyLimit::max(MAX_LAYOUT_JSON_BYTES)),
        )
        .route(
            "/gang-up/export-history/{id}",
            get(super::gang_up_catalog::download_export_history_record)
                .delete(super::gang_up_catalog::delete_export_history_record),
        )
        .route(
            "/gang-up/sources/{id}",
            axum::routing::delete(super::gang_up::delete_source),
        )
        .route(
            "/gang-up/sources/{id}/lease",
            axum::routing::put(super::gang_up::renew_source),
        )
        .route(
            "/gang-up/sources/{id}/preview/{page}",
            get(super::gang_up::preview_source_page),
        )
        .route(
            "/gang-up/sources/{id}/previews",
            post(super::gang_up::preview_source_pages)
                .layer(DefaultBodyLimit::max(MAX_LAYOUT_JSON_BYTES)),
        )
        .route("/jobs/{id}/download", get(super::jobs::download));

    Ok(Router::new()
        .route("/health", get(health))
        .route_service("/", frontend_entry)
        .merge(multipart_routes)
        .merge(impose_intake_route)
        .merge(operation_routes)
        .route(
            "/jobs/{id}",
            get(super::jobs::status).delete(super::jobs::cancel),
        )
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|request: &axum::http::Request<Body>| {
                    let route = request
                        .extensions()
                        .get::<MatchedPath>()
                        .map(MatchedPath::as_str)
                        .unwrap_or("fallback");
                    tracing::info_span!("request", method = %request.method(), %route)
                })
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        .with_state(state)
        .fallback_service(frontend_assets)
        .layer(middleware::from_fn(require_frontend_revalidation)))
}

async fn require_frontend_revalidation(request: Request<Body>, next: Next) -> Response {
    let is_frontend_entry = matches!(*request.method(), Method::GET | Method::HEAD)
        && (request.uri().path() == "/" || request.uri().path().ends_with(".html"));
    let mut response = next.run(request).await;

    if is_frontend_entry {
        response
            .headers_mut()
            .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    }

    response
}

async fn limit_multipart_uploads(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
    next: Next,
) -> AppResult<Response> {
    let _upload_permit = state.admit_upload()?;
    Ok(next.run(request).await)
}

async fn health() -> &'static str {
    "ok"
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    #[test]
    fn app_rejects_upload_limits_that_overflow_bytes() {
        let Some(pdfium) = crate::test_pdfium() else {
            return;
        };
        let state = Arc::new(AppState::for_tests(pdfium.shared()).unwrap());

        assert!(app(state, usize::MAX.into()).is_err());
    }

    #[tokio::test]
    async fn frontend_entry_requires_cache_revalidation() {
        let router = Router::new()
            .route("/", get(|| async { "frontend" }))
            .route("/health", get(health))
            .layer(middleware::from_fn(require_frontend_revalidation));

        let frontend_response = router
            .clone()
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let health_response = router
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(
            frontend_response.headers().get(CACHE_CONTROL),
            Some(&HeaderValue::from_static("no-cache"))
        );
        assert!(health_response.headers().get(CACHE_CONTROL).is_none());
    }
}
