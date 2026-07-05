use std::env;
use std::path::PathBuf;

#[derive(Clone)]
pub struct Config {
    pub bind_addr: String,
    pub cache_dir: PathBuf,
    pub max_cache_bytes: u64,
    pub max_concurrent_downloads: usize,
    pub auth_token: String,
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

        Config {
            bind_addr,
            cache_dir: PathBuf::from(cache_dir),
            max_cache_bytes: max_cache_gb * 1024 * 1024 * 1024,
            max_concurrent_downloads,
            auth_token,
        }
    }
}
