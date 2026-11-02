"""Focused adapter tests for scripts/vision.py + scripts/mcp-vision.mjs (DL-13).

Runs the configured MCP client against a fake local MCP server speaking
newline-delimited JSON-RPC over stdio: no network, no real credentials and no
real model calls. Client libraries resolve from the test profile directory:
when the pinned profile dependencies are installed
(config/track-vision/node_modules) the real @modelcontextprotocol/sdk 1.26.0
and ajv 8.17.1 are used (symlinked); otherwise a minimal offline stub SDK and
schema validator stand in, and one dedicated real-library smoke case skips.
"""

import base64
import hashlib
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
VISION_SCRIPT = REPO / "scripts" / "vision.py"
PRODUCTION_PROFILE = REPO / "config" / "track-vision"
NODE = shutil.which("node")


def _python_with_pil():
    if sys.executable:
        probe = subprocess.run([sys.executable, "-c", "import PIL"],
                               capture_output=True)
        if probe.returncode == 0:
            return sys.executable
    return None


PIL_PYTHON = _python_with_pil()

SPEC = importlib.util.spec_from_file_location("vision_adapter", VISION_SCRIPT)
vision = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(vision)

REAL_LIBS = ((PRODUCTION_PROFILE / "node_modules"
              / "@modelcontextprotocol/sdk/package.json").is_file()
             and (PRODUCTION_PROFILE / "node_modules"
                  / "ajv/package.json").is_file())

# 1x1 PNG: a valid image for the guard pass-through cases.
TINY_PNG = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJ"
    "AAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==")

FIXTURE_SECRET = "fixture-secret-never-printed"

FIXTURE_SERVER = r'''
// Test-only fake MCP server (newline-delimited JSON-RPC over stdio). The
// scenario comes from FIXTURE_SCENARIO; the pid lands in FIXTURE_PIDFILE so
// tests can prove the client killed the child. check_path validates that the
// forwarded image_source exists on disk and carries a .png suffix, like the
// real server's file service does.
import { existsSync, writeFileSync } from "node:fs";

const scenario = process.env.FIXTURE_SCENARIO ?? "ok";
// Stuck servers ignore both EOF and SIGTERM: only the client's SIGKILL
// escalation can reap them, which is exactly what the cleanup cases assert.
const stuck = scenario === "stuck_initialize" || scenario === "stuck_call";
if (process.env.FIXTURE_PIDFILE) {
  writeFileSync(process.env.FIXTURE_PIDFILE, String(process.pid));
}
if (scenario === "crash") {
  process.exit(3);
}
if (scenario === "echo_env") {
  // Secret goes to stderr on purpose: the client must discard child stderr.
  console.error("server stderr saw " + (process.env.Z_AI_API_KEY ?? "nothing"));
}
if (stuck) {
  process.on("SIGTERM", () => {});
} else {
  process.on("SIGTERM", () => process.exit(0));
}

const RESULTS = {
  ok: JSON.stringify({ status: "ok", artist: "Forhill & Unfound", title: "Atlas" }),
  literal: JSON.stringify({
    status: "ok",
    artist: "--title=evil; rm -rf / $(id)",
    title: 'See {artist} "Q$`"',
  }),
  echo_env: JSON.stringify({
    status: "ok",
    artist: process.env.Z_AI_API_KEY ? "env-present" : "env-missing",
    title: "Probe",
  }),
  not_recognized: JSON.stringify({ status: "not_recognized" }),
  injection_detected: JSON.stringify({ status: "injection_detected" }),
  unknown_status: JSON.stringify({ status: "weird" }),
  extra: JSON.stringify({ status: "ok", artist: "A", title: "T", surprise: 1 }),
  malformed: "this is not json at all",
};

let buffer = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let index;
  while ((index = buffer.indexOf("\n")) >= 0) {
    const line = buffer.slice(0, index).trim();
    buffer = buffer.slice(index + 1);
    if (line) {
      handle(JSON.parse(line));
    }
  }
});
if (!stuck) {
  process.stdin.on("end", () => process.exit(0));
}

function send(message) {
  process.stdout.write(JSON.stringify(message) + "\n");
}

function handle(request) {
  if (request.id === undefined) {
    return;
  }
  if (request.method === "initialize") {
    if (scenario === "stuck_initialize") {
      return;  // never answer: the connect deadline must fire
    }
    send({
      jsonrpc: "2.0", id: request.id,
      result: {
        protocolVersion: request.params?.protocolVersion ?? "2025-06-18",
        capabilities: {},
        serverInfo: { name: "fixture-server", version: "0.0.0" },
      },
    });
    return;
  }
  if (request.method === "ping") {
    send({ jsonrpc: "2.0", id: request.id, result: {} });
    return;
  }
  if (request.method === "tools/call") {
    if (scenario === "stuck_call") {
      return;  // never answer: the call deadline must fire
    }
    if (scenario === "sleep") {
      setTimeout(() => callResult(request), 8000);
      return;
    }
    callResult(request);
    return;
  }
  send({ jsonrpc: "2.0", id: request.id,
         error: { code: -32601, message: "method not found" } });
}

function callResult(request) {
  if (scenario === "is_error") {
    send({
      jsonrpc: "2.0", id: request.id,
      result: {
        content: [{ type: "text", text: "provider failure detail http://internal" }],
        isError: true,
      },
    });
    return;
  }
  if (scenario === "echo_args") {
    send({
      jsonrpc: "2.0", id: request.id,
      result: {
        content: [{
          type: "text",
          text: JSON.stringify({
            status: "ok",
            artist: request.params?.name ?? "missing-tool",
            title: request.params?.arguments?.image_source ?? "missing-arg",
          }),
        }],
      },
    });
    return;
  }
  if (scenario === "check_path") {
    const source = request.params?.arguments?.image_source ?? "";
    const accepted = existsSync(source) && source.endsWith(".png");
    send({
      jsonrpc: "2.0", id: request.id,
      result: {
        content: [{
          type: "text",
          text: JSON.stringify({
            status: "ok",
            artist: accepted ? "path-ok" : "path-bad",
            title: "Probe",
          }),
        }],
      },
    });
    return;
  }
  if (scenario === "prompt_probe") {
    const prompt = request.params?.arguments?.prompt ?? "";
    send({
      jsonrpc: "2.0", id: request.id,
      result: {
        content: [{
          type: "text",
          text: JSON.stringify({
            status: "ok",
            artist: prompt.includes("Required JSON Schema:") ? "prompt-ok" : "prompt-bad",
            title: "Probe",
          }),
        }],
      },
    });
    return;
  }
  send({
    jsonrpc: "2.0", id: request.id,
    result: { content: [{ type: "text", text: RESULTS[scenario] ?? RESULTS.ok }] },
  });
}
'''

