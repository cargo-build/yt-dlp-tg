# Downloader behavior

[Project](README.md)

Authority: this specification defines the generic application contracts.
Operational host choices and private deployment evidence are maintained by each
operator outside the source repository.

- DL-1: Only users permitted by the configured access policy may trigger extraction,
  search or download. ALLOWED_USER_IDS=all explicitly permits all positive Telegram
  user IDs; otherwise a nonempty numeric allowlist is required. Missing, empty
  and malformed settings must fail closed. Public access retains per-user
  callback/correction ownership, queue reauthorization and all media limits.
  The identity-only /id command may return the requester's numeric ID without
  starting media work.
- DL-2: HTTP(S) URL support is determined by the installed yt-dlp extractors,
  including generic extraction. Failed extraction falls back to text search.
  Plain text uses YouTube by default; a paginated menu exposes the installed
  yt-dlp text-search providers. URL-only providers must not pretend to support search.
- DL-3: Search results expose titles and source links, pagination, result
  selection, and audio/video choices. Direct URLs expose the same format choice.
  Callback state must belong to the requesting user/chat/message and expire.
- DL-4: Offer Opus audio and source-native AAC music in M4A. Preserve
  existing Opus; convert other source codecs to high-quality Opus when needed.
  The AAC choice copies the best available source AAC without re-encoding or
  bitrate presets; bitrate/channels depend on the source. Never synthesize AAC
  from another codec.
  Version the native-AAC cache policy so earlier converted music cannot hide
  the change; preserve old state for rollback and pending accepted work.
  Opus remains lossy. Use Telegram voice
  and audio messages respectively, with a meaningful source title, rather than
  document-only audio. Video delivery produces a real, streamable MP4 with
  H.264/yuv420p video and AAC audio when audio is present. Copy compatible
  streams; convert incompatible SDR streams on CPU without changing the pinned
  source selection or raising its height. Do not send MKV as a video substitute.
  Validate codecs, container, dimensions, final size and faststart before upload.
  New video cache policies must not replay older document IDs; accepted queued
  video requests survive the policy upgrade and use the new MP4 pipeline.
  Offer Opus first as the primary music action for both links and search
  results, and keep native AAC/M4A as the explicit music-player alternative.
  Telegram plays Opus as a voice message while M4A opens in the music player.
  When the user selects native AAC and the source has no AAC, deliver Opus in
  the same request and state the substitution in the terminal status; neither a
  failure message nor a second button press is acceptable. Embed the selected
  video's thumbnail, when available, in both files without changing native
  audio packets. No uploaded OCR image becomes the cover. Version covered audio
  cache policies so old bare uploads cannot hide the change; accepted prior
  native-AAC jobs survive the artwork upgrade and use the covered pipeline.
  Embedded artwork does not turn a voice message into a Telegram music message.
- DL-5: Video must not exceed 1080p. Reject formats with unknown height rather
  than bypassing the cap. Enforce the configured Telegram upload limit.
  /start and /help explain the file/height/time limits and that a user's Premium
  subscription does not raise this bot's upload limit. Choosing Video opens
  known resolution/size options; oversized and missing resolutions say
  unavailable directly on their buttons. Include audio when estimating split
  video streams and label estimates honestly. If no usable size estimate is
  available, mark the resolution unavailable before download.
  For direct HTTP media formats missing size metadata, bounded header-only
  requests may obtain a positive whole-file Content-Length using the extractor's
  request headers and configured media proxy. Never download a body to populate
  the menu. Reject partial/compressed/error/non-media responses; missing sizes
  remain unavailable. Preserve the height cap and include the audio size.
  Download only the selected safe
  format IDs at the selected height, with no larger fallback. Unavailable
  callbacks explain the reason without starting media work or invalidating the
  valid menu. Bound temporary download growth and preserve final-size rejection,
  timeout/group cleanup and private scratch cleanup. Video cache identities
  distinguish explicit resolution choices; previous private state is retained.
  Keep video files within the upload cap. Audio extraction inputs may exceed
  that cap within the same bounded scratch budget; their final audio output
  must still fit the upload cap.
  Video normalization shares the existing download deadline and bounded job
  directory with extraction. Conversion may increase file size; menu sizes
  describe source estimates and the final upload cap remains authoritative.
  Do not truncate video to force it under the cap. Preparing the MP4 remains
  visible in the combined progress message before Sending starts.
- DL-6: Bot prose is localized in English, Chinese, Hindi, Spanish, Arabic,
  French, Portuguese and Russian, chosen from Telegram language_code. Unknown
  languages use English. Source titles and provider names retain their spelling.
- DL-7: Use compio with io_uring, native Teloxide, and Tako for optional webhooks.
  Do not start a Tokio executor. Downloader/FFmpeg child I/O is a separate boundary.
