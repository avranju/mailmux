pub mod auth;
pub mod documents;
pub mod health;
pub mod ingest;
pub mod search;
pub mod view;

use crate::{
    config::Config,
    index::{SearchIndex, worker::IndexWorker},
    search::SearchService,
    storage::Repository,
};
use anyhow::Result;
use axum::{
    Router, middleware,
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tower_http::limit::RequestBodyLimitLayer;

#[derive(Clone)]
pub struct AppState {
    pub repo: Arc<Repository>,
    pub search: Arc<SearchService>,
    pub config: Arc<Config>,
    pub notify: Arc<Notify>,
    pub ready: Arc<AtomicBool>,
    pub token: Option<String>,
    pub cancel: CancellationToken,
}

pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route(
            "/v1/documents/{source}/{source_id}",
            put(ingest::upload)
                .get(documents::get)
                .layer(middleware::from_fn(ingest::require_multipart)),
        )
        .route(
            "/v1/documents/{source}/{source_id}/reindex",
            post(search::reindex),
        )
        .route("/v1/search", post(search::search))
        .layer(middleware::from_fn_with_state(state.clone(), auth::require));
    let view = Router::new().route("/view/{source}/{source_id}", get(view::view));
    let view = if state.config.server.protect_view {
        view.layer(middleware::from_fn_with_state(state.clone(), auth::require))
    } else {
        view
    };
    let mcp = Router::new()
        .nest_service(
            "/mcp",
            crate::mcp::server::service(
                state.search.clone(),
                state.cancel.clone(),
                state.config.mcp_allowed_hosts(),
            ),
        )
        .layer(middleware::from_fn_with_state(state.clone(), auth::require));
    Router::new()
        .route("/health", get(health::health))
        .route("/ready", get(health::ready))
        .merge(protected)
        .merge(view)
        .merge(mcp)
        .layer(RequestBodyLimitLayer::new(
            state.config.server.max_request_bytes,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            reject_oversized_content_length,
        ))
        // tower-http emits a text/plain response for a limit violation. Keep
        // the transport-level streaming limit, but normalize that response to
        // the same JSON envelope as handler-level payload errors.
        .layer(middleware::from_fn(normalize_payload_limit_response))
        .with_state(state)
}

async fn reject_oversized_content_length(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    let oversized = request
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > state.config.server.max_request_bytes);
    if oversized {
        crate::error::AppError::TooLarge.into_response()
    } else {
        next.run(request).await
    }
}

async fn normalize_payload_limit_response(
    request: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    let response = next.run(request).await;
    if response.status() == axum::http::StatusCode::PAYLOAD_TOO_LARGE {
        crate::error::AppError::TooLarge.into_response()
    } else {
        response
    }
}

pub async fn serve(config: Config, repo: Repository) -> Result<()> {
    config.validate_server_auth()?;
    let config = Arc::new(config);
    let repo = Arc::new(repo);
    let (idx, writer) = SearchIndex::open(&config.index.path, config.index.writer_memory_bytes)?;
    let idx = Arc::new(idx);
    let notify = Arc::new(Notify::new());
    let cancel = CancellationToken::new();
    let worker = IndexWorker::new(
        repo.clone(),
        idx.clone(),
        writer,
        notify.clone(),
        config.index.batch_size,
        config.index.commit_interval_ms,
        cancel.clone(),
    );
    let worker_handle = tokio::spawn(worker.run());
    let search = Arc::new(SearchService {
        repo: repo.clone(),
        index: idx,
        config: config.clone(),
    });
    let ready = Arc::new(AtomicBool::new(false));
    let state = AppState {
        repo,
        search,
        config: config.clone(),
        notify,
        ready: ready.clone(),
        token: config.token(),
        cancel: cancel.clone(),
    };
    let listener = tokio::net::TcpListener::bind(config.server.bind).await?;
    let app = router(state);
    ready.store(true, Ordering::Release);
    tracing::info!(address=%config.server.bind,"mailindex ready");
    let shutdown_cancel = cancel.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{SignalKind, signal};
                let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
                tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
            }
            shutdown_cancel.cancel();
        })
        .await?;
    cancel.cancel();
    let _ = worker_handle.await;
    ready.store(false, Ordering::Release);
    Ok(())
}