STUB_SDK_PACKAGE = json.dumps({
    "name": "@modelcontextprotocol/sdk",
    "version": "1.26.0-stub",
    "private": True,
    "exports": {
        ".": "./stub-client.cjs",
        "./client/index.js": "./stub-client.cjs",
        "./client/stdio.js": "./stub-transport.cjs",
    },
}, indent=1)

STUB_TRANSPORT = r'''
// Offline stand-in for the SDK stdio transport: same constructor options and
// callback surface the generic client uses; kills the child on close.
const { spawn } = require("node:child_process");

class StdioClientTransport {
  constructor(params) {
    this.params = params;
    this.child = null;
    this.onmessage = null;
    this.onclose = null;
    this.onerror = null;
    this._buffer = "";
  }

  async start() {
    const p = this.params;
    this.child = spawn(p.command, p.args ?? [], {
      cwd: p.cwd,
      env: p.env,
      stdio: ["pipe", "pipe", "ignore"],
      shell: false,
      detached: false,
    });
    this.child.stdout.setEncoding("utf8");
    this.child.stdout.on("data", (chunk) => {
      this._buffer += chunk;
      let index;
      while ((index = this._buffer.indexOf("\n")) >= 0) {
        const line = this._buffer.slice(0, index).trim();
        this._buffer = this._buffer.slice(index + 1);
        if (line && this.onmessage) {
          try {
            this.onmessage(JSON.parse(line));
          } catch (error) {
            this.onerror?.(error);
          }
        }
      }
    });
    this.child.on("error", (error) => this.onerror?.(error));
    this.child.on("exit", () => this.onclose?.());
    if (!this.child.pid) {
      await new Promise((_, reject) => this.child.once("error", reject));
    }
  }

  async send(message) {
    if (!this.child) {
      throw new Error("transport not started");
    }
    await new Promise((resolve, reject) => {
      try {
        this.child.stdin.write(JSON.stringify(message) + "\n",
                               (error) => (error ? reject(error) : resolve()));
      } catch (error) {
        reject(error);
      }
    });
  }

  async close() {
    const child = this.child;
    this.child = null;
    if (!child || child.exitCode !== null) {
      return;
    }
    // Mirror the SDK escalation (stdin EOF, SIGTERM, SIGKILL) compactly so
    // stub-backed runs exercise the same single-flight teardown semantics.
    child.stdin.end();
    const exit = new Promise((resolve) => child.once("exit", resolve));
    const grace = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
    await Promise.race([exit, grace(300)]);
    if (child.exitCode === null) {
      child.kill("SIGTERM");
      await Promise.race([exit, grace(300)]);
    }
    if (child.exitCode === null) {
      child.kill("SIGKILL");
      await Promise.race([exit, grace(300)]);
    }
  }
}

module.exports = { StdioClientTransport };
'''

