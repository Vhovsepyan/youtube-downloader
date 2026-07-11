use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::io::AsyncReadExt;
use tokio::sync::{Mutex, RwLock, Semaphore};
use tracing::{error, info};
use uuid::Uuid;

use crate::cache::{CacheIndex, ReadPin};
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

/// Where to read a job's output file from and how to label it, known as
/// soon as the extension is resolved — i.e. before the download itself
/// (which may take a while) has produced any bytes.
#[derive(Clone)]
pub struct StreamingTarget {
    pub path: PathBuf,
    pub content_type: &'static str,
}

#[derive(Clone, serde::Serialize)]
pub struct Job {
    pub id: Uuid,
    #[serde(skip)]
    pub cache_key: String,
    pub status: JobStatus,
    #[serde(skip)]
    pub updated_at: Instant,
    #[serde(skip)]
    pub streaming_target: Option<StreamingTarget>,
}

/// What `/api/videos/:id` should do for a job right now.
pub enum ServeState {
    /// Fully downloaded and cached; serve normally (Range requests, etc.)
    /// via the given path, pinned against eviction until `ReadPin` drops.
    Ready(PathBuf, ReadPin),
    /// Still being written by yt-dlp; the caller should tail the file and
    /// stream whatever's been written so far.
    Downloading(StreamingTarget),
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
                        streaming_target: None,
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
                streaming_target: None,
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

    /// Returns what `/api/videos/:id` should do for this job right now: a
    /// pinned path to serve normally if it's `Ready`, or a target to stream
    /// progressively if it's still `Downloading`. `None` for any other
    /// status (queued/failed/unknown).
    pub async fn serve_state(&self, id: Uuid) -> Option<ServeState> {
        let job = self.jobs.read().await.get(&id).cloned()?;
        match job.status {
            JobStatus::Ready => {
                let (path, pin) = self.cache.lock().await.get_for_read(&job.cache_key)?;
                Some(ServeState::Ready(path, pin))
            }
            JobStatus::Downloading => job.streaming_target.map(ServeState::Downloading),
            _ => None,
        }
    }

    /// Streams a job's output file as it's written: reads whatever's
    /// available, and on catching up to the current end of file, waits and
    /// retries as long as the job is still queued/downloading. Stops once
    /// the job is `Ready` (by then the file is guaranteed complete — this
    /// only ever observes `Ready` after `execute_download` has fully
    /// finished writing it and calling `insert`) or anything else
    /// (failed/gone), at which point one last zero-byte read reliably means
    /// "that's the whole file."
    pub fn stream_downloading(
        self: Arc<Self>,
        id: Uuid,
        path: PathBuf,
    ) -> impl futures_util::Stream<Item = std::io::Result<Bytes>> {
        async_stream::try_stream! {
            let mut file = self.wait_for_file(id, &path).await?;

            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = file.read(&mut buf).await?;
                if n > 0 {
                    yield Bytes::copy_from_slice(&buf[..n]);
                    continue;
                }
                if self.still_producing(id).await {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                } else {
                    break;
                }
            }
        }
    }

    /// Waits for a job's output file to be created — for merged formats it
    /// isn't created until the source streams finish downloading and
    /// ffmpeg starts merging, which can be well after the job entered
    /// `Downloading` — polling job status meanwhile so a job that fails
    /// before ever producing a file doesn't wait forever.
    async fn wait_for_file(&self, id: Uuid, path: &std::path::Path) -> std::io::Result<tokio::fs::File> {
        loop {
            match tokio::fs::File::open(path).await {
                Ok(f) => return Ok(f),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && self.still_producing(id).await => {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn still_producing(&self, id: Uuid) -> bool {
        matches!(
            self.jobs.read().await.get(&id).map(|j| &j.status),
            Some(JobStatus::Queued) | Some(JobStatus::Downloading)
        )
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

        let ext = match ytdlp::resolve_extension(&url, format, self.config.cookies_file.as_deref())
            .await
        {
            Ok(ext) => ext,
            Err(e) => {
                error!(job_id = %id, cache_key, error = %e, "failed to resolve extension");
                self.set_status(id, JobStatus::Failed { error: e.to_string() }).await;
                return;
            }
        };

        let dest = {
            let cache = self.cache.lock().await;
            cache.path_for(&cache_key, &ext)
        };
        let target = StreamingTarget {
            path: dest.clone(),
            content_type: ytdlp::content_type_for(format, &ext),
        };
        self.set_downloading(id, target).await;

        let result = self.execute_download(&url, &cache_key, format, &dest).await;

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

    async fn execute_download(
        &self,
        url: &str,
        cache_key: &str,
        format: Format,
        dest: &std::path::Path,
    ) -> Result<(), String> {
        // yt-dlp writes straight to `dest` (see download_to_file's
        // --no-part), so any file already there from a prior attempt (e.g.
        // this process was killed mid-download) must be cleared first —
        // otherwise yt-dlp treats it as resumable and can 416 against it.
        let _ = tokio::fs::remove_file(dest).await;

        ytdlp::download_to_file(
            url,
            format,
            dest,
            self.config.download_timeout,
            self.config.cookies_file.as_deref(),
        )
        .await
        .map_err(|e| e.to_string())?;

        let size = tokio::fs::metadata(dest)
            .await
            .map_err(|e| e.to_string())?
            .len();

        self.cache
            .lock()
            .await
            .insert(cache_key.to_string(), dest.to_path_buf(), size)?;

        Ok(())
    }

    async fn set_downloading(&self, id: Uuid, target: StreamingTarget) {
        if let Some(job) = self.jobs.write().await.get_mut(&id) {
            job.status = JobStatus::Downloading;
            job.streaming_target = Some(target);
            job.updated_at = Instant::now();
        }
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