- DL-8: Secrets come from explicit environment/file configuration. Do not expose
  tokens, cookies, proxies or subprocess stderr in Telegram/logs. Clean per-job
  files after uploads and failures. Drain accepted work on graceful shutdown.
- DL-9: Persist successful typed Telegram file IDs in redb, keyed by bot ID,
  canonical selected source URL and output-policy version. Repeated delivery
  reuses file_id without media download/conversion, scratch files or upload.
  Store only validated successful response IDs. Never substitute file_unique_id.
  An explicit invalid-file-ID rejection permits eviction and one cold retry;
  other Telegram errors preserve cache state. Cache failures are explicit.
  Preserve authorization and avoid concurrent duplicate cold uploads. All redb
  transactions run off the compio executor and end before returning across await.
  For recognized YouTube extraction/search metadata, a validated video ID owns
  source identity: short/watch/mobile/music/shorts/embed/live URLs for that ID
  share the canonical https://www.youtube.com/watch?v=ID key, without tracking,
  playlist or playback-position parameters. A recognized YouTube item without a
  valid ID fails explicitly. Other extractors keep their validated webpage URL.
  In the single deployed process, the media slot covers cache lookup, cold
  download/upload and committed cache write: a second same-output request waits,
  then reuses the first successful Telegram file_id. User/chat IDs do not split
  shared cache entries; different output policies remain distinct. Failed work
  is not cached as success and a later request may retry.
- DL-10: Run the deployed bot on the configured Linux host. Use one polling
  instance under persistent host-native supervision and an unprivileged service
  account. Preserve private token/state files and the durable file-ID cache during
  migration. Start the destination only after the laptop poller has stopped; do
  not require a new public inbound port. Qualify the target binary/runtime before
  cutover and retain a recoverable source deployment until destination startup
  succeeds. Qualify CPU OCR on the target before enabling image input.
  Repository deployment examples require an explicit numeric allowlist or all;
  installed account IDs and credentials belong in private host configuration.
- DL-11: Acknowledge accepted searches/links before waiting for the media slot
  or starting extraction. Show the actual queued, checking, searching,
  downloading and sending stages, then replace progress with results, success
  or failure. Unsupported input receives an immediate localized explanation and
  a text/link alternative; never imply OCR is running when it is unavailable.
  Preserve authorization and callback ownership before feedback.
  Operator diagnostics must correlate accepted link requests, extraction
  outcomes, video availability reasons and download outcomes with request/job
  identifiers. Log canonical Instagram/YouTube source identities without share
  queries; other source hosts may be recorded by origin only. Do not log raw
  query text, arbitrary URL paths, credentials, extractor CDN URLs/headers or
  raw subprocess/Telegram errors. Successful extraction with unavailable video
  formats is an explicit diagnostic outcome, not an unobserved silent path.
  Split metadata, source download, MP4 normalization, cache and file-open times
  from upload time. Upload diagnostics record bytes handed to the HTTP body,
  RAM-body read time, gaps between body reads, synchronous request-poll time,
  executor timer delay and elapsed time after all expected file bytes were read.
  These are application boundaries, not TCP acknowledgements or isolated kernel,
  TLS or server timings. Emit bounded numeric summaries at completion/failure/
  cancellation and during long uploads (first after one second, then every30s),
  never per chunk. Best-effort Linux ring snapshots contain only whitelisted
  numeric process-local queue counters and opaque descriptors; missing counters
  remain unavailable. A bounded ring read runs off the executor. Diagnostics
  otherwise run with media work on the same executor, do not buffer/retry
  media or start another runtime, and cannot turn a successful send into failure.
  Active media work keeps Downloading and Sending rows together in the same
  existing status message, with progress updates every3.5seconds. Use observed
  bytes/known totals; distinguish estimates, pending work and indeterminate
  totals. File-ID reuse must not pretend to transfer a file again. Keep byte
  HTTP body chunks bounded, aggregate split downloads, and preserve work
  when cosmetic edits fail. Progress edits must not stall child pipe draining
  or uploads, and the updater ends before terminal success/failure status.
  A cache-miss active worker loads its validated output fully into one RAM
  buffer before HTTP upload. Enforce the current API upload cap and reject
  empty, oversized, truncated or grown inputs before sending. The existing
  media slot bounds live buffers to one; queued jobs retain request metadata,
  and cache hits reuse Telegram IDs without loading media. File loading is a
  separate measured phase. RAM ownership must survive until the request drops
  and be released on success, error and cancellation without media cloning.