STUB_CLIENT = r'''
// Offline stand-in for the SDK Client: connect/callTool/close with the same
// request-timeout semantics (timeout in options rejects the pending call).
class Client {
  constructor(clientInfo) {
    this.clientInfo = clientInfo;
    this.transport = null;
    this._nextId = 0;
    this._pending = new Map();
  }

  async connect(transport) {
    this.transport = transport;
    transport.onmessage = (message) => {
      const entry = this._pending.get(message.id);
      if (entry) {
        this._pending.delete(message.id);
        entry.resolve(message);
      }
    };
    transport.onerror = (error) => {
      for (const entry of this._pending.values()) {
        entry.reject(error);
      }
      this._pending.clear();
    };
    await transport.start();
  }

  async callTool(params, _resultSchema, options = {}) {
    const id = ++this._nextId;
    const response = new Promise((resolve, reject) => {
      this._pending.set(id, { resolve, reject });
    });
    try {
      await this.transport.send({
        jsonrpc: "2.0", id, method: "tools/call",
        params: { name: params.name, arguments: params.arguments },
      });
    } catch (error) {
      this._pending.delete(id);
      throw error;
    }
    let timer;
    try {
      return await new Promise((resolve, reject) => {
        timer = setTimeout(() => {
          this._pending.delete(id);
          reject(new Error("Request timed out"));
        }, options.timeout ?? 60000);
        response.then((message) => {
          if (message.error) {
            reject(new Error("mcp error"));
          } else {
            resolve(message.result);
          }
        }, reject);
      });
    } finally {
      clearTimeout(timer);
    }
  }

  async close() {
    if (this.transport) {
      const transport = this.transport;
      this.transport = null;
      await transport.close();
    }
  }
}

module.exports = { Client };
'''

# Minimal JSON Schema subset validator covering the constructs the configured
# result schema uses: $ref/$defs, oneOf, allOf, not, const, enum, types,
# properties/required/additionalProperties, length bounds, pattern.
STUB_AJV = r'''
function matchType(type, data) {
  switch (type) {
    case "object": return !!data && typeof data === "object" && !Array.isArray(data);
    case "array": return Array.isArray(data);
    case "string": return typeof data === "string";
    case "integer": return typeof data === "number" && Number.isInteger(data);
    case "number": return typeof data === "number";
    case "boolean": return typeof data === "boolean";
    case "null": return data === null;
    default: return false;
  }
}

function valid(schema, data, root) {
  if (schema === true) return true;
  if (schema === false) return false;
  if (!schema || typeof schema !== "object") return false;
  if (typeof schema.$ref === "string") {
    if (!schema.$ref.startsWith("#/$defs/")) return false;
    const target = root?.$defs?.[schema.$ref.slice("#/$defs/".length)];
    return target ? valid(target, data, root) : false;
  }
  if (schema.allOf && !schema.allOf.every((sub) => valid(sub, data, root))) return false;
  if (schema.not && valid(schema.not, data, root)) return false;
  if (schema.oneOf
      && schema.oneOf.filter((sub) => valid(sub, data, root)).length !== 1) return false;
  if (schema.const !== undefined && data !== schema.const) return false;
  if (schema.enum !== undefined && !schema.enum.includes(data)) return false;
  if (schema.type !== undefined) {
    const types = Array.isArray(schema.type) ? schema.type : [schema.type];
    if (!types.some((type) => matchType(type, data))) return false;
  }
  if (typeof data === "string") {
    if (schema.minLength !== undefined && data.length < schema.minLength) return false;
    if (schema.maxLength !== undefined && data.length > schema.maxLength) return false;
    if (schema.pattern !== undefined && !new RegExp(schema.pattern).test(data)) return false;
  }
  if (typeof data === "number") {
    if (schema.minimum !== undefined && data < schema.minimum) return false;
    if (schema.maximum !== undefined && data > schema.maximum) return false;
  }
  if (schema && typeof data === "object" && data !== null && !Array.isArray(data)) {
    for (const key of schema.required ?? []) {
      if (!(key in data)) return false;
    }
    if (schema.additionalProperties === false) {
      const allowed = new Set(Object.keys(schema.properties ?? {}));
      for (const key of Object.keys(data)) {
        if (!allowed.has(key)) return false;
      }
    }
    for (const [key, sub] of Object.entries(schema.properties ?? {})) {
      if (key in data && !valid(sub, data[key], root)) return false;
    }
  }
  return true;
}

class MiniAjv2020 {
  compile(schema) {
    const root = schema;
    return (data) => valid(schema, data, root);
  }
}

module.exports = { MiniAjv2020, default: MiniAjv2020 };
'''


def write_stub_libraries(root):
    sdk_dir = root / "node_modules/@modelcontextprotocol/sdk"
    sdk_dir.mkdir(parents=True)
    (sdk_dir / "package.json").write_text(STUB_SDK_PACKAGE)
    (sdk_dir / "stub-transport.cjs").write_text(STUB_TRANSPORT)
    (sdk_dir / "stub-client.cjs").write_text(STUB_CLIENT)
    ajv_dir = root / "node_modules/ajv"
    ajv_dir.mkdir(parents=True)
    (ajv_dir / "package.json").write_text(json.dumps({
        "name": "ajv", "version": "8.17.1-stub", "private": True,
        "exports": {".": "./stub-2020.cjs", "./dist/2020": "./stub-2020.cjs"},
    }))
    (ajv_dir / "stub-2020.cjs").write_text(STUB_AJV)


