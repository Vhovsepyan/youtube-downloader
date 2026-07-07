use std::env;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone)]
pub struct Config {
    pub bind_addr: String,
    pub cache_dir: PathBuf,
    pub max_cache_bytes: u64,
    pub max_concurrent_downloads: usize,
    pub auth_token: String,
    pub download_timeout: Duration,
}

impl Config {
    pub fn from_env() -> Self {
        let bind_addr = env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
        let cache_dir = env::var("CACHE_DIR").unwrap_or_else(|_| "/data/cache".to_string());
        let max_cache_gb: u64 = env::var("MAX_CACHE_GB")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10);
        let max_concurrent_downloads: usize = env::var("MAX_CONCURRENT_DOWNLOADS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v| v > 0)
            .unwrap_or(2);
        let auth_token = env::var("AUTH_TOKEN")
            .expect("AUTH_TOKEN environment variable must be set");
        // YouTube throttles some video-only streams (seen live: ~720p+ on
        // some videos crawl at tens of KiB/s), so this needs to be generous
        // enough not to kill a legitimately-slow-but-working download —
        // it's a backstop against a genuinely stalled/hung yt-dlp process,
        // not a "this quality is taking a while" limit.
        let download_timeout_secs: u64 = env::var("DOWNLOAD_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v| v > 0)
            .unwrap_or(900);

        Config {
            bind_addr,
            cache_dir: PathBuf::from(cache_dir),
            max_cache_bytes: max_cache_gb * 1024 * 1024 * 1024,
            max_concurrent_downloads,
            auth_token,
            download_timeout: Duration::from_secs(download_timeout_secs),
        }
    }
}
