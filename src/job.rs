use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, RwLock, Semaphore};
use tracing::{error, info};
use uuid::Uuid;

use crate::cache::CacheIndex;
use crate::config::Config;
use crate::ytdlp::{self, Format};

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Downloading,
    Ready,
    Failed { error: String },
}

#[derive(Clone, serde::Serialize)]
pub struct Job {
    pub id: Uuid,
    #[serde(skip)]
    pub cache_key: String,
    pub status: JobStatus,
    #[serde(skip)]
    pub updated_at: Instant,
}

pub struct JobManager {
    config: Config,
    cache: Mutex<CacheIndex>,
    jobs: RwLock<HashMap<Uuid, Job>>,
    /// Maps a cache key to the job currently producing it, so concurrent
    /// requests for the same not-yet-cached video/format share one yt-dlp
    /// run instead of racing duplicate downloads. Wrapped in its own `Arc`
    /// so `InFlightGuard` can hold a handle to it independent of the
    /// `JobManager` it came from.
    in_flight: Arc<Mutex<HashMap<String, Uuid>>>,
    semaphore: Arc<Semaphore>,
}

/// Removes a cache key's in-flight marker when dropped, so it's cleared on
/// every exit path out of `run_job` — including a panic — not just the
/// normal-completion path.
struct InFlightGuard {
    in_flight: Arc<Mutex<HashMap<String, Uuid>>>,
    cache_key: String,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let in_flight = Arc::clone(&self.in_flight);
        let cache_key = std::mem::take(&mut self.cache_key);
        tokio::spawn(async move {
            in_flight.lock().await.remove(&cache_key);
        });
    }
}

impl JobManager {
    pub fn new(config: Config, cache: CacheIndex) -> Arc<Self> {
        let max_concurrent = config.max_concurrent_downloads;
        Arc::new(JobManager {
            config,
            cache: Mutex::new(cache),
            jobs: RwLock::new(HashMap::new()),
            in_flight: Arc::new(Mutex::new(HashMap::new())),
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
        })
    }

    /// Submits a download request. Returns the id of a job the caller
    /// should poll — this may be a brand-new job, an existing in-flight
    /// job for the same video+format, or an immediately-`Ready` job if the
    /// result was already cached.
    pub async fn submit(self: &Arc<Self>, url: String, format: Format) -> Result<Uuid, String> {
        let video_id = ytdlp::extract_video_id(&url)
            .ok_or_else(|| "could not extract a YouTube video ID from that URL".to_string())?;
        let cache_key = format!("{video_id}_{}", format.cache_key_suffix());
        // From here on, only this canonical URL (built from the validated
        // id) is ever passed to yt-dlp — never the raw client-supplied
        // `url` — so client input can't smuggle CLI flags or point at a
        // non-YouTube host.
        let canonical_url = ytdlp::canonical_url(&video_id);

        // Hold the in_flight lock across the whole decision — cache-hit
        // check, in-flight check, and (if neither hits) registering the new
        // job as in-flight — so two concurrent submits for the same
        // cache_key can't both slip past the checks before either registers,
        // which would otherwise race two yt-dlp downloads onto one file.
        let mut in_flight = self.in_flight.lock().await;

        if let Some(existing_id) = in_flight.get(&cache_key) {
            return Ok(*existing_id);
        }

        {
            let mut cache = self.cache.lock().await;
            if cache.get(&cache_key).is_some() {
                let id = Uuid::new_v4();
                self.jobs.write().await.insert(
                    id,
                    Job {
                        id,
                        cache_key: cache_key.clone(),
                        status: JobStatus::Ready,
                        updated_at: Instant::now(),
                    },
                );
                return Ok(id);
            }
        }

        let id = Uuid::new_v4();
        self.jobs.write().await.insert(
            id,
            Job {
                id,
                cache_key: cache_key.clone(),
                status: JobStatus::Queued,
                updated_at: Instant::now(),
            },
        );
        in_flight.insert(cache_key.clone(), id);
        drop(in_flight);

        let this = Arc::clone(self);
        tokio::spawn(async move {
            this.run_job(id, canonical_url, cache_key, format).await;
        });

        Ok(id)
    }

    pub async fn get_job(&self, id: Uuid) -> Option<Job> {
        self.jobs.read().await.get(&id).cloned()
    }

    /// Returns the cache file path for a `Ready` job, if it still exists,
    /// pinned against eviction for as long as the returned `ReadPin` lives.
    pub async fn ready_path(&self, id: Uuid) -> Option<(std::path::PathBuf, crate::cache::ReadPin)> {
        let job = self.jobs.read().await.get(&id).cloned()?;
        if !matches!(job.status, JobStatus::Ready) {
            return None;
        }
        self.cache.lock().await.get_for_read(&job.cache_key)
    }

    /// Removes finished (`Ready`/`Failed`) jobs whose status hasn't changed
    /// in over `max_age`, so the job map doesn't grow without bound over
    /// the life of the process.
    pub async fn reap_finished_jobs(&self, max_age: Duration) {
        let now = Instant::now();
        self.jobs.write().await.retain(|_, job| match job.status {
            JobStatus::Ready | JobStatus::Failed { .. } => {
                now.duration_since(job.updated_at) < max_age
            }
            _ => true,
        });
    }

    async fn run_job(self: Arc<Self>, id: Uuid, url: String, cache_key: String, format: Format) {
        let _permit = self.semaphore.acquire().await.expect("semaphore not closed");
        let _in_flight_guard = InFlightGuard {
            in_flight: Arc::clone(&self.in_flight),
            cache_key: cache_key.clone(),
        };

        self.set_status(id, JobStatus::Downloading).await;

        let result = self.execute_download(&url, &cache_key, format).await;

        match result {
            Ok(()) => {
                info!(job_id = %id, cache_key, "download complete");
                self.set_status(id, JobStatus::Ready).await;
            }
            Err(e) => {
                error!(job_id = %id, cache_key, error = %e, "download failed");
                self.set_status(id, JobStatus::Failed { error: e }).await;
            }
        }
    }

    async fn execute_download(&self, url: &str, cache_key: &str, format: Format) -> Result<(), String> {
        let ext = ytdlp::resolve_extension(url, format)
            .await
            .map_err(|e| e.to_string())?;

        let dest = {
            let cache = self.cache.lock().await;
            cache.path_for(cache_key, &ext)
        };

        ytdlp::download_to_file(url, format, &dest)
            .await
            .map_err(|e| e.to_string())?;

        let size = tokio::fs::metadata(&dest)
            .await
            .map_err(|e| e.to_string())?
            .len();

        self.cache
            .lock()
            .await
            .insert(cache_key.to_string(), dest, size)?;

        Ok(())
    }

    async fn set_status(&self, id: Uuid, status: JobStatus) {
        if let Some(job) = self.jobs.write().await.get_mut(&id) {
            job.status = status;
            job.updated_at = Instant::now();
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }
}
