# PortOS Agent Guide

`AGENTS.md` links to this file (`CLAUDE.md`); edit this shared source.
PortOS is a Rust workspace developing an Agent OS/Runtime.

**There is one kind of runnable thing: a plugin** — a process that speaks the
ABI. The kernel has no notion of kinds and nothing branches on one. The tree
says so: `abi/` is the wire, `kernel/` is the kernel, `sdk/` is the plugin
side, `drivers/` holds the interfaces that regulate a class of plugin (they
never run), `plugins/` holds everything that does, and `cli/` is the front
door. **New components are written as plugins, and any plugin can be
replaced by another implementation of its driver's interface with no change
anywhere else.** Nothing outside a plugin may find it by name — only by what
it answers. `cli/tests/run.rs` proves it on the model driver.

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

### 1b. No Special Cases

**A special case is a design defect reporting itself. Stop.**

When something needs an `if` before the general path — a reserved name handled
before dispatch, a verb intercepted before it is routed, one caller that must
be treated differently — do not write the branch. Go back and look at the
design, because the branch is evidence that the general mechanism is the wrong
shape or is missing something.

This is not a style preference. It has cost real time here: `kernel::*` was
intercepted before routing, `artifact::read` was intercepted before invoking,
`remote` built a parallel router outside the kernel, and each of those made the
missing abstraction — one routing mechanism — look *less* needed rather than
more. Three workarounds read as three solved problems.

Corollary, because it is the harder half: **a good workaround is worse than a
bad one.** It hides the gap. When you find yourself pleased with how cleanly
you routed around something, that is the moment to stop.

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
- **The CAS is a filesystem with one rule: the name is derived from the
  content.** `<root>/objects/<hh>/<rest>`, one file per artifact, holding
  exactly the bytes — not a layer over a filesystem, a naming discipline on
  one. The rule earns its keep where a path cannot: the same bytes get the
  same name on every machine, a handle still means the same bytes after five
  intervening tool calls, what ran is checkable afterwards, and a
  materialised copy never needs invalidating. Objects are stored **0444**, so
  immutability is enforced rather than promised — which is what makes it safe
  to hand a plugin the *path* of one instead of copying the bytes.
  `ClientOp::Locate` does that, and it is what stops a stored result being a
  dead end: `shell::run {artifacts: {LOG: handle}}` lets `grep` work where
  the bytes already are, and `fs::write {artifact}` gets them back out — both
  without anything crossing the model's context. Paths go to plugins, never
  to the model: the model knows only handles, because a path is a mutable
  name for immutable content.
- **Plumbing belongs in the SDK, not in every plugin.** A plugin should be
  about its business. Running a child you can be sure of collecting
  (`scope`), knowing what you were configured to be (`config`), and
  answering with something that might be large (`bulk`) are the same problem
  for every plugin, and each was re-invented before it moved — the shell
  driver's hand-rolled reclamation got it wrong twice while it owned it
  alone. A plugin never learns which runtime form it got, and must not have
  to.
- **Driver model.** The kernel does not know what a "browser" is. It knows
  processes, `driver::verb` strings, capabilities, handles and events. All
  domain knowledge lives in plugins; a driver interface is defined
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
  routing and accounting mechanism here, not a security ceremony — `portos.json`
  declares grants and that is the whole approval story.
- **Teardown is enforced, not requested.** Shutdown escalates — `shutdown`
  frame, then SIGTERM to the process group, then SIGKILL — so a plugin that
  ignores the polite request goes anyway, and so does everything it started.
  A driver's real cost is usually its grandchildren (chromium under the
  browser driver), and `Child::kill` never sees those. The CLI blocks
  SIGINT/SIGTERM and waits for them on a dedicated thread rather than dying
  in a default handler, because a signal that skips teardown orphans the
  whole tree.
