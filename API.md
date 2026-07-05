# API reference

Every endpoint requires the access token, via either:
- `Authorization: Bearer <AUTH_TOKEN>` header (for API clients/curl), or
- an `auth_token` cookie (what the bundled frontend uses, since a plain
  `<video src>`/`<a download>` can't attach a custom header).

Missing or wrong token returns `401` with `{"error": "invalid or missing token"}`.

Served from the same origin as the frontend, under `/api/*` (see
`Caddyfile`) — no CORS handling needed.

## `POST /api/jobs`

Submit a video for download. Returns immediately with a job id — the
download itself happens asynchronously.

**Request body:**
```json
{ "url": "https://www.youtube.com/watch?v=...", "quality": "1080p" }
```
`quality` is optional (defaults to `"1080p"`) and must be one of: `"1080p"`,
`"720p"`, `"480p"`, `"360p"`, `"audio"`. Video qualities are a height cap,
not an exact match — yt-dlp picks the best available format at or below
that height. `"audio"` downloads the best available audio-only stream in
its native container (no video, no re-encode).

**Response `200`:**
```json
{ "job_id": "8b1c9f3a-....-....-....-............" }
```

If the requested video+quality is already cached, the returned job is
immediately in the `ready` state — poll it once and go straight to
fetching `/api/videos/:id`.

If another client already requested the same video+quality and it's still
downloading, the same `job_id` is returned — both clients end up polling
the same job.

**Errors:**
- `400` — empty/unparseable URL, or a URL that isn't actually a YouTube
  link (e.g. `{"error": "could not extract a YouTube video ID from that URL"}`),
  or a file too large for the configured cache
  (`{"error": "downloaded file (...) exceeds the cache capacity (...) and cannot be cached"}`)

## `GET /api/jobs/:id`

Poll for job status. Recommended interval: 1-2s.

**Response `200`:**
```json
{ "id": "...", "cache_key": null, "status": { "status": "queued" } }
{ "id": "...", "status": { "status": "downloading" } }
{ "id": "...", "status": { "status": "ready" } }
{ "id": "...", "status": { "status": "failed", "error": "yt-dlp's stderr output" } }
```
(`cache_key` is never actually present — it's an internal field skipped
during serialization; shown above only to clarify shape.)

Note `status` is a nested object (`status.status`, and `status.error` when
failed), not a flat string.

**Errors:**
- `404` — unknown job id

Finished jobs (`ready`/`failed`) are pruned automatically about an hour
after they last changed state, so don't rely on a job id remaining pollable
indefinitely.

## `GET /api/videos/:id`

Fetch the actual media once the job's status is `ready`. Standard file
serving with `Range` request support (seek/scrub works out of the box —
send a `Range: bytes=...` header as any `<video>`/`<audio>` element does
automatically).

**Errors:**
- `404` — job not found, not yet `ready`, or the cached file was evicted
  (LRU) before you fetched it — in that case, `POST /api/jobs` again with
  the same URL to re-trigger a download.

## Typical frontend flow

1. `POST /api/jobs` with the URL and quality → get `job_id`.
2. Poll `GET /api/jobs/:job_id` every ~1-2s until `status.status` is
   `ready` or `failed`.
3. On `ready`, either:
   - set a `<video>`/`<audio>` element's `src` to `/api/videos/:job_id`
     directly (the browser sends the `auth_token` cookie automatically,
     including on Range requests, so native seeking/scrubbing works), or
   - use a plain `<a href="/api/videos/:job_id" download>` to trigger a
     native browser download.
4. On `failed`, show `status.error` to the user; there's no auto-retry, so
   surface a manual "try again" action that just re-submits the job.
