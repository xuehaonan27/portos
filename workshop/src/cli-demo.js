#!/usr/bin/env node
// cli-demo.js — run the browser limb WITHOUT any agent, to see it work.
//
// On your Mac (real, visible Chrome window):
//   WORKSHOP_CHROME_CHANNEL=chrome node src/cli-demo.js https://example.com
// Headless (servers/CI):
//   WORKSHOP_HEADLESS=1 WORKSHOP_NO_SANDBOX=1 node src/cli-demo.js https://example.com
//
// It navigates, prints the distilled element table, screenshots, and prints
// the context/data meter in BOTH sink modes so you can see what the data-plane
// refactor would buy.

import { createWorkshop } from "./tools.js";
import { makeSink } from "./sink.js";
import { resolveLaunchOptions } from "./driver/playwright-driver.js";

const url = process.argv[2] || "https://example.com";
const { headless } = resolveLaunchOptions(); // display only; driver re-resolves

const inlineSink = makeSink({ mode: "inline" });
const { tools, driver } = createWorkshop({
  sink: inlineSink,
  log: (s) => console.log(`[policy] ${s}`),
});
const T = Object.fromEntries(tools.map((t) => [t.name, t.handler]));

try {
  console.log(`opening ${url} (${headless ? "headless" : "headful — watch the window"})`);
  const snap = await T.browser_open({ url });
  console.log(`\n${snap.title}  —  ${snap.elementCount} interactive elements:`);
  for (const e of snap.elements.slice(0, 20)) {
    console.log(`  ${e.ref.padEnd(4)} ${e.role.padEnd(10)} ${JSON.stringify(e.name).slice(0, 60)}`);
  }

  const shot = await T.browser_screenshot({});
  console.log(`\nscreenshot → ${shot.path}`);

  // Show the seam's value: re-deliver the same snapshot through a handle-mode
  // sink and compare what would hit context.
  const handleSink = makeSink({ mode: "handle", previewChars: 300 });
  handleSink.deliver("snapshot", snap);
  console.log("\ncontext/data accounting:");
  console.log(`  inline mode : context=${inlineSink.meter.context}B  (everything hits context)`);
  console.log(`  handle mode : context=${handleSink.meter.context}B  data=${handleSink.meter.data}B  (data plane would keep context tiny)`);

  console.log("\npassthrough login is available via browser_login_passthrough (password/passkey never touch the agent).");
  await T.browser_close();
} catch (e) {
  console.error("demo error:", e.message);
  try { await driver.close(); } catch {}
  process.exit(1);
}
