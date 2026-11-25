// SPDX-License-Identifier: GPL-2.0-or-later
//! A text console on the firmware's framebuffer, for machines with no serial
//! port -- which is most PCs. Under UEFI there is no VGA text mode at all, so
//! without this a real machine shows nothing from nk.
//!
//! GRUB hands over the GOP framebuffer in Multiboot2's framebuffer tag (nk's
//! header asks for one). Every byte written to the serial console is also
//! drawn here (`uart_x86::Uart::write_str`), so nk's log, Linux's printk and
//! the panic path all reach the screen.
//!
//! It does not scroll. Scrolling copies the whole framebuffer on every
//! newline -- megabytes of uncached memory -- which on real hardware would
//! make the boot log the slowest thing the machine does. Instead the cursor
//! wraps to the top and the line below it is kept blank, so the newest text
//! is always the line above the gap.
//!
//! Lock-free on purpose: it is reached from the panic path, which must not
//! deadlock. Callers that hold the console lock serialise it in practice.

use crate::font8x16::{FONT, HEIGHT, WIDTH};
use core::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, Debug)]
pub struct Framebuffer {
    pub address: u64,
    pub pitch: u32,
    pub width: u32,
    pub height: u32,
    pub bpp: u8,
}

struct State {
    fb: Framebuffer,
    /// Bytes per pixel, from the pitch: GRUB reports GOP's 32-bit pixels as
    /// 24 bpp (it counts colour bits, not the reserved byte).
    bytes: usize,
    cols: usize,
    rows: usize,
    col: usize,
    row: usize,
}

static READY: AtomicBool = AtomicBool::new(false);
static mut STATE: State = State {
    fb: Framebuffer { address: 0, pitch: 0, width: 0, height: 0, bpp: 0 },
    bytes: 4,
    cols: 0,
    rows: 0,
    col: 0,
    row: 0,
};

const FG: u32 = 0x00c8_c8c8;
const BG: u32 = 0x0000_0000;

/// Start drawing on `fb`: 32-bit pixels (GOP on every machine nk targets,
/// which GRUB may call 24 bpp) or packed 24-bit ones, inside the low 4 GiB
/// identity map, made uncached first. Returns false and stays silent
/// otherwise.
pub fn init(fb: Framebuffer) -> bool {
    let bytes = match fb.bpp {
        32 => 4,
        24 if fb.pitch >= fb.width * 4 => 4,
        24 => 3,
        _ => return false,
    };
    if fb.width < WIDTH as u32 || fb.height < HEIGHT as u32 || fb.pitch < fb.width * bytes as u32 {
        return false;
    }
    let len = fb.pitch as u64 * fb.height as u64;
    let Some(end) = fb.address.checked_add(len) else { return false };
    if end > 1 << 32 || !unsafe { crate::paging::identity_mmio(fb.address, end) } {
        return false;
    }
    unsafe {
        let s = &mut *(&raw mut STATE);
        s.fb = fb;
        s.bytes = bytes;
        s.cols = fb.width as usize / WIDTH;
        s.rows = fb.height as usize / HEIGHT;
        s.col = 0;
        s.row = 0;
        clear(s, 0, s.rows);
    }
    READY.store(true, Ordering::Release);
    true
}

pub fn active() -> bool {
    READY.load(Ordering::Acquire)
}

pub fn size() -> Option<(usize, usize)> {
    active().then(|| unsafe {
        let s = &*(&raw const STATE);
        (s.cols, s.rows)
    })
}

/// One pixel at (`x`, `y`), in whichever pixel size the framebuffer uses.
unsafe fn put(s: &State, x: usize, y: usize, rgb: u32) {
    let at = s.fb.address + y as u64 * s.fb.pitch as u64 + (x * s.bytes) as u64;
    if s.bytes == 4 {
        core::ptr::write_volatile(at as *mut u32, rgb);
    } else {
        let p = at as *mut u8;
        core::ptr::write_volatile(p, rgb as u8);
        core::ptr::write_volatile(p.add(1), (rgb >> 8) as u8);
        core::ptr::write_volatile(p.add(2), (rgb >> 16) as u8);
    }
}

unsafe fn clear(s: &State, first_row: usize, count: usize) {
    for r in first_row..(first_row + count).min(s.rows) {
        for y in r * HEIGHT..(r + 1) * HEIGHT {
            for x in 0..s.cols * WIDTH {
                put(s, x, y, BG);
            }
        }
    }
}

unsafe fn glyph(s: &State, ch: u8, col: usize, row: usize) {
    let bits = &FONT[ch as usize * HEIGHT..ch as usize * HEIGHT + HEIGHT];
    for (dy, &b) in bits.iter().enumerate() {
        for dx in 0..WIDTH {
            let on = b & (0x80 >> dx) != 0;
            put(s, col * WIDTH + dx, row * HEIGHT + dy, if on { FG } else { BG });
        }
    }
}

unsafe fn newline(s: &mut State) {
    s.col = 0;
    s.row = (s.row + 1) % s.rows;
    // Keep the line after the cursor blank: it marks where the newest text
    // ends once the console has wrapped.
    clear(s, s.row, 1);
    clear(s, (s.row + 1) % s.rows, 1);
}

/// Draw one byte of console output. `\r` is ignored (the serial path adds
/// it before every `\n`), `\n` starts a new line, tabs go to the next
/// multiple of eight, and anything outside printable ASCII shows as '?'.
pub fn putc(b: u8) {
    if !active() {
        return;
    }
    unsafe {
        let s = &mut *(&raw mut STATE);
        match b {
            b'\r' => {}
            b'\n' => newline(s),
            b'\t' => {
                s.col = (s.col + 8) & !7;
                if s.col >= s.cols {
                    newline(s);
                }
            }
            _ => {
                let ch = if (0x20..0x7f).contains(&b) { b } else { b'?' };
                glyph(s, ch, s.col, s.row);
                s.col += 1;
                if s.col >= s.cols {
                    newline(s);
                }
            }
        }
    }
}