- **What a plugin is and how it runs are two axes.** The first is
  `LaunchSpec`'s `artifact` (an executable in the CAS) or `bin` (a path on
  this host) — exactly one. A content address is a claim anyone can check
  and means the same thing on every machine; a path is a claim about a file
  that may since have changed, so it is kept as a labelled escape hatch for
  what is already installed. It is also what makes `kernel::spawn` safe to
  hand an agent: a spec travels through the model's context, so it must
  *name* things rather than contain them. The kernel materialises an
  artifact into `<root>/exec/<id>` once — content addressing makes that
  cache correct with no invalidation rule — and the audit records which of
  the two a plugin was started by. A plugin of several files adds `bundle`:
  a tar in the CAS, unpacked once and made the process's working directory,
  so paths *inside* the plugin keep meaning what they meant when it was
  built. `bundle` is a second question, not an alternative — what files it
  has, versus what runs — and a JS plugin needs one answer to each, from
  different places (its script from the bundle, `node` from the host).
  `portos bundle <root> <base> [paths…]` builds one.
- The second axis is
  `LaunchSpec.form`, and the kernel implements exactly the two it can
  without learning a domain: `bare` (a child process leading its own group)
  and `cgroup` (that child inside a cgroup of its own, the default).
  Containers and microVMs belong on this axis too, as drivers — a form is a
  driver, so the route table is the launcher registry and no new
  mechanism is needed. A cgroup closes the two holes a process group has,
  both measured rather than assumed: a child can leave a process group with
  `setsid` and cannot leave a cgroup, and a killed runtime leaves a *named
  directory* where a process group leaves nothing, so the next run collects
  what the last one dropped. Removing the cgroup is the proof it worked —
  `rmdir` refuses one that still holds anything. It needs no root and no
  controller; it falls back to `bare` where cgroup v2 is unavailable. What
  it does *not* reclaim is worth knowing: mounts, network config and IPC
  objects belong to namespaces, and nothing at any level undoes a request
  that already went out.
- **A long operation is accepted, not awaited.** `model::send` returns once
  the turn is admitted; the turn runs on its own thread and reports through
  the event plane, and `model::cancel` stops it. The consequence to respect:
  every such verb owes its subscribers exactly one terminal event — done,
  cancelled, or failed — or a front end waits forever.
- **`kernel::` is the kernel's own driver, answered by the instance named
  `kernel`.** Nothing is reserved: a plugin may answer `kernel::spawn` too —
  a launcher for a form the kernel does not know — and callers then name
  which. `spawn`/`stop`/`plugins` are rows in the same table as everything
  else, dispatched *after* the same capability check a routed verb gets,
  with built-in tool metadata, so a caller granted `driver:kernel` sees
  `kernel__spawn` as an ordinary tool. This is what lets a running system
  gain a capability it did not have: the agent starts a driver mid-session
  and the tool surface, recomputed every turn, shows it on the next one.
- **Hot-unplug needs no theory here, because the unit of plugging is a
  process.** The plugin's own state dies with it; what the kernel keeps is a
  list short enough to write down — routes, subscriptions, socket file,
  process group, capabilities — and `kernel::stop` collects all five. One
  rule makes it work: a capability is held by a *running plugin*, not by a
  name, so stopping revokes what it held, while what others were granted
  about its driver goes inert with the route and returns if it does.
  Artifacts and the audit log survive on purpose: immutable records are not
  state. `plugins/echo/tests/hotplug.rs` asserts the list.
- **A result the model might not read does not enter its context.** Every
  bulky verb answers `{text}` when small and `{handle, size, preview}` when
  not — one shape, defined once in `portos_sdk::bulk`, because `modeld`
  already promised the model that shape in the `artifact::read` tool
  description. Where the line falls is a constant, not a knob. Pipes count
  as context discipline too: `shell::run` takes a whole `sh -c` string
  precisely so `… 2>&1 | tail -40` can shrink a log before it is ever
  carried.
- **Another node is an instance, not a kernel feature.** `plugins/remote`
  dials a peer's `bridge-http` and declares what it finds under the same
  verbs: `browser::open` there is `browser::open` here, answered by the
  instance `portos-remote-mac`, and a caller with a browser on each node
  names the one it means. Nothing is renamed — an earlier version prefixed
  the node onto the driver, which was the instance smuggled into the name.
  Authority stays split, which is the part worth keeping: the far node
  decides *what is exposed* (the grants on its bridge, read once at
  startup), this node decides *who may use it* — and the two failures are
  distinguishable, since a verb the peer never exposed has no local route
  at all.
