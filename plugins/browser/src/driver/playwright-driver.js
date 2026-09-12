// playwright-driver.js — the Playwright-backed browser.
//
// Headful-capable on purpose: on a Mac with `channel: "chrome"` you WATCH the
// real window while the agent works. Agent input goes through Playwright,
// which dispatches CDP synthetic events — it does not move your physical
// mouse, so you can use your own cursor at the same time.
//
// Credential safety is structural, not enforced: a dedicated persistent
// profile holds the logins, and nothing here ever reads a cookie. The agent
// only receives the distilled element table and screenshots.
//
// Options (the plugin's config): headless, channel, profile_dir, no_sandbox,
// chromium_args. Platform defaults fill in what is not said.

import { chromium } from "playwright";
import { distillElements, DISTILL_SCRIPT } from "../distill.js";

const firstLine = (e) => String(e?.message ?? e).split("\n")[0];

/**
 * Launch options with platform-aware defaults, so the same code runs headful
 * on a Mac and headless on a displayless Linux box with nothing configured.
 * On macOS the real Chrome build is tried first; `channelWasAuto` marks the
 * guess so a missing Chrome falls back to bundled Chromium instead of failing.
 */
export function resolveLaunchOptions(opts = {}) {
  const platform = process.platform;
  const hasDisplay =
    platform === "darwin" || platform === "win32" ||
    !!(process.env.DISPLAY || process.env.WAYLAND_DISPLAY);
  const headless = opts.headless ?? !hasDisplay;
  let channel = opts.channel;
  let channelWasAuto = false;
  if (channel === undefined && platform === "darwin") {
    channel = "chrome";
    channelWasAuto = true;
  }
  return { headless, channel, channelWasAuto };
}

export class PlaywrightDriver {
  constructor(opts = {}) {
    this.opts = opts;
    this.context = null;
    this.page = null;
    this._snapshot_id = 0;
    this._lastRefs = new Map(); // ref -> { name, tag }  (compare-and-act seed)
  }

  async _ensure() {
    if (this.context) return;
    const userDataDir =
      this.opts.profile_dir ?? `${process.env.HOME || "/tmp"}/.portos-chrome-profile`;
    const { headless, channel, channelWasAuto } = resolveLaunchOptions(this.opts);

    const args = [...(this.opts.chromium_args ?? [])];
    // Sandboxed CI/containers need this; harmless on a dev Mac if omitted.
    if (this.opts.no_sandbox) args.push("--no-sandbox");

    this.context = await this._launch(userDataDir, { headless, channel, channelWasAuto }, args);
    this.page = this.context.pages()[0] || (await this.context.newPage());
    // Re-inject the distiller on every navigation so refs are always fresh.
    await this.context.addInitScript(DISTILL_SCRIPT);
  }

  async _launch(userDataDir, { headless, channel, channelWasAuto }, args) {
    const tryLaunch = (ch, extraArgs = []) =>
      chromium.launchPersistentContext(userDataDir, {
        headless,
        channel: ch,
        args: [...args, ...extraArgs],
        viewport: { width: 1280, height: 800 },
      });

    try {
      return await tryLaunch(channel);
    } catch (e) {
      // We guessed real Chrome (macOS default) but it isn't installed:
      // fall back to Playwright's bundled Chromium. An explicitly requested
      // channel does NOT fall back — that failure should be seen.
      if (channel && channelWasAuto) {
        console.error(`[browser] ${channel} unavailable (${firstLine(e)}); falling back to bundled Chromium`);
        return this._launch(userDataDir, { headless, channel: undefined, channelWasAuto: false }, args);
      }
      // Linux hosts that restrict unprivileged user namespaces break
      // Chromium's sandbox. Retry loudly without it rather than being dead on
      // arrival; `no_sandbox: true` makes it explicit.
      const sandboxy = /no usable sandbox|user namespaces|clone|operation not permitted|setuid/i.test(String(e));
      if (process.platform === "linux" && sandboxy && !args.includes("--no-sandbox")) {
        console.error(`[browser] chromium sandbox unavailable (${firstLine(e)}); retrying with --no-sandbox`);
        return tryLaunch(channel, ["--no-sandbox"]);
      }
      throw e;
    }
  }

