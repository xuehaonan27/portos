//! The `browser::*` driver interface: a watchable browser.
//!
//! The model works from an *element table* — every interactive element on
//! the page, each with a short-lived `ref` — and acts by ref. A ref is a
//! driver-session-local name, rebuilt on every snapshot and never a kernel
//! handle: the two-layer naming rule. A screenshot is an artifact, never
//! bytes in a conversation.
//!
//! The interface is `driver.json` beside this file, like every driver's;
//! here it also carries reply schemas, because the first implementation
//! (`plugins/browser`) is JavaScript and has no types to be held to
//! instead. One document, two readers, and a conformance test that what the
//! plugin advertises is what this says.

use portos_abi::driver::Driver;
use portos_abi::ids::Verb;
use portos_abi::wire::ToolMeta;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::LazyLock;

macro_rules! verb {
    ($name:ident, $s:literal) => {
        pub static $name: LazyLock<Verb> =
            LazyLock::new(|| Verb::parse($s).expect("constant verb"));
    };
}
verb!(OPEN, "browser::open");
verb!(NAVIGATE, "browser::navigate");
verb!(SNAPSHOT, "browser::snapshot");
verb!(CLICK, "browser::click");
verb!(TYPE, "browser::type");
verb!(WAIT_FOR, "browser::wait_for");
verb!(SCREENSHOT, "browser::screenshot");
verb!(LOGIN_PASSTHROUGH, "browser::login_passthrough");
verb!(RESUME, "browser::resume");
verb!(CLOSE, "browser::close");

/// The tool descriptions, as the JS implementation reads them.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OpenArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NavigateArgs {
    pub url: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SnapshotArgs {
    #[serde(default)]
    pub with_bbox: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClickArgs {
    pub r#ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TypeArgs {
    pub r#ref: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_name: Option<String>,
    #[serde(default)]
    pub submit: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WaitForArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms: Option<f64>,
    #[serde(default)]
    pub network_idle: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WaitForReply {
    pub ok: bool,
    pub waited: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ScreenshotArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScreenshotReply {
    /// The stored image.
    pub handle: String,
    pub size: u64,
    pub r#type: String,
    /// Where it was written, for a person looking at a headful window.
    pub path: String,
}

/// One interactive element. `bbox` is present only when asked for.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Element {
    pub r#ref: String,
    pub role: String,
    pub name: String,
    pub editable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bbox: Option<BBox>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct BBox {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// The element table.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub snapshot_id: u64,
    pub url: String,
    pub title: String,
    /// How many there were before the table was capped.
    pub element_count: usize,
    pub elements: Vec<Element>,
    /// Set when `expect_name` did not match what is under the ref now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_warning: Option<String>,
}

/// What the page verbs answer with: the table when it fits in context, a
/// stored handle with a preview when it does not — the same line every
/// driver draws (`portos_abi::bulk`), for a structured result.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PageReply {
    Stored {
        handle: String,
        size: u64,
        preview: String,
    },
    Page(Snapshot),
}

/// The interface itself; the crate's constants and types are a typed view.
/// The JavaScript implementation reads the same file.
pub const DRIVER_JSON: &str = include_str!("../driver.json");
pub static DRIVER: LazyLock<Driver> = LazyLock::new(|| {
    Driver::parse(DRIVER_JSON).expect("drivers/browser/driver.json is well-formed")
});

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
        let named: Vec<&Verb> = vec![
            &OPEN,
            &NAVIGATE,
            &SNAPSHOT,
            &CLICK,
            &TYPE,
            &WAIT_FOR,
            &SCREENSHOT,
            &LOGIN_PASSTHROUGH,
            &RESUME,
            &CLOSE,
        ];
        assert_eq!(DRIVER.driver, "browser");
        assert_eq!(tools().len(), named.len());
        for v in named {
            assert!(DRIVER.spec(v).is_some(), "{v} is in driver.json");
        }
    }
}
