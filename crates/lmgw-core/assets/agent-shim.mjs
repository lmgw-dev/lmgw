// lmgw agent script shim — container-runtime design §4.2.
//
// Embedded in the gateway binary (`include_str!`) and written into the run's
// own directory beside `script.mjs`, both mounted read-only under /lmgw. It is
// the entrypoint of every `script` step:
//
//     node /lmgw/shim.mjs
//
// It reads the run's input and secrets, builds `ctx`, imports the manifest's
// script from /lmgw/script.mjs and calls the function named for the phase.
// Everything that reaches stdout goes out as the same JSONL events a container
// agent emits (§3.2), so a script and a hand-built image are indistinguishable
// downstream.
//
// **Sugar, not a sandbox.** This file strips nothing. `ctx.config` has no
// `secret` field in it because lmgw never wrote one there — `input.json` is
// `container::public_config`, and a script's `secrets.json` is
// `secrets_document(…, with_config = false)`, whose `config` is `{}` — so the
// values simply are not in the container. The **token** is, and a script runs
// with the agent's **full token authority**: it can read /lmgw/secrets.json
// itself and call `/v1` and `/mcp` with that token, inside whatever the token's
// scope and the manifest's allow list permit. What bounds a script is the
// token and the cgroup, never this file.
//
// Nothing here invents a bound: no retry count, no timeout, no page size. The
// deadline is lmgw's (`run.limits.deadline_seconds`, enforced by stopping the
// container) and the tool ceiling is the agent token's.

import { readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

const INPUT_PATH = process.env.LMGW_INPUT || "/lmgw/input.json";
const SECRETS_PATH = process.env.LMGW_SECRETS || "/lmgw/secrets.json";
const SCRIPT_PATH = process.env.LMGW_SCRIPT || "/lmgw/script.mjs";
const MCP_URL = process.env.LMGW_MCP_URL || "";
const PHASE = process.env.LMGW_PHASE || "apply";
const RUN = process.env.LMGW_RUN || "";

// The MCP versions lmgw's own ingress accepts, newest first. Sent on
// `initialize`; the gateway answers with the one it agreed to.
const PROTOCOL_VERSION = "2025-11-25";

/** One JSONL event on stdout. */
function emit(event) {
  process.stdout.write(`${JSON.stringify(event)}\n`);
}

/** One run-log line. */
function log(message) {
  emit({ type: "log", message: String(message) });
}

/**
 * Take `console` away from the script before it ever runs.
 *
 * The ledger reads *every* stdout line, so a `console.log` of an object with a
 * `type` key would forge an event — a fabricated `row`, or an `output` the
 * script never returned. Replacing the four writers with emitters closes that
 * and buys the guarantee that goes with it: **a script's console output is its
 * run log**, level and all, rather than something that may or may not survive
 * as a non-JSON line.
 */
function captureConsole() {
  const render = (arg) => {
    if (typeof arg === "string") return arg;
    if (arg instanceof Error) return arg.stack || arg.message;
    try {
      return JSON.stringify(arg);
    } catch {
      return String(arg);
    }
  };
  const at = (level) => (...args) =>
    emit({ type: "log", level, message: args.map(render).join(" ") });
  console.log = at("log");
  console.info = at("info");
  console.warn = at("warn");
  console.error = at("error");
  console.debug = at("debug");
}

function readJson(path, what) {
  let text;
  try {
    text = readFileSync(path, "utf8");
  } catch (e) {
    throw new Error(`the shim could not read ${what} at ${path}: ${e.message}`);
  }
  try {
    return JSON.parse(text);
  } catch (e) {
    throw new Error(`${what} at ${path} is not valid JSON: ${e.message}`);
  }
}

// ---------------------------------------------------------------------------
// Cancel (§4.2)
// ---------------------------------------------------------------------------
//
// SIGTERM flips a flag. The call already in flight is allowed to finish — so a
// cancel lands *between* two writes, never inside one — and every `tools.call`
// after it rejects at once with `code: "cancelled"`.
//
// Then the process **leaves on its own**, as soon as the last in-flight call
// has landed. Installing a handler suppresses node's default exit, so without
// this a script that happens to be sleeping rather than calling would sit
// there until podman's SIGKILL at the end of `stop_grace_seconds` (measured:
// 5.19 s and exit 137 before this was here). SIGKILL is the backstop, not the
// mechanism.

let cancelled = false;
let inFlight = 0;

function cancelledError() {
  const e = new Error("the run was cancelled, so no further tool call was made");
  e.code = "cancelled";
  return e;
}

/** Leave now if nothing is still writing. */
function leaveIfIdle() {
  if (!cancelled || inFlight > 0) return;
  // `exit`, deliberately: `process.stdout.write` to a pipe is synchronous on
  // Linux, so every event already handed over has been written, and there is
  // nothing left to wait for that is not the script's own dead loop.
  process.exit(1);
}

for (const signal of ["SIGTERM", "SIGINT"]) {
  process.on(signal, () => {
    if (cancelled) return;
    cancelled = true;
    log(
      `${signal} received; the ${inFlight} call(s) in flight finish, every later tools.call is ` +
        `refused (cancelled), and this process exits as soon as they land`,
    );
    leaveIfIdle();
  });
}

// ---------------------------------------------------------------------------
// tools.call — JSON-RPC `tools/call` on /mcp with the agent's own token
// ---------------------------------------------------------------------------

class McpClient {
  constructor(url, token, run) {
    this.url = url;
    this.token = token;
    this.run = run;
    this.sessionId = null;
    this.nextId = 0;
    this.opening = null;
  }

  headers() {
    const h = {
      "content-type": "application/json",
      accept: "application/json, text/event-stream",
      "mcp-protocol-version": PROTOCOL_VERSION,
    };
    if (this.token) h.authorization = `Bearer ${this.token}`;
    // Attributes the call to this run's meter and nothing else (§3.1).
    if (this.run) h["x-lmgw-run"] = String(this.run);
    if (this.sessionId) h["mcp-session-id"] = this.sessionId;
    return h;
  }

  async post(body) {
    return fetch(this.url, {
      method: "POST",
      headers: this.headers(),
      body: JSON.stringify(body),
    });
  }

  /** The MCP handshake, done once and shared by every later call. */
  async open() {
    if (this.sessionId) return;
    if (this.opening) return this.opening;
    this.opening = (async () => {
      const res = await this.post({
        jsonrpc: "2.0",
        id: ++this.nextId,
        method: "initialize",
        params: {
          protocolVersion: PROTOCOL_VERSION,
          capabilities: {},
          clientInfo: { name: "lmgw agent script", version: "1" },
        },
      });
      const text = await res.text();
      if (!res.ok) {
        throw new Error(`the gateway refused the MCP handshake (HTTP ${res.status}): ${text.trim()}`);
      }
      this.sessionId = res.headers.get("mcp-session-id");
      if (!this.sessionId) {
        throw new Error("the gateway's MCP handshake returned no Mcp-Session-Id");
      }
      // Protocol courtesy; the gateway answers 202 and needs nothing back.
      await this.post({ jsonrpc: "2.0", method: "notifications/initialized" }).catch(() => {});
    })();
    try {
      await this.opening;
    } finally {
      this.opening = null;
    }
  }

  /**
   * Hand the session back. The ingress' `SESSIONS` map has no reaper, so a run
   * that simply exits leaves one entry behind for the life of the gateway.
   * Best effort by construction: the run is over either way.
   */
  async close() {
    if (!this.sessionId) return;
    const sid = this.sessionId;
    this.sessionId = null;
    try {
      await fetch(this.url, { method: "DELETE", headers: { "mcp-session-id": sid } });
    } catch {
      /* the run is over; a leaked session is the gateway's to notice */
    }
  }

  async call(name, args) {
    if (cancelled) throw cancelledError();
    if (typeof name !== "string" || name.trim() === "") {
      throw new Error("ctx.tools.call needs the exposed tool name as its first argument");
    }
    if (!this.url) {
      throw new Error("LMGW_MCP_URL is not set, so this script has no tool plane to call");
    }
    inFlight += 1;
    try {
      await this.open();
      const res = await this.post({
        jsonrpc: "2.0",
        id: ++this.nextId,
        method: "tools/call",
        params: { name, arguments: args ?? {} },
      });
      const text = await res.text();
      if (!res.ok) {
        throw new Error(`the tool '${name}' could not be called (HTTP ${res.status}): ${text.trim()}`);
      }
      let body;
      try {
        body = JSON.parse(text);
      } catch {
        throw new Error(`the gateway's answer for '${name}' is not JSON: ${text.trim()}`);
      }
      if (body.error) {
        throw new Error(body.error.message || `the tool '${name}' failed`);
      }
      return readResult(name, body.result ?? {});
    } finally {
      inFlight -= 1;
      // A cancel that arrived mid-call: this was the write it was waiting for.
      leaveIfIdle();
    }
  }
}

/**
 * A tool result read as data: `structuredContent` when present, else the first
 * text block parsed as JSON. Prose throws — catalog §2.2 — because a script
 * that string-matches on prose is the guessing the batch executor already
 * refuses to do.
 */
function readResult(name, result) {
  const firstText = (result.content ?? []).find((b) => b && b.type === "text" && typeof b.text === "string");
  if (result.isError) {
    throw new Error(firstText ? firstText.text : `the tool '${name}' failed`);
  }
  if (result.structuredContent !== undefined && result.structuredContent !== null) {
    return result.structuredContent;
  }
  if (firstText) {
    try {
      return JSON.parse(firstText.text.trim());
    } catch {
      /* falls through to the §2.2 refusal */
    }
  }
  throw new Error(`the tool '${name}' returned text that is not JSON; a script step needs a JSON result`);
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

async function main() {
  const input = readJson(INPUT_PATH, "the run input");
  const secrets = readJson(SECRETS_PATH, "the run secrets");
  const mcp = new McpClient(MCP_URL, secrets.token, RUN || input.run?.id);

  // The token is not on `ctx` — but see the header: that is tidiness, not a
  // boundary. The script can read the secrets file, and every call it makes
  // carries the agent's own authority.
  const ctx = {
    rows: input.rows ?? [],
    config: input.config ?? {},
    agent: input.agent ?? {},
    run: input.run ?? {},
    phase: input.phase ?? PHASE,
    log,
    tools: { call: (name, args) => mcp.call(name, args) },
  };

  try {
    let mod;
    try {
      mod = await import(pathToFileURL(SCRIPT_PATH).href);
    } catch (e) {
      throw new Error(`the script at ${SCRIPT_PATH} could not be loaded: ${e.message}`);
    }
    const hook = mod[ctx.phase];
    if (typeof hook !== "function") {
      throw new Error(`the script exports no ${ctx.phase} function`);
    }
    const output = await hook(ctx);
    if (output !== undefined) {
      emit({ type: "output", output });
    }
  } finally {
    await mcp.close();
  }
}

captureConsole();

main().catch((e) => {
  process.stderr.write(`${e && e.stack ? e.stack : e}\n`);
  // `exitCode`, not `exit()`: stdout is a pipe, and exiting outright can drop
  // events the run has already been told about.
  process.exitCode = 1;
});