def write_profile(root, policy_overrides=None, server_env=None, libs="auto",
                  messages_mutator=None):
    """Materialize a complete test profile from the production files."""
    root.mkdir(parents=True, exist_ok=True)
    shutil.copy(PRODUCTION_PROFILE / "extract-track.md", root / "extract-track.md")
    shutil.copy(PRODUCTION_PROFILE / "result.schema.json", root / "result.schema.json")
    messages = json.loads((PRODUCTION_PROFILE / "messages.json").read_text("utf-8"))
    if messages_mutator:
        messages_mutator(messages)
    (root / "messages.json").write_text(
        json.dumps(messages, ensure_ascii=False), encoding="utf-8")
    policy = json.loads((PRODUCTION_PROFILE / "policy.json").read_text("utf-8"))
    policy.update(policy_overrides or {})
    (root / "policy.json").write_text(
        json.dumps(policy, ensure_ascii=False), encoding="utf-8")
    env = {"FIXTURE_SCENARIO": "ok"}
    env.update(server_env or {})
    (root / "mcp.json").write_text(json.dumps({"mcpServers": {"track-vision": {
        "command": "node", "args": ["fixture-server.mjs"], "env": env}}}))
    (root / "fixture-server.mjs").write_text(FIXTURE_SERVER, encoding="utf-8")
    (root / "package.json").write_text(json.dumps({
        "name": "track-vision-test-profile", "private": True}))
    link = root / "node_modules"
    if libs == "real" or (libs == "auto" and REAL_LIBS):
        if not link.exists():
            os.symlink(PRODUCTION_PROFILE / "node_modules", link)
    elif not link.exists():
        write_stub_libraries(root)
    return root


def write_secrets(root, values):
    path = root / "secrets.json"
    path.write_text(json.dumps(values), encoding="utf-8")
    return str(path)


def run_vision(args, python=None, env=None, timeout=60):
    command = [python or sys.executable, "-I", str(VISION_SCRIPT), *args]
    merged = {k: v for k, v in os.environ.items() if not k.startswith("Z_AI")}
    merged.update(env or {})
    return subprocess.run(command, capture_output=True, env=merged, timeout=timeout)


def make_webp(path):
    subprocess.run(
        [PIL_PYTHON, "-c",
         f"from PIL import Image; Image.new('RGB', (8, 6), 'white').save({str(path)!r}, 'WEBP')"],
        check=True, capture_output=True)


def make_noise_png(path, width=400, height=300):
    code = (f"import os; from PIL import Image; "
            f"Image.frombytes('RGB', ({width}, {height}), "
            f"os.urandom({width * height * 3})).save({str(path)!r}, 'PNG')")
    subprocess.run([PIL_PYTHON, "-c", code], check=True, capture_output=True)


def out_json(completed):
    text = completed.stdout.decode("utf-8")
    lines = [line for line in text.splitlines() if line.strip()]
    if len(lines) != 1:
        raise AssertionError(f"expected exactly one stdout object, got: {text!r}")
    return json.loads(lines[0])


class SubstituteTest(unittest.TestCase):
    def test_only_braced_placeholders_expand(self):
        template = "artist {artist} title {title} artist"
        self.assertEqual(vision._substitute(template, {"artist": "A", "title": "T"}),
                         "artist A title T artist")

    def test_inserted_values_are_never_rescanned(self):
        self.assertEqual(vision._substitute("{artist}", {"artist": "{title}"}),
                         "{title}")

    def test_unknown_placeholders_stay_literal(self):
        self.assertEqual(vision._substitute("{nope} {artist}", {"artist": "A"}),
                         "{nope} A")


