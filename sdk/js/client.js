// PortOS plugin protocol client — ABI v2, JS side. Zero dependencies.
//
// Mirrors sdk/rust: a plugin connects to $PORTOS_PLUGIN_SOCK twice
// (roles "serve" and "client", both authenticated by $PORTOS_PLUGIN_TOKEN).
// Frames are 4-byte LE length + JSON; payloads ride after a frame as raw
// chunks (4-byte LE length + bytes, zero-length terminator) and never inside
// JSON (decisions-v1.md D25 — this is why no native addon is needed).
//
// The shared JS SDK: extracted here (D30's own trigger) when the renderer
// became the second JS driver alongside the browser.

import net from "node:net";
import { readFile } from "node:fs/promises";

export const ABI_VERSION = "0.2";
const MAX_FRAME = 8 * 1024 * 1024;
const CHUNK_MAX = 4 * 1024 * 1024;
const CHUNK_SIZE = 1024 * 1024;

/** Buffered reader + writer over one UDS connection. */
class Channel {
  constructor(socket) {
    this.socket = socket;
    this.bufs = [];
    this.buffered = 0;
    this.waiter = null; // { n, resolve, reject }
    this.err = null;
    socket.on("data", (d) => {
      this.bufs.push(d);
      this.buffered += d.length;
      this._pump();
    });
    socket.on("error", (e) => this._fail(e));
    socket.on("close", () => this._fail(new Error("kernel closed the channel")));
  }

  _pump() {
    const w = this.waiter;
    if (w && this.buffered >= w.n) {
      this.waiter = null;
      w.resolve(this._take(w.n));
    }
  }

  _take(n) {
    const all = Buffer.concat(this.bufs, this.buffered);
    this.bufs = n < all.length ? [all.subarray(n)] : [];
    this.buffered = all.length - n;
    return all.subarray(0, n);
  }

  _fail(e) {
    this.err = e;
    if (this.waiter) {
      const w = this.waiter;
      this.waiter = null;
      w.reject(e);
    }
  }

  readExact(n) {
    if (this.buffered >= n) return Promise.resolve(this._take(n));
    if (this.err) return Promise.reject(this.err);
    return new Promise((resolve, reject) => {
      this.waiter = { n, resolve, reject };
    });
  }

  async readFrame() {
    const len = (await this.readExact(4)).readUInt32LE(0);
    if (len > MAX_FRAME) throw new Error(`frame too large: ${len}`);
    return JSON.parse((await this.readExact(len)).toString("utf8"));
  }

  writeFrame(obj) {
    const body = Buffer.from(JSON.stringify(obj), "utf8");
    if (body.length > MAX_FRAME) throw new Error(`frame too large: ${body.length}`);
    const head = Buffer.alloc(4);
    head.writeUInt32LE(body.length, 0);
    this.socket.write(Buffer.concat([head, body]));
  }

  /** Read a chunk stream to its terminator, returning the payload. */
  async readChunks() {
    const parts = [];
    for (;;) {
      const len = (await this.readExact(4)).readUInt32LE(0);
      if (len === 0) break;
      if (len > CHUNK_MAX) throw new Error(`chunk too large: ${len}`);
      parts.push(await this.readExact(len));
    }
    return Buffer.concat(parts);
  }

  /** Write a payload as a chunk stream, terminator included. */
  writeChunks(buf) {
    for (let o = 0; o < buf.length; o += CHUNK_SIZE) {
      const c = buf.subarray(o, Math.min(o + CHUNK_SIZE, buf.length));
      const head = Buffer.alloc(4);
      head.writeUInt32LE(c.length, 0);
      this.socket.write(head);
      this.socket.write(c);
    }
    this.socket.write(Buffer.alloc(4)); // zero-length terminator
  }
}

function unwrapOk(resp) {
  if (resp && typeof resp.err === "string") throw new Error(resp.err);
  return resp?.ok ?? null;
}

