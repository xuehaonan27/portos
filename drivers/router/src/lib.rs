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
//! 1. **A name has exactly one answerer.** Registering a name that is taken
//!    is a [`Conflict`], not a silent overwrite and not a first-match-wins.
//!    This is also what makes a reserved name ordinary: whoever registers
//!    first has it, and the second registration is refused with a reason.
//! 1b. **A *family* has one answerer.** Not a separate rule so much as the
//!    consequence of what a family is: the capability resource is
//!    `driver:<family>`, so a family answered by two targets makes one grant
//!    statement mean two different things to whoever wrote it.
//!
//!    Whether two targets *are* the same answerer is the implementation's
//!    to decide — only it knows what its targets name. The kernel's
//!    `kernel::spawn` and `kernel::stop` are different targets and one
//!    answerer.
//!
//!    This law was discovered by deleting a special case. The kernel used to
//!    refuse a plugin that claimed *any* `kernel::*` verb, which looked like
//!    a privilege of the kernel's; it was not. Nothing stopped two ordinary
//!    plugins from splitting `browser::*` between them and quietly doing the
//!    same damage to `driver:browser`. The special case was a general law
//!    wearing a disguise.
//! 2. **Resolution yields the name the *target* knows.** Usually the same
//!    name; not always, because a route may cross a boundary where the far
//!    side calls it something else. Callers must use what resolution gave
//!    them, never the name they looked up.
//! 3. **A miss is an error, never a default.** There is no fallback answerer.
//!
//! ## What a target is, is the implementation's business
//!
//! [`Router::Target`] is an associated type on purpose. The kernel's targets
//! are a plugin process or one of its own built-in verbs; the SDK's are its
//! own handlers. They have nothing in common and should not be forced into a
//! shared enum — an earlier draft of this did exactly that, and the strain of
//! making one type mean both was the clue that it was an implementation
//! pretending to be an interface.
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

#[derive(Debug, thiserror::Error)]
pub enum Conflict {
    #[error("verb already routed: {0}")]
    Verb(Verb),
    /// The family is answered by something else. Refused because the
    /// capability resource is the family, not the verb.
    #[error("the `{family}` family is answered by something else already: {verb}")]
    Family { verb: Verb, family: String },
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
    type Target;

    /// Who answers `verb`, and what they call it. `None` is a miss, and a
    /// miss is an error for the caller to report — not a place to put a
    /// default.
    fn resolve(&self, verb: &Verb) -> Option<Resolved<'_, Self::Target>>;

    /// Claim a name. Refuses one that is taken, which is how a name stays
    /// reserved without anybody writing a check for it.
    ///
    /// `meta` rides along because in PortOS the route table is also where a
    /// name says what it is: a granted verb joined with what its answerer
    /// advertised *is* the tool definition. Keeping it in a second table
    /// keyed the same way is the two-tables mistake, and both implementations
    /// would make it.
    fn add(&mut self, verb: Verb, target: Self::Target, meta: ToolMeta) -> Result<(), Conflict>;

    /// What this name says about itself, for whoever is entitled to call it.
    fn meta(&self, verb: &Verb) -> Option<&ToolMeta>;

    /// Give up every name held by `f`. Returns how many.
    fn remove_where(&mut self, f: &dyn Fn(&Verb, &Self::Target) -> bool) -> usize;

    fn verbs(&self) -> Vec<Verb>;
}
