# API reference

All endpoints require `Authorization: Bearer <AUTH_TOKEN>`. Missing or wrong
token returns `401` with `{"error": "invalid or missing token"}`.

Served from the same origin as the frontend, under `/api/*` (see
`Caddyfile`) — no CORS handling needed.

## `POST /api/jobs`

Submit a video for download. Returns immediately with a job id — the
download itself happens asynchronously.

**Request body:**
```json
{ "url": "https://www.youtube.com/watch?v=...", "audio_only": false }
```
`audio_only` is optional, defaults to `false`.

**Response `200`:**
```json
{ "job_id": "8b1c9f3a-....-....-....-............" }
```

If the requested video+format is already cached, the returned job is
immediately in the `ready` state — poll it once and go straight to
fetching `/api/videos/:id`.

If another client already requested the same video+format and it's still
downloading, the same `job_id` is returned — both clients end up polling
the same job.

**Errors:**
- `400` — empty/unparseable URL (e.g. `{"error": "could not extract a YouTube video ID from that URL"}`)

## `GET /api/jobs/:id`

Poll for job status. Recommended interval: 1-2s.

**Response `200`:**
```json
{ "id": "...", "status": "queued" }
{ "id": "...", "status": "downloading" }
{ "id": "...", "status": "ready" }
{ "id": "...", "status": "failed", "error": "yt-dlp's stderr output" }
```

**Errors:**
- `404` — unknown job id

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

1. `POST /api/jobs` with the URL → get `job_id`.
2. Poll `GET /api/jobs/:job_id` every ~1-2s until `status` is `ready` or
   `failed`.
3. On `ready`, set a `<video>`/`<audio>` element's `src` to
   `/api/videos/:job_id` (with the auth header attached, e.g. via a fetch +
   blob URL, since `<video src>` can't set custom headers directly).
4. On `failed`, show `error` to the user; there's no auto-retry, so surface
   a manual "try again" action that just re-submits the job.
