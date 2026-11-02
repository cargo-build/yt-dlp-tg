#!/usr/bin/env python3
"""Generic configured MCP track-vision adapter (DL-13).

Contract (SPEC.md DL-13): the native runner calls
``python -I scripts/vision.py --config POLICY_JSON [--secrets-file JSON]
(--image ABS --max-bytes N --max-pixels N | --render --artist=V --title=V)
[--language LANG]``. Stdout carries exactly one JSON object:
``{"kind":"track","artist":..,"title":..,"plain_query":..,"quoted_query":..}``
for a validated preview action result or a render pass, and
``{"kind":"rejected","message":..}`` with a configured localized message for a
configured rejection or a safe service failure. Image-guard failures keep the
OCR helper exits (3 malformed/unsupported, 4 oversized bytes/pixels) and print
only the fixed stderr marker; configuration/internal failures exit 1 the same
way. No model, prompt, tool, schema or status-policy value lives here: those
come from the profile directory, and only the two generic policy actions and
the consumer field names are program constants. Credential values never reach
argv, logs or output; they are forwarded only to the MCP child environment.
"""

import argparse
import contextlib
import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

EXIT_INTERNAL = 1
EXIT_IMAGE = 3
EXIT_TOO_LARGE = 4

SAFE_MARKER = "vision: failed"

ACTION_PREVIEW = "preview_artist_and_title"
ACTION_REJECT = "reject"
KNOWN_ACTIONS = frozenset((ACTION_PREVIEW, ACTION_REJECT))

# Generic consumer contract: the two query templates and the one message
# section used for transport/schema/service failures (all data comes from the
# configured messages file).
QUERY_TEMPLATES = ("plain", "quoted")
SERVICE_MESSAGES = "provider_error"

# Substitution placeholders. Values are inserted once, verbatim; a value that
# itself contains a placeholder is never re-expanded.
PLACEHOLDER = re.compile(r"\{(artist|title|image_path|prompt|schema)\}")
CONTROL_CHARS = re.compile(r"[\u0000-\u001f\u007f-\u009f]")

# Lossless re-encode formats this adapter can produce for the configured
# server. The conversion format itself belongs to configuration; anything
# else fails explicitly instead of silently sending a different encoding.
SUPPORTED_IMAGE_FORMATS = frozenset({"PNG"})

NODE_CLIENT = "mcp-vision.mjs"
NODE_STDOUT_LIMIT = 1_048_576
# The Node client must finish its own child teardown (stdin EOF, SIGTERM,
# SIGKILL escalation, ~4s worst case in SDK 1.26.0) before this deadline; the
# native parent's outer process-group kill remains the final bound.
NODE_SHUTDOWN_GRACE_SECONDS = 6.0

# The MCP server child gets an explicit minimal environment: base process
# lookup variables, configured optional passthrough names, the required
# credential names and the profile's own env block. Nothing else is inherited.
BASE_ENV_PASSTHROUGH = ("PATH", "HOME", "TMPDIR")

CLIENT_RESULT = "result"


class ConfigError(Exception):
    """Invalid or incomplete configuration/usage; fails explicitly."""


class ServiceFailure(Exception):
    """Transport/schema/deadline failure; safe localized rejection."""


def _ocr():
    """Trusted file import of the sibling OCR helper (Python -I mode)."""
    module = globals().get("_OCR_MODULE")
    if module is None:
        source = Path(__file__).resolve().parent / "ocr.py"
        spec = importlib.util.spec_from_file_location("vision_ocr_helper", source)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        globals()["_OCR_MODULE"] = module
    return module


def _read_json(path, what):
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError):
        raise ConfigError(f"unreadable or invalid {what}") from None


def _read_text(path, what):
    try:
        return path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        raise ConfigError(f"unreadable {what}") from None


def _required_str(mapping, key):
    value = mapping.get(key)
    if not isinstance(value, str) or not value:
        raise ConfigError(f"missing config field: {key}")
    return value


def _string_list(policy, key):
    value = policy.get(key, [])
    if (not isinstance(value, list)
            or not all(isinstance(item, str) and item for item in value)):
        raise ConfigError(f"invalid config field: {key}")
    return list(value)


def _positive_int_field(policy, key):
    value = policy.get(key)
    if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
        raise ConfigError(f"invalid config field: {key}")
    return value


def _message(messages, section, language):
    """Localized configured message; a missing one is a config failure."""
    entry = messages.get(section) if isinstance(messages, dict) else None
    text = entry.get(language) if isinstance(entry, dict) else None
    if not isinstance(text, str) or not text.strip():
        raise ConfigError(f"missing message: {section}/{language}")
    return text


