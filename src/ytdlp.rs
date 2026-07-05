use std::path::Path;

use tokio::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Default,
    AudioOnly,
}

impl Format {
    pub fn cache_key_suffix(&self) -> &'static str {
        match self {
            Format::Default => "default",
            Format::AudioOnly => "audio",
        }
    }
}

pub struct YtDlpError(pub String);

impl std::fmt::Display for YtDlpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<std::io::Error> for YtDlpError {
    fn from(e: std::io::Error) -> Self {
        YtDlpError(format!("failed to run yt-dlp: {e}"))
    }
}

/// Extracts a YouTube video ID from common URL shapes (watch?v=,
/// youtu.be/, /shorts/, /embed/).
pub fn extract_video_id(url: &str) -> Option<String> {
    let after_marker = |marker: &str| -> Option<String> {
        let idx = url.find(marker)?;
        let rest = &url[idx + marker.len()..];
        let end = rest.find(['?', '&', '#', '/']).unwrap_or(rest.len());
        let candidate = &rest[..end];
        if candidate.is_empty() {
            None
        } else {
            Some(candidate.to_string())
        }
    };

    after_marker("v=")
        .or_else(|| after_marker("youtu.be/"))
        .or_else(|| after_marker("/shorts/"))
        .or_else(|| after_marker("/embed/"))
}

/// For audio-only downloads we don't force a container conversion (avoids an
/// unnecessary re-encode), so the real extension varies by source. Resolve
/// it up front with a cheap metadata-only query before running the actual
/// download.
pub async fn resolve_extension(url: &str, format: Format) -> Result<String, YtDlpError> {
    match format {
        Format::Default => Ok("mp4".to_string()),
        Format::AudioOnly => {
            let output = Command::new("yt-dlp")
                .args([
                    "-f",
                    "bestaudio",
                    "--skip-download",
                    "--print",
                    "%(ext)s",
                    "--no-warnings",
                    url,
                ])
                .output()
                .await?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                return Err(YtDlpError(stderr));
            }

            let ext = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if ext.is_empty() {
                return Err(YtDlpError("yt-dlp returned no extension".to_string()));
            }
            Ok(ext)
        }
    }
}

/// Runs yt-dlp with `-o <dest>`, telling it to write directly to a real
/// file on disk rather than piping through us. This matters: yt-dlp's MP4
/// merger needs a seekable output to write the moov atom, so asking it to
/// merge to a pipe silently downgrades the container to MPEG-TS instead —
/// which doesn't reliably carry common video codecs (e.g. VP9) at all.
/// Writing straight to `dest` avoids that entirely and is simpler besides.
pub async fn download_to_file(url: &str, format: Format, dest: &Path) -> Result<(), YtDlpError> {
    let format_args: Vec<&str> = match format {
        Format::Default => vec![
            "-f",
            "bv*[height<=1080]+ba/b[height<=1080]",
            "--merge-output-format",
            "mp4",
        ],
        Format::AudioOnly => vec!["-f", "bestaudio"],
    };

    let dest_str = dest
        .to_str()
        .ok_or_else(|| YtDlpError("cache path is not valid UTF-8".to_string()))?;

    let output = Command::new("yt-dlp")
        .args(&format_args)
        .args(["-o", dest_str, "--no-warnings", "--no-progress", url])
        .output()
        .await?;

    if !output.status.success() {
        let _ = tokio::fs::remove_file(dest).await;
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(YtDlpError(if stderr.trim().is_empty() {
            format!("yt-dlp exited with status {}", output.status)
        } else {
            stderr
        }));
    }

    Ok(())
}
