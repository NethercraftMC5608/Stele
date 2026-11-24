//! The 16550 UART every PC has at I/O port 0x3f8.
//!
//! The same interface as the PL011 driver in uart.rs -- `put`, `get`,
//! `enable_receive`, `drain_receive`, `console()` and the print macros --
//! so everything above the console is architecture-independent and prints
//! through whichever of the two is compiled in.
//!
//! Port I/O rather than MMIO, which is the one genuinely different thing:
//! `in`/`out` instructions on a separate address space, where aarch64 has
//! only memory. QEMU, every PC chipset and most embedded x86 put a 16550 at
//! 0x3f8 and it has been the first serial port since 1981, which is why it
//! is a constant here and a device-tree lookup there.
//!
//! Unlike the PL011, this one *is* initialised. A PL011 under QEMU comes up
//! transmitting; a 16550 does not necessarily -- the line control and FIFO
//! registers hold whatever the firmware left, and on real hardware that is
//! frequently not 8N1 at a useful rate.

use core::fmt;

/// COM1. The port is fixed by forty years of convention rather than read
/// from anywhere: there is no device tree on a PC, and the alternative --
/// walking ACPI's SPCR table -- needs ACPI parsed, which needs a console to
/// report what went wrong with parsing it.
const COM1: u16 = 0x3f8;

// Register offsets from the base port. The first two change meaning when
// DLAB (bit 7 of LCR) is set, which is how the divisor is written.
const DATA: u16 = 0; // receive/transmit buffer, or divisor low with DLAB
const IER: u16 = 1; // interrupt enable, or divisor high with DLAB
const FCR: u16 = 2; // FIFO control (write); interrupt identification (read)
const LCR: u16 = 3; // line control
const MCR: u16 = 4; // modem control
const LSR: u16 = 5; // line status

const LSR_DATA_READY: u8 = 1 << 0;
const LSR_THR_EMPTY: u8 = 1 << 5;

/// # Safety
/// Port I/O talks directly to a device; the caller must mean it.
#[inline]
unsafe fn outb(port: u16, value: u8) {
    core::arch::asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags));
}

/// # Safety
/// As `outb`.
#[inline]
unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    core::arch::asm!("in al, dx", out("al") value, in("dx") port, options(nomem, nostack, preserves_flags));
    value
}

pub struct Uart {
    base: u16,
}

impl Uart {
    pub const fn new(base: u16) -> Self {
        Uart { base }
    }

    pub fn put(&self, byte: u8) {
        unsafe {
            // Bounded, and write anyway when the wait runs out -- for the
            // reason uart.rs gives at length: an unbounded spin turns the
            // one device that reports every other failure into a silent
            // hang, and under a hypervisor each of these reads is a trap.
            let mut spins = 0;
            while inb(self.base + LSR) & LSR_THR_EMPTY == 0 && spins < 1000 {
                spins += 1;
            }
            outb(self.base + DATA, byte);
        }
    }

    /// One byte, or None when nothing has arrived.
    pub fn get(&self) -> Option<u8> {
        unsafe {
            if inb(self.base + LSR) & LSR_DATA_READY == 0 {
                return None;
            }
            Some(inb(self.base + DATA))
        }
    }

    /// Ask to be interrupted when input arrives.
    ///
    /// # Safety
    /// The interrupt controller must be up, or the interrupt has nowhere to
    /// go.
    pub unsafe fn enable_receive(&self) {
        outb(self.base + IER, 0x01); // received-data-available only
    }

    /// Drain whatever has arrived. Returns how many bytes it took.
    ///
    /// No acknowledge step: a 16550 clears its receive interrupt when the
    /// data register is read, so draining *is* the acknowledgement. The
    /// PL011 needs an explicit write to ICR, which is why that driver has
    /// one and this does not.
    pub fn drain_receive(&self, mut sink: impl FnMut(u8)) -> usize {
        let mut n = 0;
        while let Some(b) = self.get() {
            sink(b);
            n += 1;
        }
        n
    }

    /// 8N1 at 115200, FIFOs on, interrupts off.
    ///
    /// # Safety
    /// Called once, before anything else uses the port.
    pub unsafe fn init(&self) {
        outb(self.base + IER, 0x00); // no interrupts while reconfiguring
        outb(self.base + LCR, 0x80); // DLAB: the next two writes are the divisor
        outb(self.base + DATA, 0x01); // 115200 = 115200 / 1
        outb(self.base + IER, 0x00); // divisor high
        outb(self.base + LCR, 0x03); // 8 bits, no parity, one stop -- clears DLAB
        outb(self.base + FCR, 0xc7); // FIFOs on and cleared, 14-byte trigger
        outb(self.base + MCR, 0x0b); // DTR, RTS, and OUT2 -- which gates the
                                     // interrupt line to the PIC on a real PC
    }
}

impl fmt::Write for Uart {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            // A bare \n leaves the cursor in column 0 on a real terminal.
            if b == b'\n' {
                self.put(b'\r');
            }
            self.put(b);
            // The same text on the screen, for machines with no serial port.
            crate::fbcon::putc(b);
        }
        Ok(())
    }
}

/// The console used before anything is initialised, and the one the panic
/// handler uses. No lock, for the reason uart.rs gives: the panic path must
/// not be able to deadlock on one.
pub fn console() -> Uart {
    Uart::new(COM1)
}

/// Which interrupt the UART raises. IRQ 4 for COM1 on every PC, where the
/// aarch64 side reads the number from the device tree.
pub const RX_IRQ: u32 = 4;

pub fn rx_intid() -> u32 {
    RX_IRQ
}

/// # Safety
/// The interrupt controller must be initialised.
pub unsafe fn init_receive(_intid: u32) {
    console().enable_receive();
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let __console = $crate::sync::console_lock();
        let _ = write!($crate::uart_x86::console(), $($arg)*);
        unsafe { $crate::sync::console_unlock(__console) };
    }};
}

#[macro_export]
macro_rules! println {
    ()                 => { $crate::print!("\n") };
    ($($arg:tt)*)      => { $crate::print!("{}\n", format_args!($($arg)*)) };
}

/// `print!` without the console lock, for the panic and fault paths.
#[macro_export]
macro_rules! raw_print {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let _ = write!($crate::uart_x86::console(), $($arg)*);
    }};
}

/// `println!` without the console lock. See `raw_print!`.
#[macro_export]
macro_rules! raw_println {
    ()                 => { $crate::raw_print!("\n") };
    ($($arg:tt)*)      => { $crate::raw_print!("{}\n", format_args!($($arg)*)) };
}