  async _snap() {
    this._snapshot_id += 1;
    const elements = await distillElements(this.page);
    this._lastRefs = new Map(elements.map((e) => [e.ref, { name: e.name, tag: e.tag }]));
    return {
      snapshot_id: this._snapshot_id,
      url: this.page.url(),
      title: await this.page.title().catch(() => ""),
      elements,
    };
  }

  // compare-and-act seed (browser-driver-v0.md §5.3): if the caller says which
  // element it believed it was acting on, verify the DOM hasn't swapped it out
  // from under us. In the demo this only WARNS; hardening turns it into a hard
  // precondition failure.
  _staleCheck(ref, expect_name) {
    if (expect_name == null) return null;
    const cur = this._lastRefs.get(ref);
    if (!cur) return `ref ${ref} no longer present in latest snapshot`;
    if (cur.name && expect_name && cur.name.trim() !== expect_name.trim()) {
      return `ref ${ref} changed: expected "${expect_name}", now "${cur.name}"`;
    }
    return null;
  }

  async open({ url } = {}) {
    await this._ensure();
    if (url) await this.page.goto(url, { waitUntil: "domcontentloaded" });
    return this._snap();
  }

  async navigate({ url }) {
    await this._ensure();
    await this.page.goto(url, { waitUntil: "domcontentloaded" });
    return this._snap();
  }

  async snapshot() {
    await this._ensure();
    return this._snap();
  }

  async click({ ref, expect_name } = {}) {
    await this._ensure();
    const stale_warning = this._staleCheck(ref, expect_name);
    const el = await this.page.$(`[data-wref="${ref}"]`);
    if (!el) throw new Error(`no element for ref ${ref} (snapshot may be stale)`);
    await el.click({ timeout: 5000 });
    await this.page.waitForLoadState("domcontentloaded").catch(() => {});
    const snap = await this._snap();
    return { ...snap, stale_warning };
  }

  async type({ ref, text, expect_name, submit } = {}) {
    await this._ensure();
    const stale_warning = this._staleCheck(ref, expect_name);
    const el = await this.page.$(`[data-wref="${ref}"]`);
    if (!el) throw new Error(`no element for ref ${ref} (snapshot may be stale)`);
    await el.fill(text, { timeout: 5000 });
    if (submit) await el.press("Enter");
    await this.page.waitForLoadState("domcontentloaded").catch(() => {});
    const snap = await this._snap();
    return { ...snap, stale_warning };
  }

  async waitFor({ selector, ms, network_idle } = {}) {
    await this._ensure();
    if (selector) await this.page.waitForSelector(selector, { timeout: ms ?? 10000 });
    else if (network_idle) await this.page.waitForLoadState("networkidle");
    else if (ms) await this.page.waitForTimeout(ms);
    return { ok: true, waited: selector ?? (network_idle ? "networkidle" : `${ms}ms`) };
  }

  async screenshot({ path } = {}) {
    await this._ensure();
    const out = path || `/tmp/portos-shot-${Date.now()}.png`;
    await this.page.screenshot({ path: out, fullPage: false });
    return { path: out };
  }

  /** Current page URL, or null before any page exists: the provenance an
   *  artifact made from the page carries. */
  currentUrl() {
    return this.page ? this.page.url() : null;
  }

  async passthroughBegin({ url } = {}) {
    await this._ensure();
    if (url) await this.page.goto(url, { waitUntil: "domcontentloaded" });
    // In a headful window the human now types directly. Agent input is not
    // sent during this phase (the caller is expected to stop driving).
    return {
      mode: "user_driving",
      hint: "Log in or pass the check in the browser window, then call browser::resume. Passwords and passkeys never reach the agent.",
    };
  }

  async passthroughEnd() {
    await this._ensure();
    return {
      mode: "agent_driving",
      url: this.page.url(),
      logged_in_hint: "The session lives in the dedicated profile; the agent never reads a cookie.",
    };
  }

  async close() {
    if (this.context) await this.context.close();
    this.context = null;
    this.page = null;
    return {};
  }
}
