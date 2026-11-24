//! Host memory, CPU admission, synchronization and unwind boundaries.
#[cfg(all(target_arch = "x86_64", stela_ring0))]
pub mod boot;
pub mod cpu;
pub mod sync;
pub mod unwind;
