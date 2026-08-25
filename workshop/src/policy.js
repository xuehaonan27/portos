// policy.js — SEAM #2: the single policy chokepoint.
//
// EVERY action passes through policy.check() before the driver executes it.
// In the demo this is nearly a no-op: reads and ordinary acts are allowed;
// "destructive" acts (form submit, downloads, cross-origin navigation) can
// optionally require a confirmation. Ten lines of logic.
//
// But this is THE seam. When we stop deferring enforcement, this is the one
// place the capability check / effect-plan admission plugs in — architecture
// -v0.md §5 and effect-plan-v0.md become the body of check(), and callers
// never change. Keeping every judgment here (instead of scattering ifs across
// the driver) is what makes that later swap a local edit.

const DESTRUCTIVE = new Set(["submit", "download", "cross_origin_nav"]);

/**
 * @param {object} o
 * @param {'supervised'|'allow_all'} [o.mode]  supervised = confirm destructive
 * @param {(action)=>Promise<boolean>} [o.confirmFn]  human confirm hook
 * @param {string[]|null} [o.allowedOrigins]  if set, navigation is origin-scoped
 * @param {(s:string)=>void} [o.log]
 */
export function makePolicy(o = {}) {
  const mode = o.mode ?? "supervised";
  const log = o.log ?? (() => {});
  const allowed = o.allowedOrigins ?? null;

  return {
    /**
     * @param {object} action  { verb, kind?, targetOrigin? }
     * @returns {Promise<{decision:'allow'|'deny', reason?:string}>}
     */
    async check(action) {
      // origin scoping (only if an allowlist was configured)
      if (allowed && action.targetOrigin && !allowed.includes(action.targetOrigin)) {
        return { decision: "deny", reason: `origin not allowed: ${action.targetOrigin}` };
      }
      const destructive = DESTRUCTIVE.has(action.kind);
      if (destructive && mode === "supervised" && o.confirmFn) {
        const ok = await o.confirmFn(action);
        log(`policy: destructive ${action.verb} → ${ok ? "allow" : "deny"}`);
        return ok ? { decision: "allow" } : { decision: "deny", reason: "user declined" };
      }
      log(`policy: ${action.verb} → allow`);
      return { decision: "allow" };
    },
  };
}
