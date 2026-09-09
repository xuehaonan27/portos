//! Resource domain core and executable laws for PortOS.
//!
//! `identity`, `time`, `registry` and `ledger` define the operational aggregate.
//! `ra` and `auth` retain mathematical carriers, including invalid elements.
//! The kernel owns protocol conversion, persistence and physical cleanup.
//! `test_support` retains the monitor, bestiary and attachment exercises;
//! their tests follow the reviewed resource semantics, rather than freezing APIs.

pub mod auth;
pub mod coeffect;
pub mod identity;
pub mod ledger;
pub mod protocol;
pub mod ra;
pub mod registry;
pub mod time;
pub mod verbs;

pub mod cleanup;

/// Deterministic drills and fault injection, absent from normal dependencies.
#[cfg(feature = "test-support")]
pub mod test_support;
