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

    let provided = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match provided {
        Some(token) if token == expected => Ok(next.run(req).await),
        _ => Err(AppError::Unauthorized),
    }
}

#[derive(Deserialize)]
pub struct CreateJobRequest {
    pub url: String,
    #[serde(default)]
    pub audio_only: bool,
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

    let format = if body.audio_only {
        Format::AudioOnly
    } else {
        Format::Default
    };

    let job_id = manager
        .submit(body.url, format)
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
    let path = manager
        .ready_path(id)
        .await
        .ok_or_else(|| AppError::NotFound("job not found or not ready yet".to_string()))?;

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
