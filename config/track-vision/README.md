# Configurable track-vision profile

[Project](../../README.md) · [Behavior authority](../../SPEC.md)

This profile selects the model/MCP server and defines the track-only extraction
policy and strict output schema. The generic Python/Node adapter consumes these
files through OCR_CONFIG; the native bot validates its typed result and owns the
preview, search and correction conversation. The selected model is
`glm-5.3-flash` in mcp.json, without model-specific bot logic.

- [MCP server](mcp.json): pinned official package, model and inference limits.
- [Policy binding](policy.json): selected server/tool, arguments, prompt/schema
  files and outcome mapping. These bindings belong to this profile's consumer.
- [Recognition instructions](extract-track.md): image data boundaries, music
  metadata extraction and rejection rules.
- [Result contract](result.schema.json): separate artist/title on success, explicit
  rejection statuses, optional metadata only when visible.
- [User messages](messages.json): trusted rejection/service messages in all eight
  UI languages. Return these configured messages, not arbitrary model prose.

The plain/quoted query templates are also policy data. They are search text, not
shell commands. Configured recognition automatically uses the phrase query to
look for a matching video; treat recognized values as literal data. Download
still requires an explicit Music/Video or other format choice.

The client must inject the required Z_AI_API_KEY from a private environment/secret
source. No credential belongs in these files. The MCP child must receive the
required key explicitly; do not assume an SDK forwards every parent variable.
Package/model selection is pinned here rather than in bot source.

Consume model output as untrusted data: validate the schema and combined query
bound, reject malformed/unknown output, and never execute model-produced commands
or tool calls. A prompt does not prove complete injection detection. Provider,
quota, timeout and schema errors are service failures, distinct from the model's
not_recognized status. Do not silently use another backend on rejection/failure.

An ok result becomes a correctable artist/title preview and automatic YouTube
lookup. Matching names in a result title expose its source/link and direct
download buttons; uncertain matches keep the results list. Search exact names
uses the quoted template for later explicit searches. Configured rejection
statuses stop search. This profile does not authorize automatic media downloads.

Qualify the chosen provider, representative images and deadlines in your own
environment. A successful classification or extraction does not prove perfect
factual accuracy or general injection resistance. Optional visible metadata can
also be wrong; keep the editable preview and validated output boundary.

Install dependencies with `npm ci --ignore-scripts` in this directory. Set
OCR_PYTHON to a Python with Pillow, OCR_SCRIPT to the absolute scripts/vision.py
path, and OCR_CONFIG to the absolute policy.json path. Set OCR_SECRETS_FILE to
a service-owned0400 JSON file mapping the required credential names to values,
or supply them through the process environment. An explicitly configured missing
secret file fails; it does not fall back to environment credentials. No runtime
npx installation is performed. A managed installation may link node_modules to
a separately installed dependency tree; the public source does not depend on
such a link or a machine-specific location.

The selected server requires a JPEG/PNG filename and accepts at most 5 MiB.
Native scratch names have no suffix, so image_input config selects a private
lossless PNG transport copy with the server's 5 MiB cap. Original PNG/JPEG/WebP
input still has the native 10 MiB/16M-pixel guards; converted images that exceed
the provider cap are explicitly rejected. Both copies are removed after the call.

The [native hook](../../src/ocr.rs) accepts structured results only when
OCR_CONFIG is present. Without it, scripts/ocr.py is the explicit local CPU
alternative. Recognition invokes one configured MCP tool; only schema-validated
data or trusted configured messages enter Telegram. Corrections invoke the
same helper with --render, without credentials, image input or MCP transport.
Recognized artist/title are literal values, including quotes and braces; search
templates substitute them once. Combined fields fit 240 characters.

Use the source repository's adapter tests for deterministic transport/schema
checks. Record any live-provider accuracy/latency evaluation separately; mocked
success does not qualify every image, provider or quota condition.
