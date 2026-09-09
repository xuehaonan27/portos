//! Mutable exercises and malformed-input fixtures. This module is available
//! only with `test-support`; it is not part of a normal kernel dependency.
#[cfg(feature = "attach")]
pub mod attach;
pub mod bestiary;
pub mod declarations;
pub mod monitor;
pub mod teardown;

/// Deterministic pseudo-random generator for the law tests.
pub struct Lcg(pub u64);
impl Lcg {
    pub fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}