def _substitute(template, values):
    """Single-pass placeholder expansion; inserted values are never rescanned
    and bare words equal to a placeholder name stay literal."""
    return PLACEHOLDER.sub(
        lambda match: values.get(match.group(1), match.group(0)), template)


def _load_secrets(secrets_path, required_env):
    data = _read_json(Path(secrets_path).resolve(), "secrets file")
    if not isinstance(data, dict):
        raise ConfigError("secrets file must be an object")
    values = {}
    for name in required_env:
        value = data.get(name)
        if not isinstance(value, str) or not value:
            raise ConfigError(f"required secret not supplied: {name}")
        values[name] = value
    return values


def load_config(config_path):
    """Load and validate the whole profile; any gap fails explicitly.

    Credentials are deliberately not touched here: the render path needs
    none, so the secrets file is read only when an inference call is made.
    """
    path = Path(config_path).resolve()
    policy = _read_json(path, "policy")
    if not isinstance(policy, dict):
        raise ConfigError("policy must be an object")
    profile_dir = str(path.parent)

    server_name = _required_str(policy, "server")
    mcp = _read_json(path.parent / "mcp.json", "mcp server map")
    servers = mcp.get("mcpServers") if isinstance(mcp, dict) else None
    entry = servers.get(server_name) if isinstance(servers, dict) else None
    if not isinstance(entry, dict):
        raise ConfigError("configured server entry missing")
    command = entry.get("command")
    if not isinstance(command, str) or not command:
        raise ConfigError("server command missing")
    server_args = entry.get("args", [])
    if (not isinstance(server_args, list)
            or not all(isinstance(item, str) for item in server_args)):
        raise ConfigError("server args invalid")
    server_env = entry.get("env", {})
    if (not isinstance(server_env, dict)
            or not all(isinstance(k, str) and isinstance(v, str)
                       for k, v in server_env.items())):
        raise ConfigError("server env invalid")

    required_env = _string_list(policy, "required_environment")
    optional_env = _string_list(policy, "optional_environment")

    tool = _required_str(policy, "tool")
    tool_arguments = policy.get("tool_arguments")
    if (not isinstance(tool_arguments, dict)
            or not all(isinstance(k, str) and isinstance(v, str)
                       for k, v in tool_arguments.items())):
        raise ConfigError("tool arguments must be string templates")

    prompt_text = _read_text(path.parent / _required_str(policy, "prompt_file"),
                             "prompt file")
    schema_text = _read_text(
        path.parent / _required_str(policy, "result_schema_file"),
        "result schema file")
    schema = json.loads(schema_text)
    if not isinstance(schema, dict):
        raise ConfigError("result schema must be an object")
    messages = _read_json(path.parent / _required_str(policy, "messages_file"),
                          "messages file")
    if not isinstance(messages, dict):
        raise ConfigError("messages must be an object")

    default_language = _required_str(policy, "default_language")

    outcomes = policy.get("outcomes")
    if not isinstance(outcomes, dict) or not outcomes:
        raise ConfigError("outcomes must be a nonempty object")
    for action in outcomes.values():
        if action not in KNOWN_ACTIONS:
            raise ConfigError("unknown policy action")
    preview_statuses = [status for status, action in outcomes.items()
                        if action == ACTION_PREVIEW]
    if len(preview_statuses) != 1:
        raise ConfigError("preview outcome mapping must be unique")
    for status, action in outcomes.items():
        if action == ACTION_REJECT:
            _message(messages, status, default_language)
    _message(messages, SERVICE_MESSAGES, default_language)

    search_queries = policy.get("search_queries")
    if (not isinstance(search_queries, dict)
            or not all(isinstance(search_queries.get(key), str)
                       and search_queries[key] for key in QUERY_TEMPLATES)):
        raise ConfigError("search query templates missing")

    image_input = policy.get("image_input")
    if image_input is not None:
        if (not isinstance(image_input, dict)
                or image_input.get("format") not in SUPPORTED_IMAGE_FORMATS):
            raise ConfigError("unsupported image_input format")
        image_input = {
            "format": image_input["format"],
            "maximum_bytes": _positive_int_field(image_input, "maximum_bytes"),
        }

    config = {
        "profile_dir": profile_dir,
        "server": {"command": command, "args": list(server_args)},
        "server_env": dict(server_env),
        "required_env": required_env,
        "optional_env": optional_env,
        "tool": tool,
        "tool_arguments": dict(tool_arguments),
        "prompt_text": prompt_text,
        "schema_text": schema_text,
        "schema": schema,
        "messages": messages,
        "default_language": default_language,
        "search_queries": {key: search_queries[key]
                           for key in QUERY_TEMPLATES},
        "maximum_query_characters": _positive_int_field(
            policy, "maximum_query_characters"),
        "total_timeout_ms": _positive_int_field(policy, "total_timeout_ms"),
        "outcomes": dict(outcomes),
        "preview_status": preview_statuses[0],
        "image_input": image_input,
    }
    return config