async function connectChannel(sockPath, hello) {
  const socket = await new Promise((resolve, reject) => {
    const s = net.createConnection(sockPath, () => resolve(s));
    s.on("error", reject);
  });
  const chan = new Channel(socket);
  chan.writeFrame(hello);
  unwrapOk(await chan.readFrame());
  return chan;
}

/**
 * The plugin's connection to the kernel (client channel). Operations are
 * serialized: each holds the channel for one request/response, chunk streams
 * included.
 */
export class KernelClient {
  constructor(chan) {
    this.chan = chan;
    this.q = Promise.resolve();
  }

  _serial(fn) {
    const run = this.q.then(fn);
    this.q = run.catch(() => {});
    return run;
  }

  /** Call another plugin's verb through the kernel (capability-checked
   *  there). `at` names the instance when more than one answers the verb. */
  invoke(verb, args = null, at) {
    return this._serial(async () => {
      const req = { op: "invoke", verb, args };
      if (at) req.at = at;
      this.chan.writeFrame(req);
      return unwrapOk(await this.chan.readFrame());
    });
  }

  /** Publish an event; resolves to the number of subscribers reached. */
  emit(topic, data = null) {
    return this._serial(async () => {
      this.chan.writeFrame({ op: "emit", topic, data });
      return unwrapOk(await this.chan.readFrame()).delivered ?? 0;
    });
  }

  /** Where an artifact's bytes are on disk, for handing to something that
   *  only speaks in paths. The file is read-only. Prefer this over `read`
   *  whenever the consumer is a program rather than this process. */
  locate(id) {
    return this._serial(async () => {
      this.chan.writeFrame({ op: "locate", id });
      return unwrapOk(await this.chan.readFrame()).path;
    });
  }

  /** Live grants for this plugin, joined with the target verbs' advertised
   *  metadata: [{verb, description, schema, counts_left?}]. */
  grants() {
    return this._serial(async () => {
      this.chan.writeFrame({ op: "grants" });
      return unwrapOk(await this.chan.readFrame()).grants ?? [];
    });
  }

  /** Ingest a Buffer into the kernel CAS; resolves to the ArtifactMeta. */
  put(buf, type = "application/octet-stream", labels = null) {
    return this._serial(async () => {
      this.chan.writeFrame({ op: "put", type, labels });
      this.chan.writeChunks(buf);
      return unwrapOk(await this.chan.readFrame()).meta;
    });
  }

  /** Dereference (a range of) an artifact; resolves to a Buffer. */
  read(id, { offset = 0, len } = {}) {
    return this._serial(async () => {
      const req = { op: "read", id, offset };
      if (len !== undefined) req.len = len;
      this.chan.writeFrame(req);
      unwrapOk(await this.chan.readFrame());
      return this.chan.readChunks();
    });
  }
}

/**
 * A driver interface, as data: `drivers/<name>/driver.json`. The same
 * document the Rust interface crate reads, so an implementation here and a
 * caller there cannot disagree about what a verb is.
 */
export async function loadDriver(file) {
  const doc = JSON.parse(await readFile(file, "utf8"));
  if (typeof doc.driver !== "string" || typeof doc.verbs !== "object") {
    throw new Error(`${file}: not a driver document`);
  }
  return doc;
}

/**
 * Hold a value to a JSON Schema — the subset the drivers write: type,
 * properties, required, additionalProperties, items, prefixItems, min/max
 * items, enum, anyOf/oneOf. Returns the first problem as a string, or null.
 * This is the JS side of what serde does for a Rust plugin: a handler never
 * sees arguments the interface did not describe, and never answers in a
 * shape it did not name.
 */
