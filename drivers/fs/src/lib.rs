//! The `fs::*` driver interface: a file tree as verbs.
//!
//! Five verbs over one tree, every path relative to the root the instance
//! serves. The discipline that matters more than any of them: **a result
//! the model might not read does not enter its context** — `read` answers
//! with a [`Bulk`], and the counted results of `list`, `glob` and `grep` say
//! when they were cut. `plugins/fs` is the first implementation.

use portos_abi::bulk::Bulk;
use portos_abi::ids::Verb;
use portos_abi::wire::{Payload, ToolMeta};
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
pub fn tools() -> BTreeMap<Verb, ToolMeta> {
    let tool = |verb: &Verb, description: &str, schema: serde_json::Value| {
        (
            verb.clone(),
            ToolMeta {
                description: description.to_string(),
                schema: Payload::of(&schema).ok(),
            },
        )
    };
    BTreeMap::from([
        tool(
            &READ,
            "Read a text file. Paths are relative to the driver's root. A large \
             file comes back as {handle, size, preview} instead of {text}: pass \
             the handle to artifact::read for the rest, or use offset/len here.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer", "description": "byte offset"},
                    "len": {"type": "integer", "description": "byte count"},
                },
                "required": ["path"],
            }),
        ),
        tool(
            &WRITE,
            "Write a file, replacing it if it exists. Give either `content` \
             (text) or `artifact` (a stored handle, copied out without passing \
             through the conversation). Set create_dirs to make missing parent \
             directories.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"},
                    "artifact": {"type": "string", "description": "a handle to write out"},
                    "create_dirs": {"type": "boolean"},
                },
                "required": ["path"],
            }),
        ),
        tool(
            &LIST,
            "List a directory. depth 1 is the directory itself; raise it to \
             descend. Ignored files (.gitignore) are not listed.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "defaults to the root"},
                    "depth": {"type": "integer"},
                },
            }),
        ),
        tool(
            &GLOB,
            "Find files by glob pattern, e.g. `**/*.rs` or `src/**/mod.rs`. \
             Matched against paths relative to the root.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "max": {"type": "integer"},
                },
                "required": ["pattern"],
            }),
        ),
        tool(
            &GREP,
            "Search file contents by regular expression, returning {path, line, \
             text} for each match. Narrow it with a glob. Ignored files and \
             binary files are skipped.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "regular expression"},
                    "glob": {"type": "string", "description": "restrict to matching paths"},
                    "max": {"type": "integer"},
                },
                "required": ["pattern"],
            }),
        ),
    ])
}
