// driver.js — SEAM #1: the browser driver interface.
//
// The whole point of this seam: the tool surface (tools.js) talks ONLY to
// this interface, never to Playwright directly. Today the implementation is
// Playwright (fastest path to a watchable browser). When we harden — hand-
// rolled CDP behind a filtering proxy, per design/browser-driver-v0.md — we
// swap the implementation here and nothing above changes.
//
// A BrowserDriver implementation must provide:
//   open({url})                  -> { snapshotId, url, title }
//   navigate({url})              -> { snapshotId, url, title }
//   snapshot()                   -> { snapshotId, url, title, elements: [ElementRef] }
//   click({ref, expectName?})    -> { snapshotId, ... , staleWarning? }
//   type({ref, text, expectName?, submit?}) -> { snapshotId, ... }
//   waitFor({selector?, ms?, networkIdle?}) -> { ok, waited }
//   screenshot({path?})          -> { path }        // returns a file path, NOT base64 in context
//   passthroughBegin({url?})     -> { mode: 'user_driving', hint }
//   passthroughEnd()             -> { mode: 'agent_driving', url, loggedInHint }
//   close()                      -> {}
//
// ElementRef = { ref, role, name, tag, bbox:{x,y,w,h}, visible, editable }
//   `ref` is a driver-session-local, volatile id (the two-layer-naming rule,
//   browser-driver-v0.md §14-7). It is NOT a kernel handle. Rebuilt each
//   snapshot; never persisted.

import { PlaywrightDriver } from "./playwright-driver.js";

/**
 * Factory. `impl` selects the backend; only "playwright" exists today, but
 * the seam is here so "cdp-filter" can arrive later without touching callers.
 */
export function createDriver(opts = {}) {
  const impl = opts.impl ?? "playwright";
  switch (impl) {
    case "playwright":
      return new PlaywrightDriver(opts);
    default:
      throw new Error(`unknown driver impl: ${impl}`);
  }
}