def _reject_payload(messages, section, language):
    return {"kind": "rejected", "message": _message(messages, section, language)}


def _track_payload(config, artist, title):
    """Validate model/user data independently of the configured schema.

    Values are preserved literally: spacing, punctuation and ampersand
    spacing are the user's/model's own and are never rewritten here. Only
    missing/blank/controls-bearing values and a combined artist+title over
    the configured character bound are rejected; query templates may then
    legitimately render longer than the bound (template overhead).
    """
    for value in (artist, title):
        if not isinstance(value, str) or CONTROL_CHARS.search(value):
            raise ServiceFailure("artist/title missing or contain controls")
    if not artist.strip() or not title.strip():
        raise ServiceFailure("artist/title blank")
    if len(artist) + 1 + len(title) > config["maximum_query_characters"]:
        raise ServiceFailure("combined artist/title over configured bound")
    values = {"artist": artist, "title": title}
    queries = {key: _substitute(config["search_queries"][key], values)
               for key in QUERY_TEMPLATES}
    return {
        "kind": "track",
        "artist": artist,
        "title": title,
        "plain_query": queries["plain"],
        "quoted_query": queries["quoted"],
    }


def run_render(config, artist, title, language):
    """Build the preview from literal values; never starts the MCP client.

    The preview action is derived from the unique outcome mapping validated at
    load time; no model status is assumed here.
    """
    if config["outcomes"][config["preview_status"]] != ACTION_PREVIEW:
        raise ConfigError("preview outcome mapping invalid")
    try:
        return _track_payload(config, artist, title)
    except ServiceFailure:
        return _reject_payload(config["messages"], SERVICE_MESSAGES, language)


def _server_env(config, secrets_file, parent_env):
    env = {name: parent_env[name] for name in BASE_ENV_PASSTHROUGH
           if name in parent_env}
    for name in config["optional_env"]:
        if name in parent_env:
            env[name] = parent_env[name]
    if secrets_file is None:
        for name in config["required_env"]:
            value = parent_env.get(name)
            if not isinstance(value, str) or not value:
                raise ConfigError(f"required environment missing: {name}")
            env[name] = value
    else:
        env.update(_load_secrets(secrets_file, config["required_env"]))
    env.update(config["server_env"])
    return env


def _client_request(config, image, secrets_file):
    values = {"image_path": image,
              "prompt": config["prompt_text"],
              "schema": config["schema_text"]}
    arguments = {name: _substitute(template, values)
                 for name, template in config["tool_arguments"].items()}
    return {
        "profileDir": config["profile_dir"],
        "server": {
            "command": config["server"]["command"],
            "args": config["server"]["args"],
            "env": _server_env(config, secrets_file, os.environ),
            "cwd": config["profile_dir"],
        },
        "tool": config["tool"],
        "arguments": arguments,
        "resultSchema": config["schema"],
        "timeoutMs": config["total_timeout_ms"],
    }


