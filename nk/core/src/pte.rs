//! Architecture-specific page table entry definitions.
//!
//! The kernel uses a unified PTE abstraction that maps to the appropriate
//! architecture-specific implementation.

#[cfg(target_arch = "aarch64")]
pub use crate::pte_arm::*;

#[cfg(target_arch = "x86_64")]
pub use crate::pte_x86::*;