//! Identifiers that carry their own rules.
//!
//! Each of these was a bare `String` that every call site re-validated, or
//! more often did not: `verb.rsplit("::").next().unwrap_or(verb)` is a
//! parse that cannot fail because it silently accepts nonsense. Parsing
//! once, at the boundary, makes the accessors below total — `family()` and
//! `short()` return the real thing or the value never existed.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IdError {
    #[error("verb must be `family::verb`, got {0:?}")]
    VerbShape(String),
    #[error("verb segment must match [a-z][a-z0-9_]* and contain no `__`, got {0:?}")]
    VerbSegment(String),
    #[error("tool name must be `family__verb`, got {0:?}")]
    ToolName(String),
    #[error("topic segment must match [a-z0-9_-]+ (last may be `*`), got {0:?}")]
    Topic(String),
    #[error("plugin name must match [a-z0-9_-]+, got {0:?}")]
    PluginName(String),
}

/// A verb name: `family::verb`, both segments lowercase.
///
/// The kernel routes verbs without understanding them, but it does need the
/// family (to find the capability resource) and the short name (to check the
/// grant). Splitting is therefore done once, here, and recorded — `sep` is
/// the byte offset of the `::`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Verb {
    text: String,
    sep: usize,
}

impl Verb {
    pub fn parse(s: &str) -> Result<Verb, IdError> {
        let Some((family, short)) = s.split_once("::") else {
            return Err(IdError::VerbShape(s.to_string()));
        };
        if short.contains("::") {
            return Err(IdError::VerbShape(s.to_string()));
        }
        for seg in [family, short] {
            if !is_verb_segment(seg) {
                return Err(IdError::VerbSegment(seg.to_string()));
            }
        }
        Ok(Verb {
            text: s.to_string(),
            sep: family.len(),
        })
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn family(&self) -> &str {
        &self.text[..self.sep]
    }

    pub fn short(&self) -> &str {
        &self.text[self.sep + 2..]
    }

    /// The capability resource a verb of this family is granted on. The
    /// kernel's convention: subject `plugin:<name>`, resource
    /// `driver:<family>`, verb the short name.
    pub fn resource(&self) -> String {
        format!("driver:{}", self.family())
    }

    /// Build a verb from its family and short name, both validated.
    pub fn new(family: &str, short: &str) -> Result<Verb, IdError> {
        Verb::parse(&format!("{family}::{short}"))
    }

    /// The name this verb takes on a model provider's tool surface.
    /// Providers commonly restrict tool names to `[A-Za-z0-9_-]`, so `::`
    /// maps to `__`. Banning `__` inside a segment is exactly what makes
    /// this a bijection, which is why [`Verb::parse`] rejects it.
    pub fn tool_name(&self) -> String {
        format!("{}__{}", self.family(), self.short())
    }

    pub fn from_tool_name(name: &str) -> Result<Verb, IdError> {
        let Some((family, short)) = name.split_once("__") else {
            return Err(IdError::ToolName(name.to_string()));
        };
        Verb::new(family, short).map_err(|_| IdError::ToolName(name.to_string()))
    }
}

fn is_verb_segment(s: &str) -> bool {
    !s.is_empty()
        && !s.contains("__")
        && s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// An event topic, or a subscription pattern.
///
/// Segments are `::`-joined like verbs, but a topic may have any number of
/// them (`model::session::s1`) and a subscription may end in `*` to match a
/// prefix. Matching lives here rather than as a loose helper so a pattern
/// and a concrete topic can never be compared the wrong way round.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Topic(String);

impl Topic {
    pub fn parse(s: &str) -> Result<Topic, IdError> {
        if s.is_empty() {
            return Err(IdError::Topic(s.to_string()));
        }
        let segs: Vec<&str> = s.split("::").collect();
        let last = segs.len() - 1;
        for (i, seg) in segs.iter().enumerate() {
            let ok = if i == last && *seg == "*" {
                true
            } else {
                !seg.is_empty()
                    && seg.chars().all(|c| {
                        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'
                    })
            };
            if !ok {
                return Err(IdError::Topic(seg.to_string()));
            }
        }
        Ok(Topic(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is a subscription pattern rather than a concrete topic.
    pub fn is_pattern(&self) -> bool {
        self.0.ends_with('*')
    }

    /// Does this pattern match `topic`? A trailing `*` matches any topic
    /// with that prefix; otherwise the two must be equal.
    pub fn matches(&self, topic: &Topic) -> bool {
        match self.0.strip_suffix('*') {
            Some(prefix) => topic.0.starts_with(prefix),
            None => self.0 == topic.0,
        }
    }
}

/// A plugin's self-declared name, which is also its identity in the route
/// table, the capability subject, and artifact origins.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PluginName(String);

impl PluginName {
    pub fn parse(s: &str) -> Result<PluginName, IdError> {
        let ok = !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
        if ok {
            Ok(PluginName(s.to_string()))
        } else {
            Err(IdError::PluginName(s.to_string()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The capability subject this plugin acts as.
    pub fn subject(&self) -> String {
        format!("plugin:{}", self.0)
    }
}

/// A subscription handle. Its own type so it cannot be confused with the
/// other `u64`s on this wire — byte offsets, lengths, budget counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SubId(u64);

impl SubId {
    pub fn new(n: u64) -> SubId {
        SubId(n)
    }
    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for SubId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

macro_rules! string_newtype_boilerplate {
    ($t:ty, $field:expr) => {
        impl fmt::Display for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
        impl fmt::Debug for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({:?})", stringify!($t), self.as_str())
            }
        }
        impl TryFrom<String> for $t {
            type Error = IdError;
            fn try_from(s: String) -> Result<$t, IdError> {
                <$t>::parse(&s)
            }
        }
        impl std::str::FromStr for $t {
            type Err = IdError;
            fn from_str(s: &str) -> Result<$t, IdError> {
                <$t>::parse(s)
            }
        }
        impl From<$t> for String {
            fn from(v: $t) -> String {
                $field(v)
            }
        }
    };
}

string_newtype_boilerplate!(Verb, |v: Verb| v.text);
string_newtype_boilerplate!(Topic, |v: Topic| v.0);
string_newtype_boilerplate!(PluginName, |v: PluginName| v.0);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verb_splits_once_and_totally() {
        let v = Verb::parse("browser::wait_for").unwrap();
        assert_eq!(v.family(), "browser");
        assert_eq!(v.short(), "wait_for");
        assert_eq!(v.resource(), "driver:browser");
        assert_eq!(v.as_str(), "browser::wait_for");
    }

    #[test]
    fn verb_rejects_what_the_old_string_code_accepted() {
        // Each of these used to flow through `split("::").next().unwrap_or()`
        // and come out as a plausible-looking family or verb.
        for bad in ["", "noseparator", "a::b::c", "::b", "a::", "Browser::Open"] {
            assert!(Verb::parse(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn tool_name_is_a_bijection_because_double_underscore_is_banned() {
        assert!(Verb::parse("browser::wait__for").is_err());
        let v = Verb::parse("egress::http_stream").unwrap();
        assert_eq!(v.tool_name(), "egress__http_stream");
        assert_eq!(Verb::from_tool_name("egress__http_stream").unwrap(), v);
    }

    #[test]
    fn topic_patterns_match_by_prefix() {
        let pat = Topic::parse("model::session::*").unwrap();
        assert!(pat.is_pattern());
        assert!(pat.matches(&Topic::parse("model::session::s1").unwrap()));
        assert!(!pat.matches(&Topic::parse("model::other::s1").unwrap()));

        let exact = Topic::parse("egress::log").unwrap();
        assert!(!exact.is_pattern());
        assert!(exact.matches(&Topic::parse("egress::log").unwrap()));
        assert!(!exact.matches(&Topic::parse("egress::log2").unwrap()));
    }

    #[test]
    fn topic_allows_hyphens_and_rejects_empty() {
        assert!(Topic::parse("portos-modeld::egress").is_ok());
        assert!(Topic::parse("").is_err());
        assert!(Topic::parse("a::::b").is_err());
    }

    #[test]
    fn plugin_name_carries_its_capability_subject() {
        let n = PluginName::parse("portos-modeld").unwrap();
        assert_eq!(n.subject(), "plugin:portos-modeld");
        assert!(PluginName::parse("").is_err());
    }
}
