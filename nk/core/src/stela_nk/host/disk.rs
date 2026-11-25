// SPDX-License-Identifier: GPL-2.0-or-later
//! ANS2 backing for the opt-in second NVMe namespace. The guest's dedicated
//! AP submits owned buffers. CPU 0 executes every Linux syscall.

use alloc::{string::String, sync::Arc, vec, vec::Vec};
use core::cell::UnsafeCell;
use stela_driver_base::block::{BlockDevice, BlockError, Features, Identity, Transport};

const OPENAT: i64 = 56;
const CLOSE: i64 = 57;
const IOCTL: i64 = 29;
const FSYNC: i64 = 82;
const PREAD: i64 = 67;
const PWRITE: i64 = 68;
const BLKGETSIZE64: i64 = 0x8008_1272;
const BLOCK_BYTES: u32 = 4096;
const MAX_TRANSFER: usize = 1 << 20;

enum Operation {
    Read,
    Write,
    Flush,
}

struct Request {
    operation: Operation,
    offset: u64,
    data: Vec<u8>,
}

enum State {
    Idle,
    Request(Request),
    InFlight,
    Complete(Result<Vec<u8>, BlockError>),
}

struct Mailbox {
    lock: crate::sync::Spinlock,
    state: UnsafeCell<State>,
}

// The lock guards every access to state. A request owns its full buffer.
unsafe impl Sync for Mailbox {}

impl Mailbox {
    fn with<R>(&self, access: impl FnOnce(&mut State) -> R) -> R {
        let flags = self.lock.lock();
        let result = access(unsafe { &mut *self.state.get() });
        unsafe { self.lock.unlock(flags) };
        result
    }

    fn submit(&self, request: Request) -> Result<Vec<u8>, BlockError> {
        self.with(|state| {
            assert!(
                matches!(state, State::Idle),
                "one ANS2 submission at a time"
            );
            *state = State::Request(request);
        });
        loop {
            if let Some(result) = self.with(|state| {
                if matches!(state, State::Complete(_)) {
                    let State::Complete(result) = core::mem::replace(state, State::Idle) else {
                        unreachable!()
                    };
                    Some(result)
                } else {
                    None
                }
            }) {
                return result;
            }
            // The AP runs the board with IF clear. A completion sent between
            // the state check and STI remains pending and wakes HLT.
            unsafe { core::arch::asm!("sti", "hlt", "cli", options(nomem, nostack)) };
        }
    }
}

pub(super) struct LinuxDisk {
    identity: Identity,
    mailbox: Arc<Mailbox>,
}

impl BlockDevice for LinuxDisk {
    fn identity(&self) -> &Identity {
        &self.identity
    }

    fn read(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), BlockError> {
        self.identity.check(lba, buffer.len())?;
        let data = self.mailbox.submit(Request {
            operation: Operation::Read,
            offset: lba * u64::from(BLOCK_BYTES),
            data: vec![0; buffer.len()],
        })?;
        buffer.copy_from_slice(&data);
        Ok(())
    }

    fn write(&mut self, lba: u64, data: &[u8]) -> Result<(), BlockError> {
        self.identity.check_write(lba, data.len())?;
        self.mailbox.submit(Request {
            operation: Operation::Write,
            offset: lba * u64::from(BLOCK_BYTES),
            data: data.to_vec(),
        })?;
        Ok(())
    }

    fn flush_cache(&mut self) -> Result<(), BlockError> {
        self.mailbox.submit(Request {
            operation: Operation::Flush,
            offset: 0,
            data: Vec::new(),
        })?;
        Ok(())
    }
}

/// The Linux descriptor and its service must stay on CPU 0.
pub(super) struct Worker {
    fd: i64,
    mailbox: Arc<Mailbox>,
    reads: u64,
    writes: u64,
    flushes: u64,
}

