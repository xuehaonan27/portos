// smoke.mjs — the driver against the local fixture, headless.
//
//   node test/smoke.mjs
//
// Asserts: open → element table → type → click with expect_name → the page
// changed → screenshot is a file.

import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { PlaywrightDriver } from "../src/driver/playwright-driver.js";

const here = dirname(fileURLToPath(import.meta.url));
const fixture = "file://" + join(here, "fixture.html");

let failures = 0;
const ok = (cond, msg) => {
  if (cond) console.log("  ✓", msg);
  else {
    failures++;
    console.log("  ✗", msg);
  }
};

const profileDir = `/tmp/portos-smoke-profile-${process.pid}`;
const driver = new PlaywrightDriver({ headless: true, profile_dir: profileDir });

try {
  console.log("browser driver smoke test");

  const opened = await driver.open({ url: fixture });
  ok(opened.title.includes("Workshop Fixture"), "open + title");
  ok(opened.elements.length >= 3, `distilled ${opened.elements.length} interactive elements`);

  const userField = opened.elements.find((e) => e.name === "username");
  const submitBtn = opened.elements.find((e) => e.role === "button" || e.name === "Sign in");
  ok(!!userField, "found username field by accessible name");
  ok(!!submitBtn, "found submit button");

  await driver.type({ ref: userField.ref, text: "PortOS", expect_name: "username" });
  const after = await driver.click({ ref: submitBtn.ref, expect_name: submitBtn.name });
  ok(!after.stale_warning, "expect_name matched on a stable button");

  const snap = await driver.snapshot();
  ok(snap.url === after.url, "snapshot url consistent after action");

  const shot = await driver.screenshot({ path: `/tmp/portos-smoke-${process.pid}.png` });
  ok(shot.path && shot.path.endsWith(".png"), "screenshot returns a file path");

  await driver.close();
} catch (e) {
  failures++;
  console.error("  ✗ threw:", e.message);
  try {
    await driver.close();
  } catch {}
} finally {
  try {
    const { rmSync } = await import("node:fs");
    rmSync(profileDir, { recursive: true, force: true });
  } catch {}
}

console.log(failures === 0 ? "\nSMOKE OK" : `\nSMOKE FAILED (${failures})`);
process.exit(failures === 0 ? 0 : 1);