- **The launcher is the one thing that is not a plugin, and it knows no
  driver.** `portos run <root>` opens the kernel, starts what
  `<root>/portos.json` lists, mints each entry's grants, and parks. A chat
  is not a mode of it: a chat is a model driver plus a front end plus
  whatever else is listed, and `portos init` is the only place the standard
  set — broker, model driver, terminal front end — is spelled out, into the
  file. The REPL that used to live in the CLI was a front end masquerading
  as the launcher, with a renderer that was not a plugin and a launcher that
  knew `model::start`; both are gone.
- **Reload is re-plug.** `SIGHUP` to `portos run` re-reads `portos.json`
  and brings the running set back in line — whatever changed is stopped and
  started again, whatever did not is left alone. There is no second
  mechanism: `kernel::stop` already collects a plugin's whole residue and
  starting it again puts it back, grants included, so reload is a policy
  over the plugin lifecycle rather than a per-setting "which of these are
  live?" matrix. An entry's grants and the files it lists under `watch`
  count as part of what it is — filling in an API key changes the broker
  without changing its command line, and that is the case reload exists
  for — so editing a grant re-plugs exactly that plugin, and a grant removed
  is a grant revoked.
- **A conversation outlives the driver that held it.** The transcript is
  written to the CAS after every turn and `modeld/sessions.json` records
  where it went — handles and small facts only, so listing sessions never
  reads one. In-memory sessions are a *cache*: `model::send` to an id this
  process has not seen loads it from the store rather than calling it
  unknown, which is what makes restarting the driver invisible to a front
  end. The ordering is load-bearing in the same way the cancel flag was: a
  terminal event promises the turn is durable, so the checkpoint happens
  *before* `done` goes out, never after.
- **One decision, one mechanism: routing.** *Given a name, who answers it?*
  was being decided in several places and differently each time, and where
  it did not fit the answer was an `if` before the general path. The
  `router` driver states the laws, and each implementation keeps its own
  table and its own idea of what a target is. The kernel's targets are a
  plugin or itself; the SDK's are its handlers; the model driver's are
  "here" or "out through the kernel". A target names an answerer; reaching
  it is a separate step, which is what lets one table route to a process, a
  function, or something across a link without knowing the difference.
- **A verb names a driver's verb; which instance answers is routing, not
  naming.** `browser::open` is what the browser interface calls opening.
  Any number of instances may answer it — two browsers here, one on
  another node — and each is a plugin under its own name, given by the
  launcher (`LaunchSpec.name`, `"name"` in `portos.json`) because only the
  launcher knows there are two. With one instance the verb alone resolves;
  with several the caller names one (`invoke_at`, `Invoke.at`, the
  `instance` argument modeld puts on the tool exactly when there is a
  choice) and an unnamed call is *ambiguous* — an error listing who could
  have been named, never a choice made on the caller's behalf. Grants stay
  on the driver (`driver:browser`): whether you may call it is authority,
  which one you reach is routing, and a grant reports the instances so a
  caller knows a choice exists. This replaced "a family has one answerer",
  which made the name do two jobs, and the second job leaked out as encoded
  names (`mac_browser::open`) that nothing could parse back. "Family" is
  gone from the vocabulary with it: the first segment of a verb is the
  driver. Deliberately not done: events carry no instance, so two nodes'
  `model::session::s1` are indistinguishable — the trigger is a front end
  that needs to tell them apart.
- **A plugin says what it cannot work without; nothing declares an order.**
  `Plugin::needs(&egress::HTTP)` — a `&Verb`, which should be a driver's own
  constant, because a dependency is a statement in some driver's vocabulary
  and a plugin that cannot name the driver it depends on is depending on a
  rumour. Until every need resolves, the plugin runs and keeps its names but
  **is not routed**: callers see exactly what they see for a stopped plugin,
  because "not there yet" and "not there any more" are the same thing to a
  caller. Readiness is re-judged after every spawn and shutdown, repeatedly
  until nothing moves — and that loop is the whole of dependency ordering.
  A need is met when somebody answers it **and** this plugin may call it:
  a plugin allowed to ask nobody and a plugin forbidden to ask are equally
  unable to work, so both are judged before it is routed, and the operator
  learns at startup rather than mid-turn. `kernel::plugins` and the spawn
  reply report what a plugin is still waiting for; the route table does not,
  because a third state in it would be a special case for every reader.
