use std::env;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone)]
pub struct Config {
    pub bind_addr: String,
    pub cache_dir: PathBuf,
    pub frontend_dir: PathBuf,
    pub max_cache_bytes: u64,
    pub max_concurrent_downloads: usize,
    pub auth_token: String,
    pub download_timeout: Duration,
    pub cookies_file: Option<PathBuf>,
}

impl Config {
    pub fn from_env() -> Self {
        // BIND_ADDR wins if set. Otherwise honor $PORT (Cloud Run and other
        // managed platforms inject it and require the container to listen on
        // it), falling back to the local-dev default.
        let bind_addr = env::var("BIND_ADDR")
            .ok()
            .or_else(|| env::var("PORT").ok().map(|p| format!("0.0.0.0:{p}")))
            .unwrap_or_else(|| "0.0.0.0:8080".to_string());
        let cache_dir = env::var("CACHE_DIR").unwrap_or_else(|_| "/data/cache".to_string());
        // Where the static frontend lives. Served directly by the app when
        // there's no reverse proxy in front (e.g. Cloud Run); when Caddy
        // fronts the app (docker-compose) it serves web/ itself and this
        // fallback simply goes unused. Defaults to "web" for `cargo run`
        // from the repo root.
        let frontend_dir = env::var("FRONTEND_DIR").unwrap_or_else(|_| "web".to_string());
        let max_cache_gb: u64 = env::var("MAX_CACHE_GB")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10);
        let max_concurrent_downloads: usize = env::var("MAX_CONCURRENT_DOWNLOADS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v| v > 0)
            .unwrap_or(2);
        // Trim surrounding whitespace: a secret store can hand us the value
        // with a trailing newline (e.g. `openssl rand -hex 32` piped into it),
        // which would otherwise never match the newline-free token a user
        // pastes into the UI.
        let auth_token = env::var("AUTH_TOKEN")
            .expect("AUTH_TOKEN environment variable must be set")
            .trim()
            .to_string();
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
        // Optional cookies.txt for yt-dlp, to get past YouTube's "confirm
        // you're not a bot" block on datacenter IPs. The source may be a
        // read-only secret mount (Cloud Run), but yt-dlp rewrites the cookie
        // jar when it exits, so copy it to a writable path and use that.
        let cookies_file = env::var("COOKIES_FILE")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .and_then(|src| {
                let dst = env::temp_dir().join("yt-dlp-cookies.txt");
                match std::fs::copy(&src, &dst) {
                    Ok(_) => Some(dst),
                    Err(e) => {
                        eprintln!(
                            "warning: COOKIES_FILE={src} could not be read ({e}); \
                             continuing without cookies"
                        );
                        None
                    }
                }
            });

        Config {
            bind_addr,
            cache_dir: PathBuf::from(cache_dir),
            frontend_dir: PathBuf::from(frontend_dir),
            max_cache_bytes: max_cache_gb * 1024 * 1024 * 1024,
            max_concurrent_downloads,
            auth_token,
            download_timeout: Duration::from_secs(download_timeout_secs),
            cookies_file,
        }
    }
}
