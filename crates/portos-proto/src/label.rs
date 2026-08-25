//! Dual-lattice labels.
//!
//! Both dimensions are powerset lattices over string atoms:
//!   - `conf`  (confidentiality): atoms like "secret:password". Empty = public.
//!   - `integ` (integrity/taint): atoms like "web:https://example.com".
//!     Empty = trusted.
//! Order is set inclusion; join is union. This is the simplest lattice
//! that satisfies need currently.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Dual-lattice labels.
///
/// TODO: atoms more than strings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Label {
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub conf: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub integ: BTreeSet<String>,
}

impl Label {
    pub fn public_trusted() -> Self {
        Label::default()
    }

    pub fn with_integ(atom: &str) -> Self {
        let mut l = Label::default();
        l.integ.insert(atom.to_string());
        l
    }

    pub fn with_conf(atom: &str) -> Self {
        let mut l = Label::default();
        l.conf.insert(atom.to_string());
        l
    }

    /// Least upper bound.
    /// Currently this means union in both dimensions.
    pub fn join(&self, other: &Label) -> Label {
        Label {
            conf: self.conf.union(&other.conf).cloned().collect(),
            integ: self.integ.union(&other.integ).cloned().collect(),
        }
    }

    /// Partial order: `self ⊑ other` iff inclusion in both dimensions. 
    pub fn leq(&self, other: &Label) -> bool {
        self.conf.is_subset(&other.conf) && self.integ.is_subset(&other.integ)
    }

    pub fn is_public_trusted(&self) -> bool {
        self.conf.is_empty() && self.integ.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_is_union_and_leq_is_inclusion() {
        let a = Label::with_integ("web:a");
        let b = Label::with_conf("secret:resume");
        let j = a.join(&b);
        assert!(a.leq(&j) && b.leq(&j));
        assert!(!j.leq(&a));
        assert!(Label::public_trusted().leq(&a));
    }

    /// Ensure that [`Label`] is a well-defined lattice.
    #[test]
    fn join_commutative_associative_idempotent() {
        let a = Label::with_integ("x");
        let b = Label::with_integ("y");
        let c = Label::with_conf("z");
        assert_eq!(a.join(&b), b.join(&a));
        assert_eq!(a.join(&b).join(&c), a.join(&b.join(&c)));
        assert_eq!(a.join(&a), a);
    }
}