- **A plugin may learn a verb after it is running.** `hello` is the ordinary
  way to declare verbs, not the only one: a driver that mirrors somebody
  else cannot know what it answers until it has asked them, and asking
  requires being up. `Registrar::tool` — handed to the `on_ready` hook, and
  `Send`, so the asking can take as long as it takes on its own thread —
  adds to the plugin's table and claims the name with the kernel in one
  step. So the kernel reconciles a plugin's routes against what it declares
  rather than toggling them when it first becomes ready; the remote driver
  starts against a peer that is down, answers nothing, and takes its verbs
  on when the peer appears, which is the same waiting state a dependency
  produces because it is the same state.
- **Rendering is event subscription, and so is input.** A renderer is an
  ordinary plugin with zero verbs that subscribes to `model::session::*`;
  several may compose. A front end is the same plugin with a source of
  input: `plugins/tty` reads a terminal and drives `model::send`, and
  `bridge-http`'s presenter does the same for a browser. Either finds the
  model driver by verb and cannot tell whose it is, which is what makes both
  halves of a chat replaceable independently.
- **Credentials stop at the broker.** Plugins get no direct network. Anything
  reaching the outside world invokes `egress::*`; the broker checks the
  allowlist and injects the key, which exists in no other process. That is
  also why the *auth header name* is broker config rather than backend code:
  `x-api-key` and `Authorization: Bearer` are the same mechanism pointed at
  different endpoints, and a backend has no business knowing which.

Deliberately absent, and not to be reintroduced without a concrete pain that
is sharp enough to write a spec from: effect-plan language and interpreter,
consent ceremonies, taint egress gates, resource-class declarations, holdings
ledgers and leases. PortOS also never acts as anyone's MCP server (consuming
MCP later is the opposite direction and is fine).

## Build and Validate

```sh
cargo build --workspace         # first: tests find plugin binaries beside
cargo test --workspace          # their own, and a package with no tests
cargo fmt --all                 # (remote) is not rebuilt by `cargo test`
```

98 tests; all must pass, zero warnings.

The end-to-end tests are the ones that matter and they are hermetic:

- `cli/tests/run.rs` — the full chain through the real CLI binary: the
  launcher starts the list, the terminal front end (its stdin the test's
  pipe) sends the line → modeld → broker (key injection) → scripted
  provider → tool_use → capability-gated invoke → headless Chromium →
  tool_result → streamed text back out. Needs `node` and `npm install` in
  `plugins/browser`; skips with a printed reason otherwise, so check for
  "skipping:" in the output before believing a green run. Also the
  substitution: a different model driver listed in `portos.json` and
  nothing else changed, which is the guarantee a plugin author relies on,
  tested rather than promised.
- `plugins/echo/tests/abi_v2.rs` — plugin ABI conformance.
- `plugins/broker/tests/egress.rs` — allowlist, injection, sanitizing.
- `plugins/modeld/tests/modeld.rs` — the agentic loop, cancellation, and a
  conversation outliving the driver: run a turn, take the driver away, start
  another over the same directory, and check what the *provider* is shown
  next, which is the only thing that proves the history is really there.
- `plugins/fs/tests/fs.rs` and `plugins/shell/tests/shell.rs` — the context
  discipline, measured rather than asserted (`host.meter()` after a big
  result), and the two things easy to get wrong: a walk that ignores
  `.gitignore`, and a timeout that collects only the leader instead of the
  process group.
- `plugins/echo/tests/hotplug.rs` — a running system gaining and losing
  a capability, the residue list after it loses one, a plugin waiting for
  what it needs, and two instances of one driver told apart by name.
- `plugins/echo/tests/form.rs` — the runtime form, measured against
  its control: a grandchild that leaves the process group with `setsid`
  survives teardown in `bare` form and does not in `cgroup` form, a busy
  cgroup refuses `rmdir`, and a cgroup left by a dead run is collected by
  the next one. It also holds the property a driver author relies on — the
  same binary behaves identically under every form — which is where a new
  form gets added. Skips with a reason where cgroup v2 is not writable.
- `plugins/echo/tests/remote.rs` — two nodes in one process, sharing
  nothing but a loopback socket: the far node's verbs arriving as ordinary
  local ones, a driver on each node being two instances that a call tells
  apart by name, the two grant tables that each get a say, and the fact that
  an ephemeral ref crosses untranslated while a handle cannot.
