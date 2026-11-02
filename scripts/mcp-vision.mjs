#!/usr/bin/env node
// Generic configured MCP client for the track-vision adapter (DL-13).
//
// Reads exactly one JSON request from stdin:
//   {profileDir, server: {command, args, env, cwd}, tool, arguments,
//    resultSchema, timeoutMs}
// Connects over stdio with the official MCP SDK, performs exactly one tool
// call, parses the returned text content as JSON, validates it against the
// configured JSON Schema and prints exactly one verdict object on stdout:
//   {"status":"result","data":{...}} or {"status":"error","code":"..."}
// with a fixed generic code. Provider text, prompts, credentials and child
// stderr are never surfaced: stderr is discarded and no diagnostic logging
// happens on either stream. Client libraries resolve from the profile's
// package.json via createRequire, so release scripts work from any location.
// The total deadline covers library loading, connection and the call; the
// standard SDK request timeouts are set from the same remaining budget. The
// client closes in finally and never spawns a shell or a new process group.

import { createRequire } from "node:module";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const CLIENT_NAME = "track-vision-client";
const CLIENT_VERSION = "1.0.0";

let finished = false;

function verdict(object) {
  if (finished) {
    return;
  }
  finished = true;
  process.stdout.write(JSON.stringify(object) + "\n");
}

function validRequest(request) {
  if (!request || typeof request !== "object") return false;
  if (typeof request.profileDir !== "string" || !request.profileDir) return false;
  const server = request.server;
  if (!server || typeof server !== "object") return false;
  if (typeof server.command !== "string" || !server.command) return false;
  if (!Array.isArray(server.args)
      || !server.args.every(item => typeof item === "string")) return false;
  if (!server.env || typeof server.env !== "object"
      || !Object.values(server.env).every(v => typeof v === "string")) return false;
  if (typeof server.cwd !== "string" || !server.cwd) return false;
  if (typeof request.tool !== "string" || !request.tool) return false;
  if (!request.arguments || typeof request.arguments !== "object") return false;
  if (!request.resultSchema || typeof request.resultSchema !== "object") return false;
  if (typeof request.timeoutMs !== "number"
      || !Number.isInteger(request.timeoutMs) || request.timeoutMs <= 0) return false;
  return true;
}

function textOf(result) {
  const content = Array.isArray(result?.content) ? result.content : [];
  return content
    .filter(part => part?.type === "text" && typeof part.text === "string")
    .map(part => part.text)
    .join("\n");
}

async function main() {
  let request;
  try {
    request = JSON.parse(readFileSync(0, "utf8"));
  } catch {
    verdict({ status: "error", code: "bad_request" });
    return;
  }
  if (!validRequest(request)) {
    verdict({ status: "error", code: "bad_request" });
    return;
  }

  const deadline = Date.now() + request.timeoutMs;
  const remaining = () => Math.max(deadline - Date.now(), 1);
  const spent = () => Date.now() >= deadline;

  let client = null;
  let transport = null;
  let transportShutdown = null;
  // Exactly one transport close is ever started; the wrapped transport.close
  // is the single owner of the shutdown promise, so every caller (the SDK's
  // own internal void close(), the finally block, the watchdog) awaits the
  // same teardown. StdioClientTransport.close() drops its process reference
  // up front and a second unwrapped call would resolve before the child is
  // torn down; the raw close self-bounds via stdin EOF, SIGTERM, SIGKILL.
  const closeTransport = () => (transport ? transport.close() : null);

  // Escape hatch if an SDK promise never settles: print the verdict early,
  // then stay alive until the same shutdown promise resolves instead of
  // racing the async close with an immediate exit. The Python parent's later
  // deadline and the native outer process-group kill remain the final bounds.
  const watchdog = setTimeout(() => {
    void (async () => {
      verdict({ status: "error", code: "timeout" });
      const shutdown = closeTransport();
      if (shutdown) {
        await shutdown;
      }
      process.exit(0);
    })();
  }, request.timeoutMs + 1000);

  let phase = "load";
  try {
    const require = createRequire(pathToFileURL(join(request.profileDir, "package.json")));
    const { Client } = require("@modelcontextprotocol/sdk/client/index.js");
    const { StdioClientTransport } = require("@modelcontextprotocol/sdk/client/stdio.js");
    const ajvModule = require("ajv/dist/2020");
    const Ajv2020 = ajvModule.default ?? ajvModule;
    const validate = new Ajv2020({ strict: false, allErrors: false })
      .compile(request.resultSchema);

    transport = new StdioClientTransport({
      command: request.server.command,
      args: request.server.args,
      env: request.server.env,
      cwd: request.server.cwd,
      stderr: "ignore",
    });
    const rawTransportClose = transport.close.bind(transport);
    transport.close = () => {
      // Sole creation point of the shutdown promise.
      if (transportShutdown === null) {
        transportShutdown = Promise.resolve()
          .then(rawTransportClose)
          .then(() => {}, () => {});
      }
      return transportShutdown;
    };
    client = new Client(
      { name: CLIENT_NAME, version: CLIENT_VERSION },
      { capabilities: {} },
    );

    phase = "connect";
    await client.connect(transport, { timeout: remaining() });

    phase = "call";
    const result = await client.callTool(
      { name: request.tool, arguments: request.arguments },
      undefined,
      { timeout: remaining(), resetTimeoutOnProgress: false },
    );
    if (result && result.isError) {
      verdict({ status: "error", code: "call" });
      return;
    }
    let data;
    try {
      data = JSON.parse(textOf(result));
    } catch {
      verdict({ status: "error", code: "invalid_output" });
      return;
    }
    if (!validate(data)) {
      verdict({ status: "error", code: "invalid_output" });
      return;
    }
    verdict({ status: "result", data });
  } catch {
    const code = spent() ? "timeout"
      : phase === "connect" ? "connect"
        : phase === "call" ? "call" : "load";
    verdict({ status: "error", code });
  } finally {
    try {
      if (client) {
        await client.close();
      }
    } catch {
      // The SDK may already have closed itself on a failed connect.
    }
    // Await the SAME single-flight shutdown even if client.close() resolved
    // early; only then disengage the watchdog and let the process exit.
    const shutdown = closeTransport();
    if (shutdown) {
      await shutdown;
    }
    clearTimeout(watchdog);
  }
}

main()
  .then(() => process.exit(0))
  .catch(() => {
    verdict({ status: "error", code: "internal" });
    process.exit(0);
  });
