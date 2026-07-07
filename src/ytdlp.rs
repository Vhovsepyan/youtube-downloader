use std::path::Path;
use std::time::Duration;

use tokio::process::Command;
use url::Url;

/// Metadata-only probes (no data transfer) should never legitimately take
/// long; a short bound here catches a genuinely broken `yt-dlp`/network
/// without needing to be user-configurable.
const METADATA_PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// Runs a yt-dlp (or other) command with a hard timeout, killing the
/// process (and any child it spawned, e.g. ffmpeg) if it's exceeded —
/// without this, a stalled download (YouTube throttling a stream to
/// near-zero, or a genuinely hung process) would tie up a concurrency slot
/// forever with no way to recover.
async fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Result<std::process::Output, YtDlpError> {
    cmd.kill_on_drop(true);
    let child = cmd.spawn()?;
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(result) => Ok(result?),
        Err(_) => Err(YtDlpError(format!(
            "yt-dlp did not finish within {}s (likely stalled or throttled by YouTube)",
            timeout.as_secs()
        ))),
    }
}

const YOUTUBE_HOSTS: &[&str] = &[
    "youtube.com",
    "www.youtube.com",
    "m.youtube.com",
    "music.youtube.com",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
pub enum Format {
    #[serde(rename = "1080p")]
    #[default]
    P1080,
    #[serde(rename = "720p")]
    P720,
    #[serde(rename = "480p")]
    P480,
    #[serde(rename = "360p")]
    P360,
    #[serde(rename = "audio")]
    Audio,
}

impl Format {
    pub fn cache_key_suffix(&self) -> &'static str {
        match self {
            // Kept as "default" rather than "1080p" so cache entries written
            // before quality tiers existed (when this was the only video
            // format) remain valid.
            Format::P1080 => "default",
            Format::P720 => "720p",
            Format::P480 => "480p",
            Format::P360 => "360p",
            Format::Audio => "audio",
        }
    }

    fn max_height(&self) -> Option<u32> {
        match self {
            Format::P1080 => Some(1080),
            Format::P720 => Some(720),
            Format::P480 => Some(480),
            Format::P360 => Some(360),
            Format::Audio => None,
        }
    }
}

/// The HTTP `Content-Type` for a job's output file, known as soon as the
/// extension is resolved (before the download itself starts) so it can be
/// set on a response that streams the file while it's still being written.
pub fn content_type_for(format: Format, ext: &str) -> &'static str {
    if format.max_height().is_some() {
        return "video/mp4";
    }
    match ext {
        "m4a" => "audio/mp4",
        "webm" => "audio/webm",
        "opus" => "audio/opus",
        "ogg" => "audio/ogg",
        "mp3" => "audio/mpeg",
        _ => "application/octet-stream",
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
        Format::P1080 | Format::P720 | Format::P480 | Format::P360 => Ok("mp4".to_string()),
        Format::Audio => {
            let mut cmd = Command::new("yt-dlp");
            cmd.args([
                "-f",
                "bestaudio",
                "--skip-download",
                "--print",
                "%(ext)s",
                "--no-warnings",
                "--",
                url,
            ]);
            let output = run_with_timeout(&mut cmd, METADATA_PROBE_TIMEOUT).await?;

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
pub async fn download_to_file(
    url: &str,
    format: Format,
    dest: &Path,
    timeout: Duration,
) -> Result<(), YtDlpError> {
    let format_args: Vec<String> = match format.max_height() {
        Some(h) => vec![
            "-f".to_string(),
            format!("bv*[height<={h}]+ba/b[height<={h}]"),
            "--merge-output-format".to_string(),
            "mp4".to_string(),
            // A regular MP4's index (moov atom) is normally written last,
            // once ffmpeg knows final byte offsets — making the file
            // unplayable until the merge finishes. A fragmented MP4 writes
            // an empty moov upfront and self-contained fragments after, so
            // whatever's been written so far is always playable, which lets
            // /api/videos/:id stream this file to a <video> tag while it's
            // still being written (see JobManager::stream_downloading).
            "--postprocessor-args".to_string(),
            "ffmpeg:-movflags frag_keyframe+empty_moov".to_string(),
        ],
        None => vec!["-f".to_string(), "bestaudio".to_string()],
    };

    let dest_str = dest
        .to_str()
        .ok_or_else(|| YtDlpError("cache path is not valid UTF-8".to_string()))?;

    let mut cmd = Command::new("yt-dlp");
    cmd.args(&format_args).args([
        "-o",
        dest_str,
        // Without this, yt-dlp downloads to "<dest>.part" and only
        // renames it to `dest` once complete — meaning `dest` never
        // exists (let alone grows) while downloading, which would
        // silently defeat progressive streaming for exactly the
        // single-stream (no-merge) case where it actually works.
        "--no-part",
        // Without --no-part there was never a stale file at `dest` to
        // worry about resuming. With it, any leftover file there (e.g.
        // from a prior attempt killed mid-download) makes yt-dlp's
        // default --continue behavior try an HTTP Range resume against
        // it — which 416s if that offset doesn't correspond to a valid
        // range on the current response. We always want a fresh
        // download (see also the proactive cleanup in job.rs), never a
        // resume, so disable that.
        "--no-continue",
        "--no-warnings",
        "--no-progress",
        "--",
        url,
    ]);
    let output = run_with_timeout(&mut cmd, timeout).await?;

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
