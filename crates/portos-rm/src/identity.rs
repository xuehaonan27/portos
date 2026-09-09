//! Domain names are distinct even when their wire representations are strings.
//! Construction names an object; it does not establish its existence or authority.
use std::fmt;

macro_rules! name {
    ($($name:ident),+ $(,)?) => {$(
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);
        impl $name {
            pub fn new(value: impl Into<String>) -> Self { Self(value.into()) }
            pub fn as_str(&self) -> &str { &self.0 }
        }
        impl From<&str> for $name { fn from(value: &str) -> Self { Self::new(value) } }
        impl From<String> for $name { fn from(value: String) -> Self { Self::new(value) } }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { self.0.fmt(f) }
        }
    )+};
}
name!(
    SubjectId,
    ClassId,
    VerbId,
    InstanceId,
    Generation,
    EffectClass,
    AccountId,
    AttachmentId
);

/// Zero is a valid historical ID. Values must round-trip through SQLite INTEGER.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HoldingId(u64);
impl HoldingId {
    pub fn get(self) -> u64 {
        self.0
    }
    pub fn to_sql(self) -> i64 {
        i64::try_from(self.0).expect("validated ID")
    }
}
impl TryFrom<u64> for HoldingId {
    type Error = crate::ledger::LedgerError;
    fn try_from(value: u64) -> Result<Self, Self::Error> {
        i64::try_from(value).map_err(|_| Self::Error::OutOfRange)?;
        Ok(Self(value))
    }
}
impl TryFrom<i64> for HoldingId {
    type Error = crate::ledger::LedgerError;
    fn try_from(value: i64) -> Result<Self, Self::Error> {
        u64::try_from(value)
            .map(Self)
            .map_err(|_| Self::Error::OutOfRange)
    }
}
impl fmt::Display for HoldingId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A reference, not a liveness witness. Every transition rechecks the generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoldingHandle {
    id: HoldingId,
    generation: Generation,
}
impl HoldingHandle {
    pub fn new(id: HoldingId, generation: Generation) -> Self {
        Self { id, generation }
    }
    pub fn id(&self) -> HoldingId {
        self.id
    }
    pub fn generation(&self) -> &Generation {
        &self.generation
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResourceKey {
    class: ClassId,
    instance: InstanceId,
}
impl ResourceKey {
    pub fn new(class: ClassId, instance: InstanceId) -> Self {
        Self { class, instance }
    }
    pub fn class(&self) -> &ClassId {
        &self.class
    }
    pub fn instance(&self) -> &InstanceId {
        &self.instance
    }
}

/// Stable pool identity; fields are never recovered by splitting its display name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PoolId(ResourceKey);
impl PoolId {
    pub(crate) fn new(key: ResourceKey) -> Self {
        Self(key)
    }
    pub fn key(&self) -> &ResourceKey {
        &self.0
    }
}

#[derive(Clone, Debug)]
pub struct SpendRequest {
    pub account: AccountId,
    pub effect: EffectClass,
    pub amount: u64,
}

/// ```compile_fail
/// use portos_rm::identity::{ResourceKey, SubjectId, InstanceId};
/// ResourceKey::new(SubjectId::new("owner"), InstanceId::new("instance"));
/// ```
const _: () = ();
