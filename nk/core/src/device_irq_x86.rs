// SPDX-License-Identifier: GPL-2.0-or-later
//! Device vectors reserved apart from exceptions, PIC, timers and IPIs.
//! Handlers must be bounded/nonblocking; Linux delivery is deferred to the
//! existing LKL IRQ pump. No allocation or LKL CPU-lock acquisition in a trap.
use crate::irq_x86::{DEVICE_FIRST, DEVICE_LAST};
use core::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};

const COUNT: usize = (DEVICE_LAST - DEVICE_FIRST + 1) as usize;
static NEXT: AtomicUsize = AtomicUsize::new(0);
static HANDLERS: [AtomicUsize; COUNT] = [const { AtomicUsize::new(0) }; COUNT];
static LKL: [AtomicI32; COUNT] = [const { AtomicI32::new(-1) }; COUNT];
static HITS: [AtomicU64; COUNT] = [const { AtomicU64::new(0) }; COUNT];

/// Allocate a permanent vector. Source teardown must disable/mask hardware
/// before retiring it. Retired vectors are not reused while stale messages
/// could still be in flight; device detach/quiescence is not implemented.
pub fn allocate(lkl: Option<i32>, handler: Option<fn()>) -> Option<u8> {
    if lkl.is_some_and(|n| !(0..128).contains(&n)) {
        return None;
    }
    let slot = NEXT
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
            (n < COUNT).then_some(n + 1)
        })
        .ok()?;
    LKL[slot].store(lkl.unwrap_or(-1), Ordering::Relaxed);
    HANDLERS[slot].store(handler.map_or(1, |h| h as usize), Ordering::Release);
    Some(DEVICE_FIRST + slot as u8)
}
pub fn count(vector: u8) -> u64 {
    if !(DEVICE_FIRST..=DEVICE_LAST).contains(&vector) {
        return 0;
    }
    HITS[(vector - DEVICE_FIRST) as usize].load(Ordering::Acquire)
}
pub fn retire(vector: u8) {
    if !(DEVICE_FIRST..=DEVICE_LAST).contains(&vector) {
        return;
    }
    let slot = (vector - DEVICE_FIRST) as usize;
    LKL[slot].store(-1, Ordering::Release);
    HANDLERS[slot].store(1, Ordering::Release);
}
pub fn dispatch(vector: u8) {
    let slot = (vector - DEVICE_FIRST) as usize;
    let handler = HANDLERS[slot].load(Ordering::Acquire);
    if handler == 0 {
        return;
    }
    if handler != 1 {
        // Only static fn() pointers from allocate are published here.
        let callback: fn() = unsafe { core::mem::transmute(handler) };
        callback();
    }
    HITS[slot].fetch_add(1, Ordering::Release);
    #[cfg(nk_lkl)]
    {
        let irq = LKL[slot].load(Ordering::Acquire);
        if irq >= 0 {
            crate::lklirq::raise(irq);
        }
    }
}
