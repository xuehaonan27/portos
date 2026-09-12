//! The `router` driver: resolving a name to something you can reach.
//!
//! This interface exists because the same decision was being made in several
//! places, differently each time — *given a name, who answers it?* The kernel
//! made it for verbs arriving from plugins; the SDK made it for verbs
//! arriving from the kernel; and where neither fitted, the answer was an
//! `if` before the general path (`kernel::*`, `artifact::read`) or a second
//! table built outside (`remote`). Three workarounds that each made the
//! missing interface look less needed.
//!
//! What it regulates is **laws, not storage**. An implementation keeps its
//! table however it likes; what it may not do is disagree about these:
//!
//! 1. **A verb names a driver's verb, not an answerer.** `browser::open` is
//!    what the browser interface calls opening; any number of instances may
//!    answer it — two browsers here, one on another node. What is unique is
//!    the pair: one target may not register one verb twice, and that is the
//!    only [`Conflict`].
//!
//!    This replaced a law that said a name, and a whole family, has exactly
//!    one answerer. That law made the name do two jobs — say which interface
//!    and say which instance — and the second job leaked out as encoded
//!    names (`mac_browser::open`) that nothing could parse back. Which
//!    instance is a routing decision, so it lives here and not in the name.
//! 2. **Resolution yields exactly one answerer, or an error.** With one
//!    answerer the verb alone resolves; with several the caller names the
//!    one it wants (`at`), and without a name that is [`Miss::Ambiguous`] —
//!    an error, not a choice made on the caller's behalf. The selector is
//!    optional because a choice with one option is not a choice.
//! 3. **Resolution yields the name the *target* knows.** Usually the same
//!    name; not always, because a route may cross a boundary where the far
//!    side calls it something else. Callers must use what resolution gave
//!    them, never the name they looked up.
//! 4. **A miss is an error, never a default.** There is no fallback answerer.
//!
//! An implementation whose callers have no way to name a target — the SDK's
//! handlers are found by an id no caller holds — must keep one target per
//! verb, or every call would be ambiguous. That is not a fifth law; it is
//! law 2 applied to a table nobody can select from.
//!
//! ## What a target is, is the implementation's business
//!
//! [`Router::Target`] is an associated type on purpose. The kernel's targets
//! are a plugin process or the kernel itself; the SDK's are its own handlers.
//! They have nothing in common and should not be forced into a shared enum —
//! an earlier draft of this did exactly that, and the strain of making one
//! type mean both was the clue that it was an implementation pretending to be
//! an interface.
//!
//! A target is a **name for an answerer, not the answerer itself.** Reaching
//! it is a separate step, and separating them is the whole point: it is what
//! lets the same table route to a process here, a function in this process,
//! or something across a link, without the table knowing the difference.
//!
//! ## What this deliberately does not cover
//!
//! Fan-out — one name, zero or more answerers, no reply — is a different set
//! of laws, and the event bus obeys those instead. Putting both behind one
//! interface would need a branch on arity at the first call site, which is
//! the kind of special case that says the shape is wrong. There is exactly
//! one fan-out implementation today, and one implementation is a guess at an
//! interface, so there is no `Fanout` here yet.

use portos_abi::ids::Verb;
use portos_abi::wire::ToolMeta;

/// The one thing registration refuses: the same target, the same verb, twice.
#[derive(Debug, thiserror::Error)]
#[error("already registered for this answerer: {0}")]
pub struct Conflict(pub Verb);

/// Why a verb did not resolve. Both are the caller's to report.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Miss {
    #[error("no route")]
    NoRoute,
    /// Several answer it and none was named.
    #[error("several answerers and none named")]
    Ambiguous,
}

/// What resolution produces: who answers, and what *they* call it.
#[derive(Debug, PartialEq)]
pub struct Resolved<'a, T> {
    pub target: &'a T,
    /// The name to use from here on. Equal to the looked-up name unless the
    /// route crosses into somewhere that names it differently.
    pub name: Verb,
}

pub trait Router {
    /// A name for an answerer. Not the answerer: reaching it is a separate
    /// step, and one this interface says nothing about.
    type Target: PartialEq;

    /// Who answers `verb`, and what they call it. `at` names the answerer
    /// when the caller already knows which one it wants; without it, the
    /// verb must have exactly one.
    fn resolve(
        &self,
        verb: &Verb,
        at: Option<&Self::Target>,
    ) -> Result<Resolved<'_, Self::Target>, Miss>;

    /// Everyone who answers `verb`, with what each says about it. Empty is a
    /// verb nobody answers.
    fn answerers(&self, verb: &Verb) -> Vec<(&Self::Target, &ToolMeta)>;

    /// Register an answerer for a verb.
    ///
    /// `meta` rides along because in PortOS the route table is also where a
    /// name says what it is: a granted verb joined with what its answerer
    /// advertised *is* the tool definition. Keeping it in a second table
    /// keyed the same way is the two-tables mistake, and both implementations
    /// would make it.
    fn add(&mut self, verb: Verb, target: Self::Target, meta: ToolMeta) -> Result<(), Conflict>;

    /// Give up every registration `f` selects. Returns how many.
    fn remove_where(&mut self, f: &dyn Fn(&Verb, &Self::Target) -> bool) -> usize;

    fn verbs(&self) -> Vec<Verb>;
}