export function check(value, schema, at = "$") {
  if (!schema || typeof schema !== "object") return null;
  const kind = (v) =>
    v === null ? "null" : Array.isArray(v) ? "array" : Number.isInteger(v) ? "integer" : typeof v;
  if (schema.type !== undefined) {
    const want = Array.isArray(schema.type) ? schema.type : [schema.type];
    const k = kind(value);
    const ok = want.some((t) => t === k || (t === "number" && k === "integer"));
    if (!ok) return `${at}: expected ${want.join("|")}, got ${k}`;
  }
  if (schema.enum && !schema.enum.some((e) => e === value)) {
    return `${at}: expected one of ${JSON.stringify(schema.enum)}`;
  }
  const alts = schema.anyOf ?? schema.oneOf;
  if (alts) {
    const problems = alts.map((s) => check(value, s, at));
    if (!problems.some((p) => p === null)) return problems.join("; or ");
  }
  if (value && typeof value === "object" && !Array.isArray(value)) {
    for (const key of schema.required ?? []) {
      if (!(key in value)) return `${at}: missing ${key}`;
    }
    for (const [key, v] of Object.entries(value)) {
      const sub = schema.properties?.[key];
      if (sub) {
        const p = check(v, sub, `${at}.${key}`);
        if (p) return p;
      } else if (schema.additionalProperties === false) {
        return `${at}: unexpected ${key}`;
      } else if (typeof schema.additionalProperties === "object") {
        const p = check(v, schema.additionalProperties, `${at}.${key}`);
        if (p) return p;
      }
    }
  }
  if (Array.isArray(value)) {
    if (schema.minItems !== undefined && value.length < schema.minItems) {
      return `${at}: expected at least ${schema.minItems} items`;
    }
    if (schema.maxItems !== undefined && value.length > schema.maxItems) {
      return `${at}: expected at most ${schema.maxItems} items`;
    }
    for (let i = 0; i < value.length; i++) {
      const sub = schema.prefixItems?.[i] ?? schema.items;
      if (sub) {
        const p = check(value[i], sub, `${at}[${i}]`);
        if (p) return p;
      }
    }
  }
  return null;
}

// Where the line between context and data is drawn: the constants of
// `portos_abi::bulk`, because the model was told one shape.
const INLINE_MAX = 16 * 1024;
const PREVIEW_CHARS = 2048;
const ARGS_LABEL_CHARS = 256;

function takeChars(s, n) {
  let out = "";
  let i = 0;
  for (const ch of s) {
    if (i++ >= n) break;
    out += ch;
  }
  return out;
}

/**
 * Apply a verb's bulk declaration to its reply: a value that is exactly
 * `{text}` is text and its bytes are the text; anything else is a document
 * and its bytes are its JSON. Over the line, it is stored under the driver's
 * content type with the verb and its arguments as provenance, and
 * `{handle, size, preview}` is left behind. The JS twin of
 * `portos_sdk::bulk`.
 */
async function spill(client, spec, verb, args, reply) {
  const provenance = { integ: [verb, `args:${takeChars(JSON.stringify(args), ARGS_LABEL_CHARS)}`] };
  const one = async (value) => {
    const isObject = value !== null && typeof value === "object" && !Array.isArray(value);
    if (isObject && "handle" in value && "size" in value && "preview" in value) return value;
    const inline = isObject && Object.keys(value).length === 1 && typeof value.text === "string";
    const bytes = inline ? value.text : JSON.stringify(value);
    if (Buffer.byteLength(bytes, "utf8") <= INLINE_MAX) return value;
    const meta = await client.put(Buffer.from(bytes, "utf8"), spec.type, provenance);
    return { handle: meta.id, size: meta.size, preview: takeChars(bytes, PREVIEW_CHARS) };
  };
  if (!spec.fields?.length) return one(reply);
  const out = { ...reply };
  for (const field of spec.fields) {
    if (field in out) out[field] = await one(out[field]);
  }
  return out;
}

