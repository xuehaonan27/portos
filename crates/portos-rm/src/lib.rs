//! Resource domain core and executable laws for PortOS.
//!
//! `identity`, `time`, `registry` and `ledger` define the operational aggregate.
//! `ra` and `auth` retain mathematical carriers, including invalid elements.
//! The kernel owns protocol conversion, persistence and physical cleanup.
//! `monitor`, `bestiary` and optional `attach` retain development exercises;
//! their tests follow the reviewed resource semantics, rather than freezing APIs.

#[cfg(feature = "attach")]
pub mod attach;
pub mod auth;
pub mod bestiary;
pub mod coeffect;
pub mod identity;
pub mod ledger;
pub mod monitor;
pub mod protocol;
pub mod ra;
pub mod registry;
pub mod teardown;
pub mod time;
pub mod verbs;
