# PortOS Agent Guide

`AGENTS.md` links to this file (`CLAUDE.md`); edit this shared source.
PortOS is a Rust workspace developing an Agent OS/Runtime. Drivers and plugins exists in separate directories like `drivers/` and `plugins/`.

## Behavioral Guidelines

Behavioral guidelines to reduce common LLM coding mistakes, combined with the
project-specific instructions below.

**Tradeoff:** These guidelines bias toward caution over speed. For trivial tasks,
use judgment.

### 1. Think Before Coding

**Don't assume. Don't hide confusion. Surface tradeoffs.**

Before implementing:

- Read the affected module and its tests. Use the code map and linked design
  docs to locate ownership boundaries.
- State your assumptions explicitly. If uncertain, ask.
- If multiple interpretations exist, present them; don't pick silently.
- If a simpler approach exists, say so. Push back when warranted.
- If something is unclear, stop. Name what's confusing. Ask.

### 2. Simplicity First

**Minimum code that solves the problem. Nothing speculative.**

- No features beyond what was asked.
- No abstractions for single-use code.
- No "flexibility" or "configurability" that wasn't requested.
- No error handling for impossible scenarios.
- If you write 200 lines and it could be 50, rewrite it.

Ask yourself: "Would a senior engineer say this is overcomplicated?" If yes,
simplify.

### 3. Surgical Changes

**Touch only what you must. Clean up only your own mess.**

When editing existing code:

- Don't "improve" adjacent code, comments, or formatting.
- Don't refactor things that aren't broken.
- Match existing style, even if you'd do it differently.
- If you notice unrelated dead code, mention it; don't delete it.
- Preserve unrelated work already present in the checkout.

When your changes create orphans:

- Remove imports, variables, and functions that YOUR changes made unused.
- Don't remove pre-existing dead code unless asked.

The test: Every changed line should trace directly to the user's request.

### 4. Goal-Driven Execution

**Define success criteria. Loop until verified.**

Transform tasks into verifiable goals:

- "Add validation" -> "Write tests for invalid inputs, then make them pass."
- "Fix the bug" -> "Write a test that reproduces it, then make it pass."
- "Refactor X" -> "Ensure tests pass before and after."

For multi-step tasks, state a brief plan:

```text
1. [Step] -> verify: [check]
2. [Step] -> verify: [check]
3. [Step] -> verify: [check]
```

Strong success criteria let you loop independently. Weak criteria ("make it
work") require constant clarification.

Run the narrowest relevant checks during development, then the applicable
repository checks below. Report what ran and any checks blocked by missing
hardware, permissions, dependencies, or credentials.

**These guidelines are working if:** fewer unnecessary changes in diffs, fewer
rewrites due to overcomplication, and clarifying questions come before
implementation rather than after mistakes.

## Core Concepts

The goal is a usable AI workstation. "Usable" is the only acceptance criterion;
theoretical elegance is not. Current state and build order live in
`.dev/plans/workstation-v1.md` — read it before planning work.

- **Control plane / data plane.** Model context is the control plane. Payloads,
  credentials and bulk data are the data plane. They meet only through handles.
  A large tool result goes into the CAS; the model receives a handle plus a
  preview and dereferences with `artifact::read` when it actually needs the
  bytes. Keeping context bytes far below data bytes is the health metric.
- **Driver model.** The kernel does not know what a "browser" is. It knows
  processes, `family::verb` strings, capabilities, handles and events. All
  domain knowledge lives in plugins; a driver family interface is defined
  outside the kernel. This is where extensibility comes from — adding a new
  kind of plugin must never require a new kernel mechanism.
- **Types at the boundary, opacity inside.** A frame's envelope is a typed
  enum the kernel must be able to reject; a verb's payload is `Payload`,
  raw bytes with no accessor. `serde_json::Value` is right in exactly three
  places and wrong everywhere else: a provider's own evolving wire format
  (kept verbatim so replay is faithful), the append-only audit log, and
  operator-edited config files. Anywhere else, name the type.
- **Capabilities double as the tool surface.** A granted verb joined with the
  metadata its driver advertised in `hello` is a tool definition; the model
  driver builds its tool list from `grants` introspection. Capabilities are a
  routing and accounting mechanism here, not a security ceremony — `chat.json`
  declares grants and that is the whole approval story.
- **Teardown is enforced, not requested.** Each plugin is spawned as its own
  process-group leader, and shutdown escalates — `shutdown` frame, then
  SIGTERM to the group, then SIGKILL — so a plugin that ignores the polite
  request goes anyway, and so does everything it started. A driver's real
  cost is usually its grandchildren (chromium under the browser driver), and
  `Child::kill` never sees those. The CLI blocks SIGINT/SIGTERM and waits for
  them on a dedicated thread rather than dying in a default handler, because
  a signal that skips teardown orphans the whole tree.
- **A long operation is accepted, not awaited.** `model::send` returns once
  the turn is admitted; the turn runs on its own thread and reports through
  the event plane, and `model::cancel` stops it. The consequence to respect:
  every such verb owes its subscribers exactly one terminal event — done,
  cancelled, or failed — or a front end waits forever.
- **`kernel::` is the kernel's own verb family, and it is reserved.** A
  plugin declaring it is rejected. `spawn`/`stop`/`plugins` are dispatched
  internally *after* the same capability check a routed verb gets, and they
  carry built-in tool metadata, so a caller granted `driver:kernel` sees
  `kernel__spawn` as an ordinary tool. This is what lets a running system
  gain a capability it did not have: the agent starts a driver mid-session
  and the tool surface, recomputed every turn, shows it on the next one.
- **Hot-unplug needs no theory here, because the unit of plugging is a
  process.** The plugin's own state dies with it; what the kernel keeps is a
  list short enough to write down — routes, subscriptions, socket file,
  process group, capabilities — and `kernel::stop` collects all five. One
  rule makes it work: a capability is held by a *running plugin*, not by a
  name, so stopping revokes what it held, while what others were granted
  about its family goes inert with the route and returns if it does.
  Artifacts and the audit log survive on purpose: immutable records are not
  state. `crates/portos-echo/tests/hotplug.rs` asserts the list.
- **Rendering is event subscription.** A renderer is an ordinary plugin with
  zero verbs and zero capabilities that subscribes to `model::session::*`.
  Several may compose. `drivers/render-tty` is the reference.
- **Credentials stop at the broker.** Plugins get no direct network. Anything
  reaching the outside world invokes `egress::*`; the broker checks the
  allowlist and injects the key, which exists in no other process.

Deliberately absent, and not to be reintroduced without a concrete pain that
is sharp enough to write a spec from: effect-plan language and interpreter,
consent ceremonies, taint egress gates, resource-class declarations, holdings
ledgers and leases. PortOS also never acts as anyone's MCP server (consuming
MCP later is the opposite direction and is fine).

