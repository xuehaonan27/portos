// sink.js — SEAM #3: the result sink.
//
// Every tool result the agent will see passes through deliver(). In the demo
// it returns the payload inline — ordinary MCP behavior, everything flows into
// model context. Because it is a single function, the eventual data-plane
// refactor (large payload → CAS, return handle + preview; architecture-v0.md
// §4.4) is a local change here, not a rewrite of every tool.
//
// It also meters context bytes vs data bytes — the health metric of the whole
// architecture (§4.5). Even in the demo we can watch that number and let the
// pain of a bloated context be the thing that motivates the data plane.

export function makeSink(o = {}) {
  const mode = o.mode ?? "inline";
  const previewChars = o.previewChars ?? 2000;
  const meter = o.meter ?? { context: 0, data: 0 };

  return {
    meter,
    /**
     * @param {string} kind    e.g. "snapshot" | "screenshot" | "text"
     * @param {any} payload
     * @returns {any} what the model receives
     */
    deliver(kind, payload) {
      if (mode === "inline") {
        const s = typeof payload === "string" ? payload : JSON.stringify(payload);
        meter.context += Buffer.byteLength(s, "utf8");
        return payload;
      }
      // mode === "handle": the shape the data plane will take. Stub for now;
      // real CAS ingest lands when we stop deferring the data plane.
      const s = typeof payload === "string" ? payload : JSON.stringify(payload);
      meter.data += Buffer.byteLength(s, "utf8");
      const preview = s.slice(0, previewChars);
      meter.context += Buffer.byteLength(preview, "utf8");
      return { handle: `blob:pending:${kind}`, size: s.length, preview };
    },

    ratio() {
      return meter.data === 0 ? (meter.context === 0 ? 0 : Infinity) : meter.context / meter.data;
    },
  };
}
