//! Executable mappings for translated native code, one implementation per host.

#[cfg(all(target_arch = "x86_64", stela_ring0))]
mod freestanding;
#[cfg(all(target_arch = "x86_64", any(target_os = "macos", target_os = "linux")))]
mod posix;
#[cfg(not(any(
    all(target_arch = "x86_64", any(target_os = "macos", target_os = "linux")),
    all(target_arch = "x86_64", stela_ring0)
)))]
mod unsupported;

#[cfg(all(target_arch = "x86_64", stela_ring0))]
pub use freestanding::*;
#[cfg(all(target_arch = "x86_64", any(target_os = "macos", target_os = "linux")))]
pub use posix::*;
#[cfg(not(any(
    all(target_arch = "x86_64", any(target_os = "macos", target_os = "linux")),
    all(target_arch = "x86_64", stela_ring0)
)))]
pub use unsupported::*;