## Build and Validate

```sh
cargo build --workspace
cargo test --workspace          # 59 tests; all must pass, zero warnings
cargo fmt --all
```

The end-to-end tests are the ones that matter and they are hermetic:

- `crates/portos-cli/tests/chat.rs` — the full chain through the real CLI
  binary: user line → modeld → broker (key injection) → scripted provider →
  tool_use → capability-gated invoke → headless Chromium → tool_result →
  streamed text. Needs `node` and `npm install` in `drivers/browser`; skips
  with a printed reason otherwise, so check for "skipping:" in the output
  before believing a green run.
- `crates/portos-echo/tests/abi_v2.rs` — plugin ABI conformance.
- `crates/portos-broker/tests/egress.rs` — allowlist, injection, sanitizing.
- `crates/portos-echo/tests/hotplug.rs` — a running system gaining and losing
  a capability, and the residue list after it loses one.
- `crates/portos-echo/tests/bridge.rs` — the extensibility claim itself: a
  plugin carrying the event plane and the invoke path over HTTP, written
  against the published ABI with no kernel change. If a change here starts
  needing one, that is the finding, not an inconvenience.

Run the narrowest relevant test first, then the full workspace before
reporting done. Never claim a driver works from unit tests alone — the
walking skeleton test is what proves the wiring.

## Configuration and Conventions
- Use Rust 2024, existing style, and tracing initialized only in binaries:
  `info` for lifecycle, `debug` for internals, `warn` for recoverable issues,
  `error` for unrecoverable failures. Update schemas/config/docs with contract changes.
- Use Conventional Commit prefixes (`feat:`, `fix:`, `refactor:`, `ci:`, `chore:`).
- Describe in comment, instead of referencing to documentation section labels.

## Workspace Codemap

- `crates`: kernel and kernel-side facilities.
    - `portos-kernel`: four responsibilities only — `caps` (authorization,
      and the join that builds the tool surface), `cas` (data plane),
      `host` (ABI v2: spawn, verb routing, event bus, chunked streaming),
      `audit`. Domain vocabulary here is an architectural violation.
    - `portos-proto`: the wire. `wire` holds the protocol as types — the
      envelope is an enum so a malformed frame is refused rather than
      defaulted, and `Payload` is unparsed JSON the kernel forwards without
      being able to read it. `ids` holds `Verb`/`Topic`/`PluginName`/`SubId`,
      which parse once so their accessors are total. Also the frame codec,
      chunk streaming, `Capability`, `Label`, artifact metadata.
    - `portos-sdk`: the Rust plugin side; `sdk/js/client.js` is its JS twin.
    - `portos-broker`: the egress chokepoint. Trusted, kernel-spawned, not a
      driver.
    - `portos-cli`: `portos init|put|meta|get|audit-verify|chat`. Links the
      kernel as a library; daemonization is deferred to W4.
    - `portos-echo`: the toy plugin the ABI conformance tests drive.
- `drivers`: driver plugins and family-interface libraries.
    - Family interfaces — the contract between an implementation and its
      callers, depended on by both so a wire shape is never written twice:
      `egress-api` (`portos-egress-api`), `model-api` (`portos-model-api`).
      The kernel depends on neither; it must not know these families exist.
    - Implementations: `model` (Rust — neutral agentic loop in `core.rs`,
      providers under `backends/`), `browser` (JS/Playwright), `render-tty`
      (renderer reference), `bridge-http` (the event plane and the invoke
      path over HTTP/SSE, so a presenter can live off-box; transport and
      presentation are separate files on purpose).
- `docs`: does not exist yet. Solid documentation goes here only after human
  approval; until then everything lives in `.dev`.
- `.dev` (gitignored): temporal development space, never added into git worktree.
    - `.dev/plans`: plans that's a draft, describing what's going to do, *MIGHT NOT* be precise or valid.
    - `.dev/gen`: store agent generated documentations.
    - `.dev/root`: ephemeral `root` that `PortOS` uses when doing experiment, test or debugging. Wiping content is allowed in this ephemeral root. Use this instead of creating directory under `/tmp` when possible. Create more directories of roots under `.dev/tmp` with prefix `root-` if multiple roots needed.
    - `.dev/tmp`: everything else that should lives in `.dev` but not in categories described above.

## Documentations
Temporal plans lives in `.dev/plans`, describing what's going to do.
Agent generated documentation could only lives under `.dev/gen`.
Only solid and documentation could go into `docs/` after human approval / refinement.

When writing, using plain and comprehensive sentences with accurate term use.

## Details by Topic

Keep this guide concise; put implementation details in the linked docs or code.
