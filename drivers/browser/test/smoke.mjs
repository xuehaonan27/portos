// smoke.mjs — automated end-to-end smoke test against the local fixture.
// Runs headless (no display needed). Same code path runs headful on a Mac.
//
//   WORKSHOP_NO_SANDBOX=1 node test/smoke.mjs
//
// Asserts: open (launch only) → navigate → distill → type → submit (Enter,
// the hard-list verb) → state change observed → compare-and-act click →
// screenshot as a file path; the interface splits hold (open and type refuse
// the arguments that would change their character); context/data meter
// printed.

import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { createWorkshop } from "../src/tools.js";
import { makeSink } from "../src/sink.js";

const here = dirname(fileURLToPath(import.meta.url));
const fixture = "file://" + join(here, "fixture.html");

let failures = 0;
const ok = (cond, msg) => { if (cond) { console.log("  ✓", msg); } else { failures++; console.log("  ✗", msg); } };
const rejects = async (p, msg) => {
  try { await p; ok(false, msg); } catch { ok(true, msg); }
};

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

  // Every tool declares its character (the kernel's truth-table row).
  ok(tools.every((t) => typeof t.character?.kind === "string"), "every tool declares a verb character");

  const launched = await T.browser_open();
  ok(Array.isArray(launched.elements), "open launches and snapshots without navigating");
  await rejects(T.browser_open({ url: fixture }), "open refuses a url (navigate is its own verb)");

  const opened = await T.browser_navigate({ url: fixture });
  ok(opened.title.includes("Workshop Fixture"), "navigate + title");
  ok(opened.elements.length >= 3, `distilled ${opened.elements.length} interactive elements`);

  const userField = opened.elements.find((e) => e.name === "username");
  const submitBtn = opened.elements.find((e) => e.role === "button" || e.name === "Sign in");
  ok(!!userField, "found username field by accessible name");
  ok(!!submitBtn, "found submit button");

  await T.browser_type({ ref: userField.ref, text: "PortOS", expectName: "username" });
  await rejects(
    T.browser_type({ ref: userField.ref, text: "x", submit: true }),
    "type refuses submit=true (submit is its own verb)",
  );

  // Enter in the field: the fixture changes the H1 to "Welcome, PortOS".
  const submitted = await T.browser_submit({ ref: userField.ref, expectName: "username" });
  ok(!submitted.staleWarning, "submit: no stale warning on a stable field");
  const changed = await T.browser_wait_for({ selector: 'h1:has-text("Welcome, PortOS")', ms: 5000 });
  ok(changed.ok, "submit changed the page state");

  // compare-and-act: click the submit button, asserting we still believe it's
  // "Sign in".
  const after = await T.browser_click({ ref: submitBtn.ref, expectName: submitBtn.name });
  ok(!after.staleWarning, "compare-and-act: no stale warning on a stable button");

  const snap = await T.browser_snapshot();
  ok(snap.url === after.url, "snapshot url consistent after action");

  // screenshot: a file path chosen by the driver (no image in context)
  const shot = await T.browser_screenshot();
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
