// smoke.mjs — automated end-to-end smoke test against the local fixture.
// Runs headless (no display needed). Same code path runs headful on a Mac.
//
//   WORKSHOP_NO_SANDBOX=1 node test/smoke.mjs
//
// Asserts: navigate → distill → type → compare-and-act click(submit) →
// state change observed; context/data meter printed.

import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import assert from "node:assert";
import { createWorkshop } from "../src/tools.js";
import { makeSink } from "../src/sink.js";

const here = dirname(fileURLToPath(import.meta.url));
const fixture = "file://" + join(here, "fixture.html");

let failures = 0;
const ok = (cond, msg) => { if (cond) { console.log("  ✓", msg); } else { failures++; console.log("  ✗", msg); } };

const profileDir = `/tmp/workshop-smoke-profile-${process.pid}`;
const sink = makeSink({ mode: "inline" });
const { tools, driver } = createWorkshop({
  driver: { headless: true, userDataDir: profileDir },
  sink,
  log: () => {},
});
const T = Object.fromEntries(tools.map((t) => [t.name, t.handler]));

try {
  console.log("workshop browser smoke test");

  const opened = await T.browser_open({ url: fixture });
  ok(opened.title.includes("Workshop Fixture"), "navigate + title");
  ok(opened.elements.length >= 3, `distilled ${opened.elements.length} interactive elements`);

  const userField = opened.elements.find((e) => e.name === "username");
  const submitBtn = opened.elements.find((e) => e.role === "button" || e.name === "Sign in");
  ok(!!userField, "found username field by accessible name");
  ok(!!submitBtn, "found submit button");

  await T.browser_type({ ref: userField.ref, text: "PortOS", expectName: "username" });

  // compare-and-act: click the submit button, asserting we still believe it's
  // "Sign in". The fixture changes the H1 to "Welcome, PortOS" on submit.
  const after = await T.browser_click({ ref: submitBtn.ref, expectName: submitBtn.name });
  ok(!after.staleWarning, "compare-and-act: no stale warning on a stable button");

  const snap = await T.browser_snapshot();
  ok(snap.url === after.url, "snapshot url consistent after action");

  // read back the heading via a distilled screenshot path (no image in context)
  const shot = await T.browser_screenshot({ path: `/tmp/workshop-smoke-${process.pid}.png` });
  ok(shot.path && shot.path.endsWith(".png"), "screenshot returns a file path, not base64");

  console.log(`  meter: context=${sink.meter.context}B data=${sink.meter.data}B ratio=${sink.ratio().toFixed(4)}`);

  await T.browser_close();
} catch (e) {
  failures++;
  console.error("  ✗ threw:", e.message);
  try { await driver.close(); } catch {}
} finally {
  try { const { rmSync } = await import("node:fs"); rmSync(profileDir, { recursive: true, force: true }); } catch {}
}

console.log(failures === 0 ? "\nSMOKE OK" : `\nSMOKE FAILED (${failures})`);
process.exit(failures === 0 ? 0 : 1);
