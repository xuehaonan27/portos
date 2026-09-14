//! The `fs::*` driver interface: a file tree as verbs.
//!
//! Five verbs over one tree, every path relative to the root the instance
//! serves. The discipline that matters more than any of them: **a result
//! the model might not read does not enter its context** — `read` answers
//! with a [`Bulk`], and the counted results of `list`, `glob` and `grep` say
//! when they were cut. `plugins/fs` is the first implementation.

use portos_abi::bulk::Bulk;
use portos_abi::driver::Driver;
use portos_abi::ids::Verb;
use portos_abi::wire::ToolMeta;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::LazyLock;

pub static READ: LazyLock<Verb> = LazyLock::new(|| Verb::parse("fs::read").expect("constant verb"));
pub static WRITE: LazyLock<Verb> =
    LazyLock::new(|| Verb::parse("fs::write").expect("constant verb"));
pub static LIST: LazyLock<Verb> = LazyLock::new(|| Verb::parse("fs::list").expect("constant verb"));
pub static GLOB: LazyLock<Verb> = LazyLock::new(|| Verb::parse("fs::glob").expect("constant verb"));
pub static GREP: LazyLock<Verb> = LazyLock::new(|| Verb::parse("fs::grep").expect("constant verb"));

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReadArgs {
    pub path: String,
    #[serde(default)]
    pub offset: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub len: Option<u64>,
}

/// `{text}` when small, `{handle, size, preview}` when not.
pub type ReadReply = Bulk;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WriteArgs {
    pub path: String,
    /// The text to write. Exactly one of this and `artifact`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// A stored artifact to write out instead — the way something large gets
    /// *out* of the store and into the tree without passing through the
    /// conversation on the way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    #[serde(default)]
    pub create_dirs: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WriteReply {
    pub path: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ListArgs {
    /// Defaults to the root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// 1 means this directory only, which is what a caller almost always
    /// means by "list".
    #[serde(default = "one")]
    pub depth: usize,
}

fn one() -> usize {
    1
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Dir,
    File,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub path: String,
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Listing {
    pub entries: Vec<Entry>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GlobArgs {
    pub pattern: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Paths {
    pub paths: Vec<String>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GrepArgs {
    /// A regular expression.
    pub pattern: String,
    /// Restrict to paths matching this glob, e.g. `**/*.rs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Match {
    pub path: String,
    pub line: usize,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Matches {
    pub matches: Vec<Match>,
    pub truncated: bool,
}

/// What each verb says about itself: the tool the model is shown. Stated
/// here so an implementation advertises the interface rather than its own
/// wording of it.
/// The interface itself; the crate's constants and types are a typed view.
pub const DRIVER_JSON: &str = include_str!("../driver.json");
pub static DRIVER: LazyLock<Driver> =
    LazyLock::new(|| Driver::parse(DRIVER_JSON).expect("drivers/fs/driver.json is well-formed"));

/// What each verb says about itself, as an implementation advertises it.
pub fn tools() -> BTreeMap<Verb, ToolMeta> {
    DRIVER.tools()
}

#[cfg(test)]
mod driver_document {
    use super::*;

    /// The document is this interface: exactly the verbs named here, all of
    /// this driver, described.
    #[test]
    fn names_exactly_these_verbs() {
        let named: Vec<&Verb> = vec![&READ, &WRITE, &LIST, &GLOB, &GREP];
        assert_eq!(DRIVER.driver, "fs");
        assert_eq!(tools().len(), named.len());
        for v in named {
            assert!(DRIVER.spec(v).is_some(), "{v} is in driver.json");
        }
    }
}
