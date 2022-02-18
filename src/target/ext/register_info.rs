//! Access the target’s register info.
use crate::target::{Target};

/// Target Extension - Access the target’s auxiliary vector.
pub trait RegisterInfo: Target {
    /// Get auxiliary vector from the target.
    ///
    /// Return the number of bytes written into `buf` (which may be less than
    /// `length`).
    ///
    /// If `offset` is greater than the length of the underlying data, return
    /// `Ok(0)`.
    fn get_register_info(&self, n: usize) -> Option<&'static str>;
}

define_ext!(RegisterInfoOps, RegisterInfo);