/**
 * Connect both channels, declare what this plugin answers, and serve until
 * shutdown.
 *
 * driver + implement: the one way to answer verbs. `driver` is a document
 *   from loadDriver; `implement` maps each short verb name to an async
 *   handler (args, client) → reply. What the kernel is told about each verb
 *   is the driver's wording, never this plugin's; arguments are checked
 *   against the driver's schema before a handler runs, the reply against
 *   its reply schema where the driver states one, and a reply the driver
 *   marks bulky is stored when it is over the line. A name the driver does
 *   not declare fails here, at startup. Omit both for a plugin with no
 *   verbs. Accepted verbs are not offered here yet: no JS driver has one.
 * onEvent(topic, data) receives subscribed events.
 * onReady (optional): async hook run with the client once connected, before
 *   serving — where a plugin whose job begins on its own starts it.
 * needs (optional): verbs this plugin cannot work without; until somebody
 *   answers them it runs but is not routed.
 * subscribes (optional): topics to listen to, registered by the kernel before
 *   the spawn returns. It is the only way to listen — there is no runtime
 *   subscribe — so nothing published "once everything is up" can be missed.
 */
export async function servePlugin({ name, driver, implement, needs, subscribes, onEvent, onReady }) {
  const sock = process.env.PORTOS_PLUGIN_SOCK;
  if (!sock) throw new Error("PORTOS_PLUGIN_SOCK unset");
  const token = process.env.PORTOS_PLUGIN_TOKEN ?? "";

  const handlers = {};
  const tools = {};
  for (const [short, handler] of Object.entries(implement ?? {})) {
    const spec = driver?.verbs?.[short];
    if (!spec) throw new Error(`${name}: ${short} is not a verb of driver ${driver?.driver}`);
    if (spec.accepted) throw new Error(`${name}: ${short} is accepted, which this SDK does not offer yet`);
    const verb = `${driver.driver}::${short}`;
    handlers[verb] = { spec, handler };
    tools[verb] = { description: spec.description, schema: spec.args };
  }
  const verbs = Object.keys(handlers);

  // JS declares only the client channel: the async event loop interleaves
  // event frames with in-flight calls on one connection, so the dedicated
  // events channel (which sync-threaded plugins need) is unnecessary here.
  const serveHello = { name, abi: ABI_VERSION, role: "serve", token, verbs, channels: ["client"] };
  if (verbs.length) serveHello.tools = tools;
  if (needs?.length) serveHello.needs = needs;
  if (subscribes?.length) serveHello.subscribes = subscribes;
  const serveChan = await connectChannel(sock, { hello: serveHello });
  const clientChan = await connectChannel(sock, {
    hello: { name, abi: ABI_VERSION, role: "client", token },
  });
  const client = new KernelClient(clientChan);
  if (onReady) await onReady(client);

  for (;;) {
    let msg;
    try {
      msg = await serveChan.readFrame();
    } catch {
      return; // kernel went away; exit quietly
    }
    if (!msg.op || msg.op === "shutdown") return;
    if (msg.op === "call") {
      try {
        const h = handlers[msg.verb];
        if (!h) throw new Error(`not a verb of this plugin: ${msg.verb}`);
        const args = msg.args ?? {};
        const bad = check(args, h.spec.args);
        if (bad) throw new Error(`${msg.verb}: arguments: ${bad}`);
        const v = await h.handler(args, client);
        let reply = v === undefined ? null : v;
        if (h.spec.bulk) reply = await spill(client, h.spec.bulk, msg.verb, args, reply);
        const off = h.spec.reply ? check(reply, h.spec.reply) : null;
        if (off) throw new Error(`${msg.verb}: reply: ${off}`);
        serveChan.writeFrame({ ok: reply });
      } catch (e) {
        serveChan.writeFrame({ err: String(e?.message ?? e) });
      }
    } else if (msg.op === "event") {
      try {
        onEvent?.(msg.topic, msg.data ?? null);
      } catch {
        // event handlers must not take the serve loop down
      }
    } else {
      serveChan.writeFrame({ err: `unknown op ${msg.op}` });
    }
  }
}
