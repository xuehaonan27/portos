#!/usr/bin/env node
// portos-bridge-http: the event plane and the invoke path, carried over one
// HTTP socket.
//
// This is a *transport*, not a console. It knows nothing about the model
// family, chat, or any other domain — it re-exports the only two interaction
// shapes PortOS has:
//
//   topics  → GET  /events        (Server-Sent Events)
//   verbs   → POST /invoke        {verb, args} → {ok} | {err}
//
// plus the two things a presenter needs to be useful:
//
//   GET /grants                   what this bridge may invoke
//   GET /artifact/<id>            dereference a handle (the data plane —
//                                 bytes stream here, never through /events)
//   GET /                         a presenter, served as plain static bytes
//
// The presentation language lives entirely on the other end of the socket.
// `web/console.html` is one presenter; a TUI, a native app or another PortOS
// node are others, and none of them require touching this file. That split is
// the point: transport changes slowly, presentation changes fast.
//
// Authority: whatever connects here acts with **this plugin's** grants. The
// `grants` list in chat.json is therefore the whole access story — binding to
// loopback and reaching it over `ssh -L` keeps that honest.
//
// Config (environment):
//   PORTOS_BRIDGE_ADDR     default 127.0.0.1:7777
//   PORTOS_BRIDGE_TOPICS   comma-separated, default "model::session::*"

import http from "node:http";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { servePlugin } from "../../sdk/js/client.js";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ADDR = process.env.PORTOS_BRIDGE_ADDR ?? "127.0.0.1:7777";
const TOPICS = (process.env.PORTOS_BRIDGE_TOPICS ?? "model::session::*")
  .split(",")
  .map((t) => t.trim())
  .filter(Boolean);

/** Recent events, so a presenter that connects mid-turn is not blind. */
const RECENT_MAX = 200;
const recent = [];
/** Open SSE responses. */
const listeners = new Set();
/** The HTTP server, so it can be torn down when the kernel goes away. */
let server = null;

function publish(event) {
  recent.push(event);
  if (recent.length > RECENT_MAX) recent.shift();
  const frame = `data: ${JSON.stringify(event)}\n\n`;
  for (const res of listeners) {
    // A slow presenter must not stall the plugin: Node buffers, and a dead
    // socket surfaces on the next write as an error we drop it for.
    try {
      res.write(frame);
    } catch {
      listeners.delete(res);
    }
  }
}

const TYPES = {
  ".html": "text/html; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
};

function send(res, status, type, body) {
  // An explicit length keeps responses un-chunked, which every client
  // handles — including the deliberately minimal one in the tests.
  const bytes = Buffer.isBuffer(body) ? body : Buffer.from(body, "utf8");
  res.writeHead(status, {
    "content-type": type,
    "content-length": bytes.length,
    "cache-control": "no-store",
  });
  res.end(bytes);
}

function sendJson(res, status, value) {
  send(res, status, "application/json", JSON.stringify(value));
}

async function readBody(req, limit = 1024 * 1024) {
  const parts = [];
  let size = 0;
  for await (const chunk of req) {
    size += chunk.length;
    if (size > limit) throw new Error("request body too large");
    parts.push(chunk);
  }
  return Buffer.concat(parts).toString("utf8");
}

function openEventStream(res) {
  res.writeHead(200, {
    "content-type": "text/event-stream",
    "cache-control": "no-store",
    connection: "keep-alive",
  });
  // Anything already seen, so a reload does not lose the conversation.
  for (const event of recent) res.write(`data: ${JSON.stringify(event)}\n\n`);
  listeners.add(res);
  res.on("close", () => listeners.delete(res));
}

async function handle(req, res, client) {
  const url = new URL(req.url, "http://bridge");
  const route = `${req.method} ${url.pathname}`;

  if (route === "GET /events") return openEventStream(res);

  if (route === "GET /grants") {
    return sendJson(res, 200, { grants: await client.grants() });
  }

  if (route === "POST /invoke") {
    let body;
    try {
      body = JSON.parse(await readBody(req));
    } catch (e) {
      return sendJson(res, 400, { err: `bad request: ${e.message}` });
    }
    if (typeof body?.verb !== "string") {
      return sendJson(res, 400, { err: "missing verb" });
    }
    try {
      // The kernel is the one that decides whether this is allowed; a denial
      // comes back as a thrown error and is reported as-is.
      const ok = await client.invoke(body.verb, body.args ?? null);
      return sendJson(res, 200, { ok });
    } catch (e) {
      return sendJson(res, 200, { err: String(e?.message ?? e) });
    }
  }

  if (req.method === "GET" && url.pathname.startsWith("/artifact/")) {
    const id = decodeURIComponent(url.pathname.slice("/artifact/".length));
    try {
      const bytes = await client.read(id);
      const type = url.searchParams.get("type") ?? "application/octet-stream";
      return send(res, 200, type, bytes);
    } catch (e) {
      return sendJson(res, 404, { err: String(e?.message ?? e) });
    }
  }

  if (req.method === "GET") {
    const name = url.pathname === "/" ? "/console.html" : url.pathname;
    // Presenters are static bytes with no privileges; keep them inside web/.
    const file = path.join(HERE, "web", path.normalize(name));
    if (!file.startsWith(path.join(HERE, "web"))) {
      return sendJson(res, 403, { err: "outside the presenter directory" });
    }
    try {
      const body = await readFile(file);
      return send(res, 200, TYPES[path.extname(file)] ?? "application/octet-stream", body);
    } catch {
      return sendJson(res, 404, { err: "not found" });
    }
  }

  return sendJson(res, 405, { err: "method not allowed" });
}

await servePlugin({
  name: "portos-bridge-http",
  // Zero verbs: this plugin serves nobody inside PortOS. It is a consumer of
  // the event plane and a caller of the invoke path, like any renderer.
  verbs: [],
  onReady: async (client) => {
    for (const topic of TOPICS) await client.subscribe(topic);

    server = http.createServer((req, res) => {
      handle(req, res, client).catch((e) => {
        if (!res.headersSent) sendJson(res, 500, { err: String(e?.message ?? e) });
      });
    });
    // Long-lived SSE connections must not be reaped as idle.
    server.timeout = 0;
    server.keepAliveTimeout = 0;

    const sep = ADDR.lastIndexOf(":");
    const host = ADDR.slice(0, sep);
    const port = Number(ADDR.slice(sep + 1));
    await new Promise((resolve, reject) => {
      server.once("error", reject);
      server.listen(port, host, resolve);
    });
    const bound = server.address();
    console.error(
      `[bridge] http://${host}:${bound.port} — topics: ${TOPICS.join(", ")}`,
    );
    // The port the kernel actually got, for a test that asked for port 0.
    if (process.env.PORTOS_BRIDGE_PORT_FILE) {
      const { writeFile } = await import("node:fs/promises");
      await writeFile(process.env.PORTOS_BRIDGE_PORT_FILE, String(bound.port));
    }
  },
  onCall: async () => {
    throw new Error("portos-bridge-http serves no verbs");
  },
  onEvent: (topic, data) => publish({ topic, data }),
});

// `servePlugin` resolves when the kernel goes away. A listening socket would
// otherwise keep node's event loop alive forever, leaving an orphan holding
// the port — so this plugin closes its own door rather than waiting to be
// reaped.
for (const res of listeners) res.end();
server?.close();
process.exit(0);
