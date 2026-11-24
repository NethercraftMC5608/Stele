//! Architecture-specific CPU registers and operations.
//!
//! A single typed boundary between the kernel core and the CPU: page table
//! root registers, TLB invalidation, user TLS pointer, and CPU halting.
//! Every register access here has a clear semantic name so the caller
//! describes *what* it is doing (switch address space, flush TLB, set thread
//! pointer) rather than the instruction it needs on one architecture.

include!(concat!(env!("OUT_DIR"), "/arch_generated.rs"));
