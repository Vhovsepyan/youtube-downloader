mod cache;
mod config;
mod error;
mod handlers;
mod job;
mod ytdlp;

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use cache::CacheIndex;
use config::Config;
use job::JobManager;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let config = Config::from_env();
    let cache = CacheIndex::load(&config.cache_dir, config.max_cache_bytes)
        .expect("failed to load cache index");

    let bind_addr = config.bind_addr.clone();
    let frontend_dir = config.frontend_dir.clone();
    let manager = JobManager::new(config, cache);

    {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(600));
            loop {
                interval.tick().await;
                manager
                    .reap_finished_jobs(std::time::Duration::from_secs(3600))
                    .await;
            }
        });
    }

    // Token-gated API. The middleware wraps only these routes, so it must be
    // applied here, before the public frontend fallback is added below.
    let api = Router::new()
        .route("/api/jobs", post(handlers::create_job))
        .route("/api/jobs/{id}", get(handlers::get_job))
        .route("/api/videos/{id}", get(handlers::get_video))
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&manager),
            handlers::require_token,
        ));

    // Static frontend served as an unauthenticated fallback (the user types
    // the token into the page, which then authenticates the /api calls). When
    // a reverse proxy like Caddy fronts the app it serves web/ itself and this
    // never gets hit; on Cloud Run (no proxy) the app serves it directly.
    let app = Router::new()
        .merge(api)
        .fallback_service(ServeDir::new(&frontend_dir))
        .layer(TraceLayer::new_for_http())
        .with_state(manager);

    tracing::info!(%bind_addr, "starting server");
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .expect("failed to bind address");
    axum::serve(listener, app)
        .await
        .expect("server error");
}
