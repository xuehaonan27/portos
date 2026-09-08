//! Stored timestamps and durations have different units and checked arithmetic.
use crate::ledger::LedgerError;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp(u64);
impl Timestamp {
    pub const ZERO: Self = Self(0);
    pub fn get(self) -> u64 {
        self.0
    }
    pub fn to_sql(self) -> i64 {
        i64::try_from(self.0).expect("validated timestamp")
    }
    pub fn checked_add(self, duration: LeaseDuration) -> Result<Self, LedgerError> {
        self.0
            .checked_add(duration.0)
            .ok_or(LedgerError::OutOfRange)?
            .try_into()
    }
}
impl TryFrom<u64> for Timestamp {
    type Error = LedgerError;
    fn try_from(value: u64) -> Result<Self, Self::Error> {
        i64::try_from(value).map_err(|_| LedgerError::OutOfRange)?;
        Ok(Self(value))
    }
}
impl TryFrom<i64> for Timestamp {
    type Error = LedgerError;
    fn try_from(value: i64) -> Result<Self, Self::Error> {
        u64::try_from(value)
            .map(Self)
            .map_err(|_| LedgerError::OutOfRange)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeaseDuration(u64);
impl LeaseDuration {
    pub fn get(self) -> u64 {
        self.0
    }
}
impl TryFrom<u64> for LeaseDuration {
    type Error = LedgerError;
    fn try_from(value: u64) -> Result<Self, Self::Error> {
        i64::try_from(value).map_err(|_| LedgerError::OutOfRange)?;
        Ok(Self(value))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lease {
    Until(Timestamp),
    ParentBound,
    Unbounded,
}
impl Lease {
    pub fn expires_at(self) -> Option<Timestamp> {
        match self {
            Self::Until(t) => Some(t),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LeaseRequest {
    #[default]
    UseClassDefault,
    For(LeaseDuration),
    Until(Timestamp),
    ParentBound,
    Unbounded,
}
impl LeaseRequest {
    pub(crate) fn resolve(
        self,
        default: Option<LeaseDuration>,
        has_parent: bool,
        now: Timestamp,
    ) -> Result<Lease, LedgerError> {
        match self {
            Self::UseClassDefault => match default {
                Some(duration) => Self::For(duration).resolve(None, has_parent, now),
                None if has_parent => Ok(Lease::ParentBound),
                None => Ok(Lease::Unbounded),
            },
            Self::For(duration) => Ok(Lease::Until(now.checked_add(duration)?)),
            Self::Until(t) => Ok(Lease::Until(t)),
            Self::ParentBound if has_parent => Ok(Lease::ParentBound),
            Self::ParentBound => Err(LedgerError::InvalidLease),
            Self::Unbounded => Ok(Lease::Unbounded),
        }
    }
}