- `plugins/echo/tests/bridge.rs` — the extensibility claim itself: a
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

- `abi` (`portos-abi`): the wire. `wire` holds the protocol as types — the
  envelope is an enum so a malformed frame is refused rather than defaulted,
  and `Payload` is unparsed JSON the kernel forwards without being able to
  read it. `ids` holds `Verb`/`Topic`/`PluginName`/`SubId`, which parse once
  so their accessors are total. Also the frame codec, chunk streaming,
  `Capability`, `Label`, artifact metadata, and `boundary`: a cgroup, a
  boundary whatever is inside cannot leave — here because both sides need
  it; the kernel puts a plugin in one, a plugin puts its own children in one.
- `kernel` (`portos-kernel`): `caps` (authorization, and the join that
  builds the tool surface), `cas` (the data plane), `host` (ABI v2: the two
  axes of a plugin — what it is and how it runs — plus the event bus and
  chunked streaming), `routes` (its implementation of the `router` driver),
  `audit`. Domain vocabulary here is an architectural violation.
- `sdk/rust` (`portos-sdk`): the plugin side, and the place plumbing goes so
  that a plugin can be about its business. `config` (what the launcher said
  this plugin should be, parsed into the plugin's own type), `scope` (running
  a child you can be sure of collecting: pipes, timeout, escalation,
  reclamation), `bulk` (how any verb answers when the result might be
  large). `sdk/js/client.js` is its JS twin.
- `cli` (`portos-cli`): `portos init|put|bundle|meta|get|audit-verify|sessions|run`.
  `run` is the launcher: it starts what `<root>/portos.json` lists, mints
  each entry's grants, reloads on `SIGHUP`, and knows no driver. `init`
  writes the standard set into that file. `sessions` lists stored
  conversations offline from the model driver's index. Links the kernel as
  a library; daemonization is deferred to W4.
- `drivers`: interfaces that regulate a class of behaviour, depended on by
  implementations and callers alike so a shape is never written twice.
  `egress`, `model`, `router`. **These never run.** A new plugin that needs a
  new interface adds a driver here and becomes its first implementation.

  The kernel must not know that a *domain* driver exists — `egress`, `model`
  and whatever comes next are none of its business, and that is where
  extensibility comes from. But a driver is not always a domain: `router` is
  the interface for resolving a name to something you can reach, and the
  kernel is one of its implementations. **The line is what a driver is, not
  who happens to implement it.** The ambition that serves: the kernel should
  be a composition of standard implementations of standard interfaces, so
  that "in-kernel or a plugin?" becomes a deployment choice rather than a
  rewrite. Only two things can never be plugins, because a plugin needs them
  in order to *be* one — the ABI accept loop, and the launcher that starts
  the first plugin.

  A cross-cutting convention with no interface of its own is not a driver —
  it belongs in the SDK, which is where `bulk` lives.
- `plugins`: everything that speaks the ABI.
    - `broker`: the egress chokepoint. Trusted, kernel-spawned.
    - `echo`: the toy plugin the kernel's conformance tests drive; its
      `tests/` are where the kernel's end-to-end behaviour is asserted.
    - `model` (Rust — neutral agentic loop in `core.rs`, wire protocols under
      `backends/`; a backend names a *protocol*, so `anthropic-compatible`
      speaks the Messages API to whatever `base_url`, `path`, `headers` and
      `model` the config names, and `base_url`/`model` have no defaults
      because guessing a vendor is worse than an error).
    - `model-echo`: the second model driver — it says the line back. What
      `portos run` is tested against to prove a plugin is replaceable, and
      the way to see the whole runtime work with no provider, key or network.
    - `tty`: the terminal front end — stdin to `model::send`, session events
      to stdout, `resume` in its config. It takes the terminal's foreground
      so Ctrl-C cancels a turn, and asks the launcher to stop with `SIGTERM`
      when it quits, since a front end has no verb for that and should not.
    - `browser` (JS/Playwright), `fs`, `shell`, `remote` (another node's
      verbs, mirrored here), `bridge-http` (the event plane and the invoke
      path over HTTP/SSE; transport and presentation are separate files on
      purpose), `render-tty` (the renderer reference).
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