class ConfigValidationTest(unittest.TestCase):
    """Configuration failures exit 1 with only the fixed marker."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def _render_args(self, **kwargs):
        profile = write_profile(self.root / "profile", **kwargs)
        return ["--config", str(profile / "policy.json"), "--render",
                "--artist=A", "--title=T"]

    def assert_config_failure(self, args):
        completed = run_vision(args)
        self.assertEqual(completed.returncode, 1)
        self.assertEqual(completed.stderr.decode("utf-8").strip(),
                         vision.SAFE_MARKER)
        self.assertEqual(completed.stdout, b"")

    def test_production_profile_loads(self):
        config = vision.load_config(str(PRODUCTION_PROFILE / "policy.json"))
        self.assertEqual(config["preview_status"], "ok")
        self.assertEqual(config["tool"], "analyze_image")
        self.assertEqual(config["total_timeout_ms"], 55000)

    def test_unknown_action_rejected(self):
        args = self._render_args(policy_overrides={
            "outcomes": {"ok": "preview_artist_and_title",
                         "not_recognized": "do_something"}})
        self.assert_config_failure(args)

    def test_duplicate_preview_mapping_rejected(self):
        args = self._render_args(policy_overrides={
            "outcomes": {"ok": "preview_artist_and_title",
                         "also_ok": "preview_artist_and_title",
                         "not_recognized": "reject"}})
        self.assert_config_failure(args)

    def test_missing_preview_mapping_rejected(self):
        args = self._render_args(policy_overrides={
            "outcomes": {"not_recognized": "reject"}})
        self.assert_config_failure(args)

    def test_missing_rejection_message_rejected(self):
        def drop(messages):
            del messages["not_recognized"]
        self.assert_config_failure(self._render_args(messages_mutator=drop))

    def test_unconfigured_language_rejected(self):
        args = self._render_args() + ["--language", "de"]
        self.assert_config_failure(args)

    def test_missing_quoted_template_rejected(self):
        args = self._render_args(policy_overrides={
            "search_queries": {"plain": "{artist} {title}"}})
        self.assert_config_failure(args)

    def test_nonpositive_bound_rejected(self):
        self.assert_config_failure(
            self._render_args(policy_overrides={"maximum_query_characters": 0}))

    def test_noninteger_timeout_rejected(self):
        self.assert_config_failure(
            self._render_args(policy_overrides={"total_timeout_ms": "55000"}))

    def test_missing_server_entry_rejected(self):
        self.assert_config_failure(
            self._render_args(policy_overrides={"server": "nope"}))

    def test_render_with_image_arguments_rejected(self):
        self.assert_config_failure(
            self._render_args() + ["--image", "/abs/x.png",
                                   "--max-bytes", "10", "--max-pixels", "10"])

    def test_unsupported_image_input_format_rejected(self):
        args = self._render_args(policy_overrides={
            "image_input": {"format": "WEBP", "maximum_bytes": 5242880}})
        self.assert_config_failure(args)

    def test_nonpositive_image_cap_rejected(self):
        args = self._render_args(policy_overrides={
            "image_input": {"format": "PNG", "maximum_bytes": 0}})
        self.assert_config_failure(args)

    def test_inference_without_limits_rejected(self):
        profile = write_profile(self.root / "profile")
        self.assert_config_failure(
            ["--config", str(profile / "policy.json"),
             "--image", "/abs/x.png"])


class RenderTest(unittest.TestCase):
    """Render-only pass: literal values, no image guard, no MCP client."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.profile = write_profile(self.root / "profile")
        self.policy = str(self.profile / "policy.json")
        self.pidfile = self.root / "server.pid"
        # The fixture server records its pid if it ever starts.
        (self.profile / "mcp.json").write_text(json.dumps({"mcpServers": {
            "track-vision": {"command": "node", "args": ["fixture-server.mjs"],
                             "env": {"FIXTURE_SCENARIO": "ok",
                                     "FIXTURE_PIDFILE": str(self.pidfile)}}}}))

    def _render(self, artist, title, extra=()):
        argv = ["--config", self.policy, "--render",
                f"--artist={artist}", f"--title={title}", *extra]
        completed = run_vision(argv)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(completed.stderr, b"")
        return out_json(completed)

    def test_never_starts_the_mcp_client(self):
        payload = self._render("Forhill & Unfound", "Atlas")
        self.assertEqual(payload["kind"], "track")
        self.assertFalse(self.pidfile.exists())

    def test_values_preserved_literally(self):
        artist = "Hotel  Pools"  # deliberate double space stays
        payload = self._render(artist, "A&B")
        self.assertEqual(payload["artist"], artist)
        self.assertEqual(payload["title"], "A&B")
        self.assertEqual(payload["plain_query"], "Hotel  Pools A&B")
        self.assertEqual(payload["quoted_query"], '"Hotel  Pools" "A&B"')

    def test_metacharacters_and_unicode_pass_literally(self):
        artist = "--title=evil; rm -rf / $(id) `echo hi`"
        title = 'See {artist} "Q$`"'
        payload = self._render(artist, title)
        self.assertEqual(payload["artist"], artist)
        self.assertEqual(payload["title"], title)
        # {artist} inside the title is data, never re-expanded.
        self.assertIn("See {artist}", payload["plain_query"])
        self.assertNotIn("See --title=evil", payload["plain_query"])
        self.assertEqual(payload["quoted_query"], f'"{artist}" "{title}"')

    def test_combined_bound_uses_artist_plus_title_not_template(self):
        payload = self._render("A" * 238, "T")
        self.assertEqual(payload["kind"], "track")
        # 240-char combined bound accepts a longer rendered quoted query.
        self.assertEqual(len(payload["quoted_query"]), 244)
        rejected = self._render("A" * 240, "T")
        self.assertEqual(rejected["kind"], "rejected")
        self.assertIn("unavailable", rejected["message"])

    def test_blank_values_rejected(self):
        for artist, title in (("", "T"), ("   ", "T"), ("A", ""), ("A", "\t")):
            payload = self._render(artist, title)
            self.assertEqual(payload["kind"], "rejected", (artist, title))

    def test_control_characters_rejected_not_cleaned(self):
        payload = self._render("A\u0001B", "T")
        self.assertEqual(payload["kind"], "rejected")

    def test_secrets_file_is_not_read_in_render_mode(self):
        payload = self._render("Forhill & Unfound", "Atlas",
                               extra=["--secrets-file", "/nonexistent/secrets.json"])
        self.assertEqual(payload["kind"], "track")