impl Worker {
    pub(super) fn open() -> Result<(Self, LinuxDisk), String> {
        assert_eq!(crate::arch::cpu_id(), 0);
        // O_RDWR | O_EXCL. The first namespace remains the read-only bundle.
        let fd = crate::lkl::syscall(
            OPENAT,
            [
                -100,
                c"/stele-dev/nvme0n2".as_ptr() as i64,
                2 | 128,
                0,
                0,
                0,
            ],
        );
        if fd < 0 {
            return Err(alloc::format!("opening /dev/nvme0n2: errno {}", -fd));
        }
        let mut bytes = 0u64;
        let rc = crate::lkl::syscall(IOCTL, [fd, BLKGETSIZE64, &raw mut bytes as i64, 0, 0, 0]);
        if rc < 0
            || bytes == 0
            || bytes > i64::MAX as u64
            || !bytes.is_multiple_of(u64::from(BLOCK_BYTES))
        {
            crate::lkl::syscall(CLOSE, [fd, 0, 0, 0, 0, 0]);
            return Err(alloc::format!(
                "/dev/nvme0n2: unusable capacity ({bytes} bytes, ioctl {rc})"
            ));
        }
        let mailbox = Arc::new(Mailbox {
            lock: crate::sync::Spinlock::named("stele-ssd"),
            state: UnsafeCell::new(State::Idle),
        });
        let disk = LinuxDisk {
            identity: Identity {
                transport: Transport::Nvme,
                location: "/dev/nvme0n2".into(),
                model: "nk LKL-backed SSD".into(),
                serial: "NKSSDNVME02".into(),
                blocks: bytes / u64::from(BLOCK_BYTES),
                block_size: BLOCK_BYTES,
                read_only: false,
                features: Features {
                    write_cache: true,
                    max_transfer: MAX_TRANSFER,
                    ..Features::default()
                },
            },
            mailbox: Arc::clone(&mailbox),
        };
        crate::println!("STELA_SPTM: guest SSD backed by /dev/nvme0n2 ({bytes} bytes)");
        Ok((
            Self {
                fd,
                mailbox,
                reads: 0,
                writes: 0,
                flushes: 0,
            },
            disk,
        ))
    }

    pub(super) fn service(&mut self, guest_cpu: usize) -> bool {
        assert_eq!(crate::arch::cpu_id(), 0);
        let Some(mut request) = self.mailbox.with(|state| {
            if matches!(state, State::Request(_)) {
                let State::Request(request) = core::mem::replace(state, State::InFlight) else {
                    unreachable!()
                };
                Some(request)
            } else {
                None
            }
        }) else {
            return false;
        };
        let result = self.execute(&mut request).map(|()| request.data);
        self.mailbox.with(|state| *state = State::Complete(result));
        super::irq::kick(guest_cpu);
        true
    }

    fn execute(&mut self, request: &mut Request) -> Result<(), BlockError> {
        let (syscall, completed, name) = match request.operation {
            Operation::Read => (PREAD, &mut self.reads, "read"),
            Operation::Write => (PWRITE, &mut self.writes, "write"),
            Operation::Flush => (FSYNC, &mut self.flushes, "flush"),
        };
        if syscall == FSYNC {
            status(crate::lkl::syscall(FSYNC, [self.fd, 0, 0, 0, 0, 0]))?;
        } else {
            let mut done = 0;
            while done < request.data.len() {
                let remaining = &mut request.data[done..];
                let advanced = status(crate::lkl::syscall(
                    syscall,
                    [
                        self.fd,
                        remaining.as_mut_ptr() as i64,
                        remaining.len() as i64,
                        (request.offset + done as u64) as i64,
                        0,
                        0,
                    ],
                ))?;
                if advanced == 0 || advanced > remaining.len() {
                    return Err(BlockError::Protocol("short Linux block transfer"));
                }
                done += advanced;
            }
        }
        if *completed == 0 {
            crate::println!("STELA_SPTM: first guest SSD {name} completed through LKL");
        }
        *completed += 1;
        Ok(())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        assert_eq!(crate::arch::cpu_id(), 0);
        let flush = crate::lkl::syscall(FSYNC, [self.fd, 0, 0, 0, 0, 0]);
        let close = crate::lkl::syscall(CLOSE, [self.fd, 0, 0, 0, 0, 0]);
        crate::println!(
            "STELA_SPTM: LKL SSD completed {} reads, {} writes, {} flushes; fsync={flush}, close={close}",
            self.reads, self.writes, self.flushes
        );
    }
}

fn status(result: i64) -> Result<usize, BlockError> {
    usize::try_from(result).map_err(|_| BlockError::Device {
        status: result.unsigned_abs() as u32,
    })
}
