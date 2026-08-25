// PortOS plugin protocol client — ABI v2, JS side. Zero dependencies.
//
// Mirrors crates/portos-sdk: a plugin connects to $PORTOS_PLUGIN_SOCK twice
// (roles "serve" and "client", both authenticated by $PORTOS_PLUGIN_TOKEN).
// Frames are 4-byte LE length + JSON; payloads ride after a frame as raw
// chunks (4-byte LE length + bytes, zero-length terminator) and never inside
// JSON (decisions-v1.md D25 — this is why no native addon is needed).
//
// Lives with the browser driver for now; becomes a shared JS SDK when a
// second JS driver exists (D30, two-implementations rule).

import net from "node:net";

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

  /** Call another plugin's verb through the kernel (capability-checked there). */
  invoke(verb, args = null) {
    return this._serial(async () => {
      this.chan.writeFrame({ op: "invoke", verb, args });
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

  /** Subscribe to a topic; events arrive via servePlugin's onEvent. */
  subscribe(topic) {
    return this._serial(async () => {
      this.chan.writeFrame({ op: "subscribe", topic });
      return unwrapOk(await this.chan.readFrame()).sub;
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
 * Connect both channels, declare verbs, and serve until shutdown.
 * onCall(verb, args, client) → result (thrown errors become {"err"}).
 * onEvent(topic, data) receives subscribed events.
 */
export async function servePlugin({ name, verbs, onCall, onEvent }) {
  const sock = process.env.PORTOS_PLUGIN_SOCK;
  if (!sock) throw new Error("PORTOS_PLUGIN_SOCK unset");
  const token = process.env.PORTOS_PLUGIN_TOKEN ?? "";

  const serveChan = await connectChannel(sock, {
    hello: { name, abi: ABI_VERSION, role: "serve", token, verbs },
  });
  const clientChan = await connectChannel(sock, {
    hello: { name, abi: ABI_VERSION, role: "client", token },
  });
  const client = new KernelClient(clientChan);

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
        const v = await onCall(msg.verb, msg.args ?? null, client);
        serveChan.writeFrame({ ok: v === undefined ? null : v });
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
