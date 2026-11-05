# yt-dlp-tg

Copyright 2026 yt-dlp-tg contributors. Licensed under [Apache-2.0](LICENSE).
Dependencies retain their own licenses and notices.

A private Telegram music/video downloader using one native compio/io_uring
executor, native Teloxide/Cyper, optional Tako webhooks and an optional durable
Iggy queue. See [SPEC.md](SPEC.md) for the behavior contract.

## Behavior

- Send text to search YouTube, or a URL for extraction through installed yt-dlp
  providers. Failed URL extraction falls back to search. Source/result menus
  have pagination; every result shows its source link.
- Opus is the primary music action: it copies native Opus or converts other
  codecs at high quality and plays as a Telegram voice message. M4A music copies
  the best available native AAC without re-encoding or bitrate presets and opens
  in the music player. Selecting M4A on a source without AAC delivers Opus in the
  same request and states the substitution. Both audio choices embed the selected
  video's thumbnail when available; Telegram may still show a waveform for Opus
  voice.
- Video is delivered as streamable MP4 with H.264/yuv420p and AAC where present.
  Compatible streams are copied; incompatible SDR streams are converted on CPU
  under the existing download timeout and scratch limits. Conversion can increase
  file size; the final upload cap is always enforced without truncation.
  Video is limited to1080p. Resolution buttons show audio-inclusive source estimates;
  oversized, unknown-size and unsupported choices are unavailable. Cloud Bot API
  uploads default to50MiB. A user's Premium subscription does not raise that cap.
- Successful uploads are cached as typed Telegram file IDs in redb. YouTube URL
  aliases share a video identity. One media slot covers cache lookup through
  committed result storage, so concurrent requests share one successful cold
  download/upload and subsequent recipients receive the stored file ID.
- One status message shows Downloading and Sending together, with3500ms progress
  updates, measured bytes or honest indeterminate bars, and explicit cache reuse.
  Upload counters measure bytes handed to the request body; completion still
  waits for Telegram's response. Cosmetic edit failures do not cancel media work.
  Preparing MP4 is shown during normalization without an invented encoder percentage.
- Optional image recognition keeps artist/title separate, searches for a matching
  video and offers download/correction controls. The selected MCP model, prompt,
  schema and messages live in [configuration](config/track-vision/README.md).
  Local CPU OCR is an explicit alternative; recognition never downloads media
  before a format button is pressed.
- English, Chinese, Hindi, Spanish, Arabic, French, Portuguese and Russian UI.
  Downloads use an explicit allowlist or `ALLOWED_USER_IDS=all`. `/id` returns the caller's numeric
  ID without download permission; `/start`, `/help` and `/search` are supported.

## Requirements and running

Use Linux with io_uring available, Rust1.99.1+, yt-dlp2026.10.21+ and FFmpeg.
For YouTube, install yt-dlp's matching default/EJS dependencies and keep Deno on
PATH. The process rejects a different I/O driver. Pinned Git dependencies provide
the native clients; Tokio crates may be linked, but no Tokio executor is started.

```sh
cargo build --locked
BOT_TOKEN_FILE=/absolute/private/bot-token ALLOWED_USER_IDS=123456789 ./target/debug/yt-dlp-tg
```

The ID above is a placeholder. Supply your own numeric Telegram ID(s), or set
`ALLOWED_USER_IDS=all` explicitly to open the bot to everyone. Keep
token bytes in a private file or managed environment, never in source or argv.
Bot menus expire after20minutes and are discarded on restart.

## Configuration

| Variable | Meaning/default |
|---|---|
| `BOT_TOKEN` / `BOT_TOKEN_FILE` | Token environment value or private file; default file `/run/secrets/GET_MUSIC_VIDEO_BOT` |
| `ALLOWED_USER_IDS` | Required: positive numeric IDs, comma-separated, or `all` for public access; empty/missing values fail |
| `MODE` | `polling` or `webhook` |
| `TG_API_BASE` / `TG_PROXY` | Bot API origin, default api.telegram.org; optional explicit proxy |
| `LISTEN_ADDR` | `127.0.0.1:8080` for webhooks |
| `WEBHOOK_PUBLIC_URL` / `WEBHOOK_PATH` | Required HTTPS origin in webhook mode; path defaults to `tg-webhook` |
| `YTDLP_PATH` | `yt-dlp` executable |
| `YTDLP_PROXY` / `YTDLP_COOKIES` | Optional explicit proxy or operator-supplied cookie file |
| `DOWNLOAD_DIR` | Private scratch parent, default `/tmp/yt-dlp-tg` |
| `CACHE_PATH` | Default `$XDG_STATE_HOME/yt-dlp-tg/files.redb`, otherwise `$HOME/.local/state/yt-dlp-tg/files.redb` |
| `MAX_UPLOAD_MB` |50MiB;1–50 for cloud API, up to2000 for a suitably configured local/custom Bot API |
| `PROCESS_TIMEOUT_SECS` / `DOWNLOAD_TIMEOUT_SECS` |180 /900 seconds per child |
| `OCR_PYTHON` / `OCR_SCRIPT` | Optional paired executable/script; neither disables image input |
| `OCR_CONFIG` / `OCR_SECRETS_FILE` | Absolute configured-vision policy and optional private credential JSON |
| `OCR_TIMEOUT_SECS` |60; range1–300 |
| `IGGY_CONNECTION_STRING` / `IGGY_CONNECTION_STRING_FILE` | Optional authenticated loopback URI or absolute owner-only file; exactly one |
| `IGGY_STREAM` / `IGGY_TOPIC` | `yt_dlp_tg` / `downloads` |
| `IGGY_MAX_PENDING` |256 retained recipient requests; range1–256 |

