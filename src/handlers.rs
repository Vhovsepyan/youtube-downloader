use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use tower::ServiceExt;
use tower_http::services::ServeFile;
use uuid::Uuid;

use crate::error::AppError;
use crate::job::{Job, JobManager};
use crate::ytdlp::Format;

pub async fn require_token(
    State(manager): State<Arc<JobManager>>,
    req: Request,
    next: Next,
) -> Result<Response, AppError> {
    let expected = &manager.config().auth_token;

    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    // The frontend can't attach an Authorization header to a plain
    // <video src>/<a download> request, so it authenticates those via a
    // cookie instead; API clients keep using the Authorization header.
    let provided = bearer.or_else(|| cookie_value(req.headers(), "auth_token"));

    match provided {
        Some(token) if constant_time_eq(token, expected) => Ok(next.run(req).await),
        _ => Err(AppError::Unauthorized),
    }
}

fn cookie_value<'a>(headers: &'a axum::http::HeaderMap, name: &str) -> Option<&'a str> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|pair| {
        let (k, v) = pair.trim().split_once('=')?;
        (k == name).then_some(v)
    })
}

/// Compares two strings without short-circuiting on the first differing
/// byte, so response timing doesn't leak how many leading bytes of a
/// guessed token matched — this guards every endpoint, so it's the one
/// comparison in the app worth doing carefully.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Deserialize)]
pub struct CreateJobRequest {
    pub url: String,
    #[serde(default)]
    pub quality: Format,
}

#[derive(Serialize)]
pub struct CreateJobResponse {
    pub job_id: Uuid,
}

pub async fn create_job(
    State(manager): State<Arc<JobManager>>,
    Json(body): Json<CreateJobRequest>,
) -> Result<Json<CreateJobResponse>, AppError> {
    if body.url.trim().is_empty() {
        return Err(AppError::BadRequest("url must not be empty".to_string()));
    }

    let job_id = manager
        .submit(body.url, body.quality)
        .await
        .map_err(AppError::BadRequest)?;

    Ok(Json(CreateJobResponse { job_id }))
}

pub async fn get_job(
    State(manager): State<Arc<JobManager>>,
    Path(id): Path<Uuid>,
) -> Result<Json<Job>, AppError> {
    manager
        .get_job(id)
        .await
        .map(Json)
        .ok_or_else(|| AppError::NotFound("no such job".to_string()))
}

pub async fn get_video(
    State(manager): State<Arc<JobManager>>,
    Path(id): Path<Uuid>,
    req: Request,
) -> Result<Response, AppError> {
    let (path, _pin) = manager
        .ready_path(id)
        .await
        .ok_or_else(|| AppError::NotFound("job not found or not ready yet".to_string()))?;

    // `_pin` stays alive until this function returns, which covers the
    // window in which ServeFile actually opens the file below — after
    // that, an unrelated eviction can't pull the file out from under an
    // already-open read.
    let result = ServeFile::new(path).oneshot(req).await;

    match result {
        Ok(response) => {
            if response.status() == StatusCode::NOT_FOUND {
                Err(AppError::NotFound(
                    "cached file was evicted before it could be served".to_string(),
                ))
            } else {
                Ok(response.into_response())
            }
        }
        Err(err) => Err(AppError::Internal(err.to_string())),
    }
}
