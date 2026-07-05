mod cache;
mod config;
mod error;
mod handlers;
mod job;
mod ytdlp;

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
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

    let app = Router::new()
        .route("/api/jobs", post(handlers::create_job))
        .route("/api/jobs/{id}", get(handlers::get_job))
        .route("/api/videos/{id}", get(handlers::get_video))
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&manager),
            handlers::require_token,
        ))
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