- DL-12: Authorized photos and PNG/JPEG/WebP image documents can produce a short
  recognized-text preview through local CPU OCR. Acknowledge first, show actual
  queue/download/recognition stages, and share the existing media semaphore.
  Local CPU OCR shows the query before explicit search. Configured track
  recognition uses the automatic lookup in DL-13. Do not guess corrections or
  automatically download media.
  Separate artist names around `&` with whitespace in the OCR query, preserving
  recognized letters and punctuation rather than inventing name corrections.
  Bound incoming bytes, decoded pixels, subprocess output and execution time;
  reject invalid/oversized input and explain empty/failed OCR in the selected
  UI language. Download only after authorization, use private scratch files,
  and remove them on all completed/error paths. Local OCR must not upload images
  or fetch models on demand; an explicitly configured remote vision backend may
  send images only to its selected provider. Disabled OCR retains an
  honest unsupported-input response. All existing text/URL/cache behavior remains.
- DL-13: Track recognition preserves separate artist/title fields. MCP connection,
  model selection, extraction prompt, output schema and rejection policy belong
  in external configuration. Successful recognition automatically searches
  YouTube using the configured artist/title phrase query. Show the recognized
  fields and suggested video's title/link, with direct Music/Video download
  buttons when a result title matches both artist and track. Prefer the first
  matching result in YouTube's order; if none clearly matches, show the normal
  results list. Never download before a format button is pressed. Keep other
  results and existing Opus/M4A/video choices reachable. Unreadable/non-track input and detected
  instructions directed at the recognizer produce distinct configured rejection
  outcomes and stop search. Treat image/model content as untrusted data: never
  execute returned instructions, follow returned tool calls or reveal credentials.
  Validate structured output independently of the prompt. Detection by a model
  does not guarantee that every injection is recognized. Provider/quota/timeout
  and malformed-output errors are service failures, not evidence of unreadable
  images. Never silently use another backend on rejection/failure.
  Display recognized track identity as one `Artist - Title` line without field
  labels in every interface language and result/provider/format menu. Keep artist
  and title separate internally for matching and owned corrections. Offer explicit plain and
  phrase YouTube searches with a plain-language explanation, without the label
  "Quoted search". Owned, versioned replies correct one field and render
  the configured templates without inference or automatic search; foreign,
  stale or replayed replies cannot mutate another user's conversation.

- DL-14: When Iggy is explicitly configured, authorized download clicks become
  durable per-recipient requests. Run a matching pinned local-only broker on the deployment host
  and a native compio client on the existing executor. Persist accepted request
  payloads/outbox state before publication. One serial consumer reuses the
  canonical typed Telegram cache before cold download, so same-output requests
  share one successful download/upload. Persist completion before manual offset
  acknowledgement. Duplicate or unknown/deleted request IDs cannot start another
  delivery. Iggy message IDs alone are not uniqueness constraints: resolve
  retained uncertain operations explicitly; any republish after lost proof must
  be harmless through durable application request IDs and completion state.
  Recheck bot/user/output policy after restart. Queue payloads contain identifiers,
  never media blobs, credentials or executable instructions. Bound pending work.
  On graceful stop, drain the active request, retain unstarted work, keep client
  liveness until processing ends, then shut it down before the compio runtime.
  Messages and consumer offsets use persisted broker durability. Preserve the
  accepted delivery when a cosmetic progress edit fails: attempt localized
  feedback, log a safe warning and continue media work. Preserve the
  documented standalone direct mode when Iggy is absent from configuration;
  failure of a configured broker never silently selects that mode. Telegram can
  accept a send before its result is recorded: crash recovery is at-least-once,
  without an exactly-once external-message guarantee.

Acceptance: text→YouTube results; provider/result page switches; supported URL→
format choice; failed URL→search; unauthorized/foreign/stale callbacks rejected;
native Opus/AAC copy and codec conversion; typed playback; persistent cache hits,
format/bot isolation and explicit invalid-ID recovery; ≤1080p selection; oversized upload
rejected; all eight locales; compio startup and both update modes.
Local OCR acceptance: authorized photo/document → receipt/stages → uncorrected query
preview → owned explicit YouTube search; corrected text uses normal search;
invalid/empty/oversized/timeout failures are localized and private scratch is cleaned.
Configured recognition additionally preserves artist/title through result menus,
explicit plain/quoted searches and owned corrections without repeat inference;
distinct rejection controls stop search. Configured recognition's automatic
lookup exposes direct buttons only for a matching title, or falls back to the
result list; its source title/link remain visible before download. Recognition accuracy and latency must be qualified for the selected provider,
images and deployment environment; no general accuracy guarantee is implied.

Operational limits and defaults are documented in README. Record live downloader and Telegram qualification for the chosen environment;
fixture tests do not prove compatibility with every hosting provider.
