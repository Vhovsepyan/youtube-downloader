use std::path::Path;

use tokio::process::Command;
use url::Url;

const YOUTUBE_HOSTS: &[&str] = &[
    "youtube.com",
    "www.youtube.com",
    "m.youtube.com",
    "music.youtube.com",
];

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
/// youtu.be/, /shorts/, /embed/), rejecting anything whose scheme/host
/// isn't actually YouTube. This is a hard security boundary, not just
/// convenience parsing: the extracted id is the only part of client input
/// that ever reaches the `yt-dlp` command line (see `canonical_url`), so a
/// non-YouTube host or a malformed id must never pass here.
pub fn extract_video_id(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    if parsed.scheme() != "https" && parsed.scheme() != "http" {
        return None;
    }

    let host = parsed.host_str()?;
    let candidate = if host == "youtu.be" {
        parsed.path_segments()?.next()?.to_string()
    } else if YOUTUBE_HOSTS.contains(&host) {
        if let Some((_, v)) = parsed.query_pairs().find(|(k, _)| k == "v") {
            v.into_owned()
        } else {
            let segments: Vec<&str> = parsed.path_segments()?.collect();
            segments
                .windows(2)
                .find(|w| w[0] == "shorts" || w[0] == "embed")
                .map(|w| w[1].to_string())?
        }
    } else {
        return None;
    };

    is_valid_video_id(&candidate).then_some(candidate)
}

/// YouTube video ids are alphanumeric plus `-`/`_`; this also happens to be
/// exactly the charset that's safe to use unescaped in a cache key/filename
/// and can never be interpreted as a `yt-dlp` CLI flag.
fn is_valid_video_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Builds the URL `yt-dlp` is actually invoked with, from an
/// already-validated video id. The raw client-supplied URL is deliberately
/// never passed to `yt-dlp` beyond this point, so there is no path for
/// attacker-controlled bytes to reach its argv.
pub fn canonical_url(video_id: &str) -> String {
    format!("https://www.youtube.com/watch?v={video_id}")
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
                    "--",
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
        .args(["-o", dest_str, "--no-warnings", "--no-progress", "--", url])
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_common_youtube_url_shapes() {
        assert_eq!(
            extract_video_id("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            Some("dQw4w9WgXcQ".to_string())
        );
        assert_eq!(
            extract_video_id("https://youtu.be/dQw4w9WgXcQ?si=abc"),
            Some("dQw4w9WgXcQ".to_string())
        );
        assert_eq!(
            extract_video_id("https://www.youtube.com/shorts/dQw4w9WgXcQ"),
            Some("dQw4w9WgXcQ".to_string())
        );
        assert_eq!(
            extract_video_id("https://www.youtube.com/embed/dQw4w9WgXcQ"),
            Some("dQw4w9WgXcQ".to_string())
        );
    }

    #[test]
    fn rejects_non_youtube_hosts() {
        assert_eq!(extract_video_id("https://evil.example/?v=dQw4w9WgXcQ"), None);
        assert_eq!(
            extract_video_id("https://youtube.com.evil.example/watch?v=dQw4w9WgXcQ"),
            None
        );
    }

    #[test]
    fn rejects_cli_flag_injection_attempts() {
        // Not a parseable absolute URL at all.
        assert_eq!(extract_video_id("--exec=touch /tmp/pwned;v=1"), None);
        // Even if it parsed, the host isn't YouTube.
        assert_eq!(
            extract_video_id("https://--exec=x/?v=dQw4w9WgXcQ"),
            None
        );
    }

    #[test]
    fn rejects_ids_with_unsafe_characters() {
        assert_eq!(
            extract_video_id("https://www.youtube.com/watch?v=../../etc/passwd"),
            None
        );
    }

    #[test]
    fn ids_starting_with_a_hyphen_are_still_wrapped_into_a_safe_url() {
        // '-' is part of YouTube's real id alphabet, so it's a valid id —
        // but canonical_url must ensure it never ends up as a bare argv
        // token starting with '-'.
        let id = extract_video_id("https://www.youtube.com/watch?v=-flag-like1").unwrap();
        let url = canonical_url(&id);
        assert!(!url.starts_with('-'));
        assert!(url.starts_with("https://www.youtube.com/watch?v="));
    }

    #[test]
    fn canonical_url_is_always_a_safe_https_youtube_link() {
        assert_eq!(
            canonical_url("dQw4w9WgXcQ"),
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        );
    }
}
