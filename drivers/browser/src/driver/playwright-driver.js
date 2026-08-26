// playwright-driver.js — the Playwright-backed BrowserDriver (Stage A).
//
// Deliberately headful-capable: on a Mac you pass { headless:false,
// channel:"chrome" } and WATCH the real window while the agent works. Agent
// input goes through Playwright, which dispatches CDP synthetic events — it
// does NOT move your physical mouse, so you can watch elements get clicked
// "by themselves" and even use your own cursor at the same time.
//
// Credential safety in the demo is structural, not enforced: we launch a
// dedicated persistent profile (your logins live there, reused across runs)
// and we simply never expose any cookie/storage read. The agent only ever
// receives distilled DOM + screenshots. Hardening (a CDP filter proxy that
// blocks Network.getCookies at the protocol layer) is the deferred step;
// see design/browser-driver-v0.md.

import { chromium } from "playwright";
import { distillElements, DISTILL_SCRIPT } from "../distill.js";

const firstLine = (e) => String(e?.message ?? e).split("\n")[0];

/**
 * Resolve launch options with platform-aware defaults, so the same code runs
 * headful on a Mac and unattended (headless) on a displayless Linux box with
 * zero configuration. Explicit opts and WORKSHOP_* env vars always win.
 *
 *   headless : WORKSHOP_HEADLESS=1 forces on, =0 forces off; default is
 *              "headful only if a display exists".
 *   channel  : WORKSHOP_CHROME_CHANNEL wins; on macOS default to the real
 *              Chrome build (the watchable-window product). `channelWasAuto`
 *              marks the guess so a missing Chrome can fall back to bundled
 *              Chromium instead of failing.
 */
export function resolveLaunchOptions(opts = {}) {
  const env = process.env;
  const platform = process.platform;
  const hasDisplay =
    platform === "darwin" || platform === "win32" ||
    !!(env.DISPLAY || env.WAYLAND_DISPLAY);

  const headless =
    opts.headless ??
    (env.WORKSHOP_HEADLESS === "1" ? true
      : env.WORKSHOP_HEADLESS === "0" ? false
      : !hasDisplay);

  let channel = opts.channel ?? (env.WORKSHOP_CHROME_CHANNEL || undefined);
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
    this._snapshotId = 0;
    this._lastRefs = new Map(); // ref -> { name, tag }  (compare-and-act seed)
  }

  async _ensure() {
    if (this.context) return;
    const {
      userDataDir = process.env.WORKSHOP_PROFILE_DIR ||
        `${process.env.HOME || "/tmp"}/.workshop-chrome-profile`,
      chromiumArgs = [],
    } = this.opts;
    const { headless, channel, channelWasAuto } = resolveLaunchOptions(this.opts);

    const args = [...chromiumArgs];
    // Sandboxed CI/containers need this; harmless on a dev Mac if omitted.
    if (process.env.WORKSHOP_NO_SANDBOX === "1") args.push("--no-sandbox");

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
        console.error(`[workshop] ${channel} unavailable (${firstLine(e)}); falling back to bundled Chromium`);
        return this._launch(userDataDir, { headless, channel: undefined, channelWasAuto: false }, args);
      }
      // Linux hosts that restrict unprivileged user namespaces break
      // Chromium's sandbox. Retry loudly without it rather than being dead on
      // arrival; WORKSHOP_NO_SANDBOX=1 makes it explicit.
      const sandboxy = /no usable sandbox|user namespaces|clone|operation not permitted|setuid/i.test(String(e));
      if (process.platform === "linux" && sandboxy && !args.includes("--no-sandbox")) {
        console.error(`[workshop] chromium sandbox unavailable (${firstLine(e)}); retrying with --no-sandbox`);
        return tryLaunch(channel, ["--no-sandbox"]);
      }
      throw e;
    }
  }

  async _snap() {
    this._snapshotId += 1;
    const elements = await distillElements(this.page);
    this._lastRefs = new Map(elements.map((e) => [e.ref, { name: e.name, tag: e.tag }]));
    return {
      snapshotId: this._snapshotId,
      url: this.page.url(),
      title: await this.page.title().catch(() => ""),
      elements,
    };
  }

  // compare-and-act seed (browser-driver-v0.md §5.3): if the caller says which
  // element it believed it was acting on, verify the DOM hasn't swapped it out
  // from under us. In the demo this only WARNS; hardening turns it into a hard
  // precondition failure.
  _staleCheck(ref, expectName) {
    if (expectName == null) return null;
    const cur = this._lastRefs.get(ref);
    if (!cur) return `ref ${ref} no longer present in latest snapshot`;
    if (cur.name && expectName && cur.name.trim() !== expectName.trim()) {
      return `ref ${ref} changed: expected "${expectName}", now "${cur.name}"`;
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

  async click({ ref, expectName } = {}) {
    await this._ensure();
    const staleWarning = this._staleCheck(ref, expectName);
    const el = await this.page.$(`[data-wref="${ref}"]`);
    if (!el) throw new Error(`no element for ref ${ref} (snapshot may be stale)`);
    await el.click({ timeout: 5000 });
    await this.page.waitForLoadState("domcontentloaded").catch(() => {});
    const snap = await this._snap();
    return { ...snap, staleWarning };
  }

  async type({ ref, text, expectName, submit } = {}) {
    await this._ensure();
    const staleWarning = this._staleCheck(ref, expectName);
    const el = await this.page.$(`[data-wref="${ref}"]`);
    if (!el) throw new Error(`no element for ref ${ref} (snapshot may be stale)`);
    await el.fill(text, { timeout: 5000 });
    if (submit) await el.press("Enter");
    await this.page.waitForLoadState("domcontentloaded").catch(() => {});
    const snap = await this._snap();
    return { ...snap, staleWarning };
  }

  async waitFor({ selector, ms, networkIdle } = {}) {
    await this._ensure();
    if (selector) await this.page.waitForSelector(selector, { timeout: ms ?? 10000 });
    else if (networkIdle) await this.page.waitForLoadState("networkidle");
    else if (ms) await this.page.waitForTimeout(ms);
    return { ok: true, waited: selector ?? (networkIdle ? "networkidle" : `${ms}ms`) };
  }

  async screenshot({ path } = {}) {
    await this._ensure();
    const out = path || `/tmp/workshop-shot-${Date.now()}.png`;
    await this.page.screenshot({ path: out, fullPage: false });
    return { path: out };
  }

  /** Current page URL, or null before any page exists. Used by the PortOS
   *  adapter to taint-label artifacts by origin. */
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
      hint: "在浏览器窗口里人肉完成登录/验证,然后调用 passthroughEnd。密码/passkey 不经过 agent。",
    };
  }

  async passthroughEnd() {
    await this._ensure();
    return {
      mode: "agent_driving",
      url: this.page.url(),
      loggedInHint: "会话已落在专用 profile 内;agent 从不读取 cookie。",
    };
  }

  async close() {
    if (this.context) await this.context.close();
    this.context = null;
    this.page = null;
    return {};
  }
}
