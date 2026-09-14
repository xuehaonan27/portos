// Test fixture: a minimal JS plugin proving the JS protocol client speaks
// ABI v2 against the real kernel host (spawned by tests/abi_v2.rs). Its
// driver is written inline: a document is a document wherever it is read
// from, and this one has no second reader.

import { servePlugin } from "../../../../sdk/js/client.js";

const any = {};
const positional = (...items) => ({
  type: "array",
  minItems: items.length,
  maxItems: items.length,
  prefixItems: items,
});

await servePlugin({
  name: "portos-jse",
  driver: {
    driver: "jse",
    verbs: {
      ping: { description: "Answer with the arguments.", args: { type: "array" } },
      store: { description: "Put a string into the CAS.", args: positional({ type: "string" }) },
      fetch: { description: "Read an artifact back as text.", args: positional({ type: "string" }) },
      publish: {
        description: "Emit an event: [topic, data].",
        args: positional({ type: "string" }, any),
        reply: { type: "object", required: ["delivered"], properties: { delivered: { type: "integer" } } },
      },
    },
  },
  implement: {
    ping: async (args) => ({ pong: args }),
    store: async (args, client) => {
      const meta = await client.put(Buffer.from(String(args[0]), "utf8"), "text/plain");
      return { meta };
    },
    fetch: async (args, client) => {
      const buf = await client.read(String(args[0]));
      return { text: buf.toString("utf8"), bytes: buf.length };
    },
    publish: async (args, client) => {
      const delivered = await client.emit(String(args[0]), args[1] ?? null);
      return { delivered };
    },
  },
});