def _run_client(request, timeout_ms):
    """Launch the sibling Node client in the inherited process group."""
    node = shutil.which("node")
    client = Path(__file__).resolve().parent / NODE_CLIENT
    if node is None or not client.is_file():
        raise ConfigError("node client unavailable")
    payload = json.dumps(request, ensure_ascii=False).encode("utf-8")
    try:
        completed = subprocess.run(
            [node, str(client)],
            input=payload,
            capture_output=True,
            timeout=timeout_ms / 1000 + NODE_SHUTDOWN_GRACE_SECONDS,
        )
    except subprocess.TimeoutExpired:
        raise ServiceFailure("client total deadline") from None
    if len(completed.stdout) > NODE_STDOUT_LIMIT:
        raise ServiceFailure("client output oversized")
    try:
        verdict = json.loads(completed.stdout.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        raise ServiceFailure("client verdict unreadable") from None
    if not isinstance(verdict, dict):
        raise ServiceFailure("client verdict malformed")
    return verdict


def _verdict_payload(config, verdict, language):
    if verdict.get("status") != CLIENT_RESULT:
        raise ServiceFailure("client reported a service failure")
    data = verdict.get("data")
    if not isinstance(data, dict):
        raise ServiceFailure("client result malformed")
    status = data.get("status")
    action = config["outcomes"].get(status)
    if not isinstance(status, str) or action is None:
        raise ServiceFailure("unknown model status in validated data")
    if action == ACTION_REJECT:
        return _reject_payload(config["messages"], status, language)
    return _track_payload(config, data.get("artist"), data.get("title"))


def _write_png_copy(source, maximum_bytes):
    """Private .png copy beside the input, bounded by the configured cap.

    The configured server accepts only suffixed .jpg/.jpeg/.png inputs while
    the native scratch file carries no suffix; the original is never touched.
    """
    ocr = _ocr()
    fd, temp = tempfile.mkstemp(prefix=".vision-", suffix=".png",
                                dir=str(Path(source).parent))
    os.close(fd)
    try:
        from PIL import Image

        with Image.open(source) as img:
            # PNG has no CMYK representation; JPEG inputs can use that mode.
            with (img.convert("RGB") if img.mode == "CMYK"
                  else contextlib.nullcontext(img)) as png:
                png.save(temp, format="PNG")
        if os.stat(temp).st_size > maximum_bytes:
            raise ocr.OcrFailure(ocr.EXIT_TOO_LARGE)
    except Exception:
        with contextlib.suppress(OSError):
            os.unlink(temp)
        raise
    return temp


@contextlib.contextmanager
def _normalized_image(image, config):
    """Yield the path the configured server receives (render never calls)."""
    image_input = config["image_input"]
    if image_input is None:
        yield str(image)
        return
    temp = _write_png_copy(image, image_input["maximum_bytes"])
    try:
        yield temp
    finally:
        with contextlib.suppress(OSError):
            os.unlink(temp)


def run_inference(config, image, max_bytes, max_pixels, language, secrets_file):
    _ocr().inspect_image(image, max_bytes, max_pixels)
    with _normalized_image(image, config) as model_image:
        request = _client_request(config, model_image, secrets_file)
        try:
            verdict = _run_client(request, config["total_timeout_ms"])
            return _verdict_payload(config, verdict, language)
        except ServiceFailure:
            return _reject_payload(config["messages"], SERVICE_MESSAGES, language)


def _positive_int(value):
    try:
        number = int(value)
    except (TypeError, ValueError):
        raise argparse.ArgumentTypeError("not an integer") from None
    if number <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return number


class _Parser(argparse.ArgumentParser):
    """Bad CLI usage is a config failure (exit 1), not argparse's exit 2."""

    def error(self, message):
        print(SAFE_MARKER, file=sys.stderr)
        raise SystemExit(EXIT_INTERNAL)


def parse_args(argv):
    parser = _Parser(add_help=False)
    parser.add_argument("--config", required=True)
    parser.add_argument("--secrets-file")
    parser.add_argument("--language")
    parser.add_argument("--render", action="store_true")
    parser.add_argument("--artist")
    parser.add_argument("--title")
    parser.add_argument("--image")
    parser.add_argument("--max-bytes", type=_positive_int)
    parser.add_argument("--max-pixels", type=_positive_int)
    args = parser.parse_args(argv)
    if args.render:
        if (args.artist is None or args.title is None or args.image is not None
                or args.max_bytes is not None or args.max_pixels is not None):
            parser.error("render mode takes --artist/--title only")
    elif (args.image is None or args.max_bytes is None or args.max_pixels is None
          or args.artist is not None or args.title is not None):
        parser.error("inference mode requires --image/--max-bytes/--max-pixels")
    return args


def main(argv=None):
    for stream in (sys.stdout, sys.stderr):
        stream.reconfigure(encoding="utf-8")
    args = parse_args(argv)
    try:
        config = load_config(args.config)
        language = args.language if args.language is not None \
            else config["default_language"]
        # The selected language must be complete up front: a missing message
        # must fail explicitly, not fall back or surface empty later.
        for status, action in config["outcomes"].items():
            if action == ACTION_REJECT:
                _message(config["messages"], status, language)
        _message(config["messages"], SERVICE_MESSAGES, language)
        if args.render:
            payload = run_render(config, args.artist, args.title, language)
        else:
            payload = run_inference(config, args.image, args.max_bytes,
                                    args.max_pixels, language,
                                    args.secrets_file)
    except _ocr().OcrFailure as failure:
        print(SAFE_MARKER, file=sys.stderr)
        raise SystemExit(failure.exit_code) from None
    except ConfigError:
        print(SAFE_MARKER, file=sys.stderr)
        raise SystemExit(EXIT_INTERNAL) from None
    except Exception:
        print(SAFE_MARKER, file=sys.stderr)
        raise SystemExit(EXIT_INTERNAL) from None
    print(json.dumps(payload, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
