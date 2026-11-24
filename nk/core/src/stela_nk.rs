//! Trusted, bounded compiler and system-mode engine probes. Run before SMP.
//! The private host recovers owned native faults; it has no guest sandbox or
//! general shadow paging and mounts only a polled UART fixture, not a complete
//! board. A separate MMU-on fixture exercises a bounded shadow aperture.
use stela_compiler::block::{BlockExit, BlockProfile, ExitKind, Features};
use stela_compiler::{GuestAddress, GuestState, Options, TargetCpu};
mod fault;
mod host;
mod ram;
pub(crate) use fault::recover_native_fault;

/// Owns the sole identity mapping of one code frame; no writable view escapes.
struct Code(*mut u8);
impl Code {
    fn new(bytes: &[u8]) -> Self {
        assert!(!bytes.is_empty() && bytes.len() <= 4096);
        let frame = crate::frames::alloc().expect("stela code frame");
        unsafe {
            crate::paging::boot_code_rights(frame as u64, false);
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), frame, bytes.len());
            crate::paging::boot_code_rights(frame as u64, true);
        }
        Self(frame)
    }
    // Only trusted finite blocks compiled below, accessing exclusively owned
    // RAM disjoint from state, code and host metadata, may reach this call.
    unsafe fn call(&self, state: &mut GuestState) -> BlockExit {
        // Use the compiler's entry thunk: ordinary exits reload the saved
        // host stack, so a plain C function-pointer call is insufficient.
        let raw = stela_compiler::x86_64::block_entry::enter(self.0, state, 0, 0);
        BlockExit {
            next_pc: raw as u64,
            kind: (raw >> 64) as u64,
        }
    }
}
impl Drop for Code {
    fn drop(&mut self) {
        unsafe {
            crate::paging::boot_code_rights(self.0 as u64, false);
            crate::frames::free(self.0);
        }
    }
}

fn run(words: &[u32], pc: u64, state: &mut GuestState) -> BlockExit {
    let bytes: alloc::vec::Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let options = Options::new(BlockProfile::new(Features::NONE), TargetCpu::Baseline);
    let block = stela_compiler::compile(&bytes, GuestAddress(pc), &options)
        .expect("stela trusted block compilation");
    assert_eq!(block.source_bytes, bytes.len());
    let code = Code::new(&block.native.x86_64_code);
    // All blocks below terminate at a return/branch; none can fault or touch
    // arbitrary host addresses. The owner remains borrowed through return.
    unsafe { code.call(state) }
}

pub fn smoke() {
    crate::println!("STELA_EXEC_BEGIN");
    let mut state = GuestState::default();
    state.x[0] = 41;
    state.x[30] = 0x2000;
    let exit = run(&[0x9100_0400, 0xd65f_03c0], 0x1000, &mut state);
    assert_eq!(state.x[0], 42);
    assert_eq!((exit.next_pc, exit.kind), (0x2000, ExitKind::Return as u64));
    crate::println!("STELA_EXEC_REGISTERS_OK");

    // A real dispatcher boundary: subtract and branch until zero (taken and
    // fallthrough paths), then add 7 and return. No host-native loop replaces it.
    state.x[0] = 3;
    for remaining in [2, 1, 0] {
        let exit = run(&[0xd100_0400, 0xb5ff_ffe0], 0x1000, &mut state);
        assert_eq!(state.x[0], remaining);
        assert_eq!(exit.next_pc, if remaining != 0 { 0x1000 } else { 0x1008 });
    }
    let exit = run(&[0x9100_1c00, 0xd65f_03c0], 0x1008, &mut state);
    assert_eq!(state.x[0], 7);
    assert_eq!(exit.next_pc, 0x2000);
    crate::println!("STELA_EXEC_BRANCH_OK");

    // ldr x0,[x1]; add x0,x0,#1; str x0,[x1]; ret
    let mut data = alloc::boxed::Box::new(0x1234_5678_9abc_def0u64);
    state.x[1] = (&mut *data as *mut u64) as u64;
    let exit = run(
        &[0xf940_0020, 0x9100_0400, 0xf900_0020, 0xd65f_03c0],
        0x3000,
        &mut state,
    );
    assert_eq!(*data, 0x1234_5678_9abc_def1);
    assert_eq!(state.x[0], *data);
    assert_eq!((exit.next_pc, exit.kind), (0x2000, ExitKind::Return as u64));
    crate::println!("STELA_EXEC_MEMORY_OK");

    state.x[0] = 1;
    run(&[0xf100_0400, 0xd65f_03c0], 0x4000, &mut state); // subs x0,x0,#1; ret
    assert_eq!(state.x[0], 0);
    assert_eq!(state.nzcv, 0x6000_0000); // Z and C
    state.v[1] = [0x00ff_00ff_00ff_00ff, 0xaaaa_aaaa_aaaa_aaaa];
    state.v[2] = [0xff00_ff00_ff00_ff00, 0x5555_5555_5555_5555];
    run(&[0x4ea2_1c20, 0xd65f_03c0], 0x5000, &mut state); // orr v0.16b,v1.16b,v2.16b; ret
    assert_eq!(state.v[0], [u64::MAX; 2]);
    assert_eq!(state.nzcv, 0x6000_0000); // vector OR does not change NZCV
    crate::println!("STELA_EXEC_FLAGS_SIMD_OK");
    crate::println!("STELA_EXEC_PASS");
    host::probe();
}