@unittest.skipUnless(NODE and PIL_PYTHON, "node or Pillow interpreter missing")
class InferenceTest(unittest.TestCase):
    """Full client path against the fake local MCP server/SDK fixture."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.image = self.root / "card.png"
        self.image.write_bytes(TINY_PNG)
        self.pidfile = self.root / "server.pid"
        self.libs = "auto"

    def _profile(self, scenario, policy_overrides=None, pidfile=False):
        server_env = {"FIXTURE_SCENARIO": scenario}
        if pidfile:
            server_env["FIXTURE_PIDFILE"] = str(self.pidfile)
        return write_profile(self.root / f"profile-{scenario}",
                             policy_overrides=policy_overrides,
                             server_env=server_env, libs=self.libs)

    def _run(self, scenario, extra=(), env=None, secrets=None,
             policy_overrides=None, pidfile=False):
        profile = self._profile(scenario, policy_overrides, pidfile)
        argv = ["--config", str(profile / "policy.json"),
                "--image", str(self.image),
                "--max-bytes", "10485760", "--max-pixels", "16000000", *extra]
        if secrets is not None:
            argv += ["--secrets-file", secrets]
        return run_vision(argv, env=env, python=PIL_PYTHON)

    def _env(self):
        return {"Z_AI_API_KEY": FIXTURE_SECRET}

    def _secrets(self):
        return write_secrets(self.root, {"Z_AI_API_KEY": FIXTURE_SECRET,
                                         "UNRELATED_SECRET": "stays-in-file"})

    def assert_track(self, completed, artist, title):
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(completed.stderr, b"")
        payload = out_json(completed)
        self.assertEqual(payload, {
            "kind": "track", "artist": artist, "title": title,
            "plain_query": f"{artist} {title}",
            "quoted_query": f'"{artist}" "{title}"',
        })
        return payload

    def assert_service_rejection(self, completed):
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(completed.stderr, b"")
        payload = out_json(completed)
        messages = json.loads(
            (PRODUCTION_PROFILE / "messages.json").read_text("utf-8"))
        self.assertEqual(payload, {"kind": "rejected",
                                   "message": messages["provider_error"]["en"]})
        return payload

    def test_valid_track_with_secrets_file(self):
        completed = self._run("ok", secrets=self._secrets())
        self.assert_track(completed, "Forhill & Unfound", "Atlas")

    def test_valid_track_with_environment_credentials(self):
        completed = self._run("ok", env=self._env())
        self.assert_track(completed, "Forhill & Unfound", "Atlas")

    def test_rejection_not_recognized_english(self):
        completed = self._run("not_recognized", env=self._env())
        self.assertEqual(completed.returncode, 0)
        messages = json.loads(
            (PRODUCTION_PROFILE / "messages.json").read_text("utf-8"))
        self.assertEqual(out_json(completed), {
            "kind": "rejected",
            "message": messages["not_recognized"]["en"]})

    def test_rejection_injection_detected_localized(self):
        completed = self._run("injection_detected", env=self._env(),
                              extra=["--language", "ru"])
        messages = json.loads(
            (PRODUCTION_PROFILE / "messages.json").read_text("utf-8"))
        self.assertEqual(out_json(completed), {
            "kind": "rejected",
            "message": messages["injection_detected"]["ru"]})

    def test_unknown_status_is_service_failure(self):
        self.assert_service_rejection(self._run("unknown_status", env=self._env()))

    def test_malformed_output_is_service_failure(self):
        self.assert_service_rejection(self._run("malformed", env=self._env()))

    def test_extra_fields_are_service_failure(self):
        self.assert_service_rejection(self._run("extra", env=self._env()))

    def test_tool_error_is_service_failure_without_provider_text(self):
        completed = self._run("is_error", env=self._env())
        payload = self.assert_service_rejection(completed)
        self.assertNotIn("provider failure detail", payload["message"])
        self.assertNotIn(b"provider failure detail", completed.stderr)

    def test_server_crash_is_service_failure(self):
        self.assert_service_rejection(self._run("crash", env=self._env()))

    def test_configured_tool_and_arguments_forwarded(self):
        completed = self._run("echo_args", env=self._env())
        self.assertEqual(completed.returncode, 0, completed.stderr)
        payload = out_json(completed)
        self.assertEqual(payload["kind"], "track")
        self.assertEqual(payload["artist"], "analyze_image")
        # The fake server echoes the normalized image_source back as title.
        forwarded = Path(payload["title"])
        self.assertEqual(forwarded.suffix, ".png")
        self.assertNotEqual(str(forwarded), str(self.image))
        self.assertEqual(forwarded.parent, self.image.parent)
        self.assertEqual(list(self.root.glob(".vision-*.png")), [])

    def test_suffixless_source_normalized_and_cleaned_up(self):
        source = self.root / "input-image"  # native scratch name: no suffix
        source.write_bytes(TINY_PNG)
        original = hashlib.sha256(source.read_bytes()).hexdigest()
        profile = self._profile("check_path", pidfile=True)
        completed = run_vision(
            ["--config", str(profile / "policy.json"), "--image", str(source),
             "--max-bytes", "10485760", "--max-pixels", "16000000",
             "--secrets-file", self._secrets()], python=PIL_PYTHON)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        payload = out_json(completed)
        self.assertEqual(payload["kind"], "track")
        # The server only accepts an existing .png path.
        self.assertEqual(payload["artist"], "path-ok")
        self.assertEqual(list(self.root.glob(".vision-*.png")), [])
        self.assertEqual(hashlib.sha256(source.read_bytes()).hexdigest(),
                         original)

    def test_webp_original_normalized_to_png(self):
        source = self.root / "card.webp"
        make_webp(source)
        original = hashlib.sha256(source.read_bytes()).hexdigest()
        profile = self._profile("check_path", pidfile=True)
        completed = run_vision(
            ["--config", str(profile / "policy.json"), "--image", str(source),
             "--max-bytes", "10485760", "--max-pixels", "16000000",
             "--secrets-file", self._secrets()], python=PIL_PYTHON)
        payload = out_json(completed)
        self.assertEqual(payload["kind"], "track")
        self.assertEqual(payload["artist"], "path-ok")
        self.assertEqual(list(self.root.glob(".vision-*.png")), [])
        self.assertEqual(hashlib.sha256(source.read_bytes()).hexdigest(),
                         original)

    def test_cmyk_jpeg_normalized_to_png(self):
        source = self.root / 'input-image'
        subprocess.run([PIL_PYTHON, '-c',
                        "from PIL import Image; import sys; Image.new('CMYK',(8,6)).save(sys.argv[1],'JPEG')",
                        str(source)], check=True, capture_output=True)
        self.image = source
        original = source.read_bytes()
        completed = self._run('check_path', env=self._env())
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(out_json(completed)['artist'], 'path-ok')
        self.assertEqual(source.read_bytes(), original)
        self.assertEqual(list(self.root.glob('.vision-*.png')), [])

    def test_converted_oversize_rejected_before_mcp(self):
        source = self.root / "input-image"
        make_noise_png(source)  # incompressible: re-encode stays above the cap
        profile = self._profile(
            "check_path", pidfile=True,
            policy_overrides={"image_input": {"format": "PNG",
                                              "maximum_bytes": 10000}})
        completed = run_vision(
            ["--config", str(profile / "policy.json"), "--image", str(source),
             "--max-bytes", "10485760", "--max-pixels", "16000000",
             "--secrets-file", self._secrets()], python=PIL_PYTHON)
        self.assertEqual(completed.returncode, 4)
        self.assertEqual(completed.stderr.decode("utf-8").strip(),
                         vision.SAFE_MARKER)
        self.assertEqual(completed.stdout, b"")
        self.assertFalse(self.pidfile.exists())
        self.assertEqual(list(self.root.glob(".vision-*.png")), [])

    def test_prompt_template_substituted(self):
        completed = self._run("prompt_probe", env=self._env())
        self.assert_track(completed, "prompt-ok", "Probe")

    def test_literal_model_data_passes_through(self):
        completed = self._run("literal", env=self._env())
        artist = "--title=evil; rm -rf / $(id)"
        title = 'See {artist} "Q$`"'
        payload = self.assert_track(completed, artist, title)
        self.assertNotIn("See --title=evil", payload["plain_query"])

    def test_secret_reaches_child_but_never_output(self):
        completed = self._run("echo_env", secrets=self._secrets(), pidfile=True)
        self.assert_track(completed, "env-present", "Probe")
        self.assertNotIn(FIXTURE_SECRET.encode(),
                         completed.stdout + completed.stderr)
        self.assertNotIn(b"server stderr saw", completed.stdout + completed.stderr)

    def test_environment_credentials_reach_child(self):
        completed = self._run("echo_env", env=self._env())
        self.assert_track(completed, "env-present", "Probe")
        self.assertNotIn(FIXTURE_SECRET.encode(),
                         completed.stdout + completed.stderr)

    def assert_pid_dead(self, pid, what):
        for _ in range(40):
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                return
            time.sleep(0.1)
        self.fail(f"{what} {pid} survived the client shutdown")

    def test_deadline_enforced_and_child_cleaned_up(self):
        started = time.monotonic()
        completed = self._run("sleep", env=self._env(), pidfile=True,
                              policy_overrides={"total_timeout_ms": 1500})
        elapsed = time.monotonic() - started
        self.assert_service_rejection(completed)
        # The fixture server would answer after 8s; the configured deadline
        # must cut it off well before that.
        self.assertLess(elapsed, 8.0, f"deadline not enforced: {elapsed:.1f}s")
        self.assertTrue(self.pidfile.exists())
        self.assert_pid_dead(int(self.pidfile.read_text("utf-8")),
                             "server child")

    def test_stuck_initialize_deadline_leaves_no_descendant(self):
        # Server ignores EOF and SIGTERM before answering initialize: only
        # the client's SIGKILL escalation can reap it.
        started = time.monotonic()
        completed = self._run("stuck_initialize", env=self._env(), pidfile=True,
                              policy_overrides={"total_timeout_ms": 1500})
        elapsed = time.monotonic() - started
        self.assert_service_rejection(completed)
        self.assertLess(elapsed, 8.0, f"connect deadline not enforced: {elapsed:.1f}s")
        self.assertTrue(self.pidfile.exists())
        self.assert_pid_dead(int(self.pidfile.read_text("utf-8")),
                             "stuck initialize server")

    def test_stuck_call_deadline_leaves_no_descendant(self):
        started = time.monotonic()
        completed = self._run("stuck_call", env=self._env(), pidfile=True,
                              policy_overrides={"total_timeout_ms": 1500})
        elapsed = time.monotonic() - started
        self.assert_service_rejection(completed)
        self.assertLess(elapsed, 8.0, f"call deadline not enforced: {elapsed:.1f}s")
        self.assertTrue(self.pidfile.exists())
        self.assert_pid_dead(int(self.pidfile.read_text("utf-8")),
                             "stuck call server")

    def test_missing_required_environment_fails_explicitly(self):
        # Scrubbed environment (empty Z_AI_API_KEY) and no secrets file.
        completed = self._run("ok", env={"Z_AI_API_KEY": ""})
        self.assertEqual(completed.returncode, 1)
        self.assertEqual(completed.stderr.decode("utf-8").strip(),
                         vision.SAFE_MARKER)
        self.assertEqual(completed.stdout, b"")

    def test_configured_secrets_file_missing_fails_explicitly(self):
        completed = self._run("ok", secrets="/nonexistent/secrets.json")
        self.assertEqual(completed.returncode, 1)
        self.assertEqual(completed.stderr.decode("utf-8").strip(),
                         vision.SAFE_MARKER)

    def test_invalid_image_rejected_before_any_mcp_work(self):
        bad = self.root / "garbage.png"
        bad.write_bytes(b"not an image at all")
        profile = self._profile("ok", pidfile=True)
        completed = run_vision(
            ["--config", str(profile / "policy.json"), "--image", str(bad),
             "--max-bytes", "10485760", "--max-pixels", "16000000",
             "--secrets-file", self._secrets()], python=PIL_PYTHON)
        self.assertEqual(completed.returncode, 3)
        self.assertEqual(completed.stderr.decode("utf-8").strip(),
                         vision.SAFE_MARKER)
        self.assertEqual(completed.stdout, b"")
        self.assertFalse(self.pidfile.exists())

    def test_oversized_bytes_rejected_before_any_mcp_work(self):
        big = self.root / "big.png"
        big.write_bytes(TINY_PNG + b"\x00" * 64)
        profile = self._profile("ok", pidfile=True)
        completed = run_vision(
            ["--config", str(profile / "policy.json"), "--image", str(big),
             "--max-bytes", "24", "--max-pixels", "16000000"], python=PIL_PYTHON)
        self.assertEqual(completed.returncode, 4)
        self.assertFalse(self.pidfile.exists())


@unittest.skipUnless(NODE and REAL_LIBS and PIL_PYTHON,
                     "pinned profile libraries or Pillow interpreter missing")
class RealLibrarySmokeTest(unittest.TestCase):
    """The same fixture against the real pinned SDK and ajv.

    Proves the client's createRequire import paths, the SDK exports map and
    ajv's dist/2020 build work with the production profile dependencies,
    including the single-flight transport teardown on a stuck server.
    """

    def _run(self, scenario, policy_overrides=None):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        image = root / "card.png"
        image.write_bytes(TINY_PNG)
        pidfile = root / "server.pid"
        server_env = {"FIXTURE_SCENARIO": scenario,
                      "FIXTURE_PIDFILE": str(pidfile)}
        profile = write_profile(root / "profile", libs="real",
                                policy_overrides=policy_overrides,
                                server_env=server_env)
        started = time.monotonic()
        completed = run_vision(
            ["--config", str(profile / "policy.json"),
             "--image", str(image),
             "--max-bytes", "10485760", "--max-pixels", "16000000",
             "--secrets-file",
             write_secrets(profile, {"Z_AI_API_KEY": FIXTURE_SECRET})],
            python=PIL_PYTHON)
        return completed, pidfile, time.monotonic() - started

    def test_real_sdk_and_ajv_end_to_end(self):
        completed, _, _ = self._run("ok")
        self.assertEqual(completed.returncode, 0, completed.stderr)
        payload = out_json(completed)
        self.assertEqual(payload["kind"], "track")
        self.assertEqual(payload["artist"], "Forhill & Unfound")
        self.assertEqual(payload["plain_query"], "Forhill & Unfound Atlas")

    def test_real_sdk_deadline_and_cleanup(self):
        completed, pidfile, elapsed = self._run(
            "sleep", policy_overrides={"total_timeout_ms": 1500})
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(out_json(completed)["kind"], "rejected")
        self.assertLess(elapsed, 8.0)
        pid = int(pidfile.read_text("utf-8"))
        for _ in range(40):
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                break
            time.sleep(0.1)
        else:
            self.fail(f"server child {pid} survived real SDK shutdown")

    def test_real_sdk_stuck_initialize_is_sigkilled(self):
        # Ignores EOF and SIGTERM during initialize; the real transport's
        # SIGKILL escalation (stdin EOF -> SIGTERM -> SIGKILL) must reap it
        # before the Python deadline kills the client.
        completed, pidfile, elapsed = self._run(
            "stuck_initialize", policy_overrides={"total_timeout_ms": 1500})
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(out_json(completed)["kind"], "rejected")
        self.assertLess(elapsed, 8.0)
        pid = int(pidfile.read_text("utf-8"))
        for _ in range(40):
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                break
            time.sleep(0.1)
        else:
            self.fail(f"stuck server {pid} survived real SDK SIGKILL escalation")


if __name__ == "__main__":
    unittest.main()
