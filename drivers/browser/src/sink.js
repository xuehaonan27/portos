// sink.js — SEAM #3: the result sink.
//
// Every tool result the agent will see passes through deliver(). Modes:
//   - "inline": everything into context (the original demo behavior).
//   - "handle": the data-plane shape with a stub backend (cli-demo compare).
//   - "kernel": the real thing — payloads over `inlineMax` go to the kernel
//     CAS through an injected async `put(kind, text, origin)` and the model
//     receives handle + metadata + bounded preview (architecture-v0.md §4.4).
//     Small payloads stay inline: the distilled element table is the model's
//     working lens, and truncating it would break acting on refs.
//
// It also meters context bytes vs data bytes — the health metric of the whole
// architecture (§4.5).

export function makeSink(o = {}) {
  const mode = o.mode ?? "inline";
  const previewChars = o.previewChars ?? 2000;
  const inlineMax = o.inlineMax ?? 16 * 1024;
  const meter = o.meter ?? { context: 0, data: 0 };

  return {
    meter,
    /**
     * @param {string} kind    e.g. "snapshot" | "screenshot" | "text"
     * @param {any} payload
     * @returns {any} what the model receives ("kernel" mode: a Promise)
     */
    deliver(kind, payload) {
      const s = typeof payload === "string" ? payload : JSON.stringify(payload);
      const bytes = Buffer.byteLength(s, "utf8");
      if (mode === "inline") {
        meter.context += bytes;
        return payload;
      }
      if (mode === "kernel") {
        if (bytes <= inlineMax) {
          meter.context += bytes;
          return payload;
        }
        meter.data += bytes;
        const preview = s.slice(0, previewChars);
        meter.context += Buffer.byteLength(preview, "utf8");
        // Origin rides along so the artifact can carry its taint label.
        const origin = originOf(payload);
        return o.put(kind, s, origin).then((meta) => ({
          handle: meta.id,
          size: meta.size,
          type: meta.type,
          preview,
        }));
      }
      // mode === "handle": stub shape for the cli-demo comparison.
      meter.data += bytes;
      const preview = s.slice(0, previewChars);
      meter.context += Buffer.byteLength(preview, "utf8");
      return { handle: `blob:pending:${kind}`, size: s.length, preview };
    },

    ratio() {
      return meter.data === 0 ? (meter.context === 0 ? 0 : Infinity) : meter.context / meter.data;
    },
  };
}

function originOf(payload) {
  try {
    const origin = new URL(payload?.url).origin;
    return origin === "null" ? null : origin;
  } catch {
    return null;
  }
}