Empty or partial Iggy settings fail startup; configured broker failures never
switch to direct delivery. With no Iggy settings, standalone direct delivery
remains available. Browser credentials are never read implicitly. yt-dlp config
files/plugins are disabled; its cookie jar must be writable by its owner.

## Diagnostics

The default `RUST_LOG=yt_dlp_tg=info` emits structured request/job IDs, accepted
link and extraction stages, per-resolution availability reasons, and delivery
outcomes. Instagram/YouTube video identities remain visible without sharing
queries. Other hosts are logged by origin only; arbitrary paths, query text,
CDN URLs/headers, credentials and raw provider errors are excluded. Use the
request ID to connect intake to availability, then the job ID for delivery.
`unknown_size`, `too_large` and `missing` distinguish unavailable menu options
even when extraction succeeded. These are service logs, not a metrics/alerting
platform; protect and rotate them with the deployment's existing log controls.

`transfer_phase` separates metadata, source download, MP4 preparation, cache,
file open, RAM-buffer loading, cached sends and slot wait. Waiting-notice delivery is timed separately
from slot contention. Repeated metadata extractions have separate events.
`upload_diagnostics` reports the actual file bytes
read into the owned HTTP body, read-await time, gaps between reads, synchronous
request-poll time, timer delay and time after the expected bytes were read.
It reports completion/failure/cancellation, plus a sample after one second and
every30seconds during long sends. No per-chunk logs or extra runtime is used.
`ring_diagnostics` is a bounded, best-effort numeric snapshot of this process's
Linux ring heads/tails, without raw fdinfo or addresses.

Each active cache-miss worker loads one complete capped output into RAM before
sending. The shared media slot limits this to one file buffer; Iggy retains
only request metadata for waiting jobs. Budget one file (up to the configured
API cap) plus HTTP/allocator/runtime overhead per worker. Cached file-ID sends
allocate no media buffer. The buffer is moved into the existing chunked reader,
preserving progress and transport framing. `upload_buffer_read` measures disk
loading separately; body-read timings now describe RAM reads.

`body_read_await_us` measures the RAM reader rather than disk I/O; large
`body_gap_ms` means the consumer stopped asking for file bytes. Long polls or
timer lag can indicate executor contention. `after_body_ms` still includes
buffer draining, network transfer and API processing. Body bytes are not TCP
acknowledgements, and these fields do not isolate TLS, socket writes or every
kernel operation. Other runtime tasks can contribute; correlate with private
host CPU/TCP observations before assigning a cause. A low-volume or cached send
can finish without a timer/ring sample. Missing kernel fields remain unknown.
An abandoned in-flight read stays visible as pending; if resumed, its abandoned
interval is counted separately rather than charged as a body-demand gap.

## Durable queue and data

Iggy uses the matching pinned native broker/client and one persisted partition.
The outbox/recipient state shares the existing redb handle. Persist before
publish; record terminal completion before committing the inclusive broker
offset, then remove the row. Startup replays retained IDs once. Completed or
unknown IDs cannot start another delivery; Iggy message IDs alone are not
deduplication keys. Current bot, allowlist and output policy are rechecked.

Shutdown finishes the active delivery and preserves the backlog while keepalive
remains running. Telegram sends are at least once across a crash after external
acceptance but before local completion storage. This is a single-host/worker
deployment, without replica fault tolerance. Broker history is not automatically
trimmed; monitor free disk and plan coordinated drained maintenance.

Protect both redb and broker data. Queued requests contain user/chat/message IDs
and source metadata; cached entries contain reusable Telegram file IDs. Scratch
and status/error handling are bounded, and credentials are excluded from logs.
Configured remote recognition sends images to the selected provider. Local CPU
OCR does not upload images or fetch model weights on demand.

## Deployment and verification

[Alpine/OpenRC deployment](deploy/alpine/README.md) covers the generic templates
and private credential/provisioning boundaries. Qualify your own host, downloader
egress and selected recognition backend before serving requests.

```sh
cargo build --locked
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
python3 tests/smoke.py
python3 tests/ocr_helper_test.py
python3 tests/vision_adapter_test.py
```

Smoke tests use localhost Telegram/downloader fixtures and send no real messages.
Run Python tests with an interpreter containing Pillow for image cases. Install
the locked Node profile dependencies for real-SDK adapter coverage; otherwise
some cases use offline stubs or explicitly skip unavailable capabilities.

`tests/queue_smoke.py --broker /absolute/path/to/iggy-server` adds a real matching
loopback broker with fake Telegram/downloader services. `--test-binary` accepts
the compiled Rust test executable for the ignored recovery/drain test; `--trace`
optionally records a native probe with strace. `tests/media_smoke.py` exercises
real local yt-dlp/FFmpeg codecs, native packet preservation and thumbnail tags.
These are scoped fixture checks, not guarantees for every video host or image.
