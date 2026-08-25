// distill.js — the structured page representation (browser-driver-v0.md §5.1,
// L1 distilled element table). We walk the live DOM for interactive elements,
// tag each with a volatile `data-wref` id, and return a compact list the model
// can reason over cheaply. This is the "default lens"; a raw full snapshot
// (L0) and screenshots (L2) are available when the model needs more.
//
// NOTE (honest): this injects a script into the page — exactly what the
// hardened design forbids. In Stage A we use Playwright, which injects anyway,
// so this is consistent with the deliberate "demo uses Playwright; the CDP
// filter proxy is the deferred hardening" decision.

// This function is stringified and injected via addInitScript so `data-wref`
// tagging survives every navigation.
export const DISTILL_SCRIPT = `(() => {
  if (window.__workshopDistill) return;
  window.__workshopDistill = function() {
    const INTERACTIVE = 'a,button,input,select,textarea,[role=button],[role=link],[role=tab],[role=checkbox],[role=menuitem],[contenteditable=true]';
    const out = [];
    let i = 0;
    const nodes = document.querySelectorAll(INTERACTIVE);
    for (const el of nodes) {
      const rect = el.getBoundingClientRect();
      const visible = !!(rect.width || rect.height) &&
        getComputedStyle(el).visibility !== 'hidden' &&
        getComputedStyle(el).display !== 'none';
      if (!visible) continue;
      const ref = 'e' + (++i);
      el.setAttribute('data-wref', ref);
      const name = (
        el.getAttribute('aria-label') ||
        el.getAttribute('placeholder') ||
        el.getAttribute('alt') ||
        el.getAttribute('name') ||
        (el.innerText || el.value || el.title || '')
      ).trim().slice(0, 120);
      const tag = el.tagName.toLowerCase();
      const editable = tag === 'input' || tag === 'textarea' ||
        el.getAttribute('contenteditable') === 'true';
      out.push({
        ref, tag,
        role: el.getAttribute('role') || tag,
        name,
        editable,
        visible: true,
        bbox: { x: Math.round(rect.x), y: Math.round(rect.y),
                w: Math.round(rect.width), h: Math.round(rect.height) },
      });
    }
    return out;
  };
})()`;

/** Run the injected distiller and return the element list. */
export async function distillElements(page) {
  // Ensure the function exists even if addInitScript hasn't run for this doc.
  await page.evaluate(DISTILL_SCRIPT).catch(() => {});
  return page.evaluate(() => window.__workshopDistill());
}
