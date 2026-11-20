// SPDX-License-Identifier: GPL-2.0-only
//! Cold wake: firmware left the controller in D1 to D3hot or with memory
//! decoding off, as boot logs show for AMD 1022:1487 and 1022:15e3.
use super::*;
use crate::bus::pci::regs;

const DECODE: u16 = regs::COMMAND_MEMORY;
const MASTER: u16 = regs::COMMAND_MASTER;

fn amd(state: u16, command: u16) -> (Function<Model>, Clock, Bar) {
    let (mut function, timer, bar) = fixture(state);
    set_identity(&mut function, AMD, 0x1487);
    function.config().set16(regs::COMMAND, command);
    (function, timer, bar)
}

fn wake_model(
    function: &mut Function<Model>,
    timer: &Clock,
    bar: Bar,
) -> Result<Wake, AttachError> {
    // SAFETY: the isolated stateful model has no real hardware/firmware owner.
    unsafe { wake_cold(function, bar, timer) }
}

fn cold_model(function: &mut Function<Model>, timer: &Clock, bar: Bar) -> Result<(), AttachError> {
    // SAFETY: The PCI function and mapped byte vector are owned by this test; Rc
    // aliases are used sequentially. No firmware or background DMA can race them.
    unsafe {
        prepare_cold_with_registers(
            function,
            bar,
            timer,
            Ownership::ExclusiveLegacy,
            MappedModel::new(),
        )
    }
    .map(drop)
}

#[test]
fn cold_entry_refuses_a_sleeping_or_undecoded_controller() {
    for (state, command) in [(3, 0x0406), (3, 0), (0, 0)] {
        let (mut function, timer, bar) = amd(state, command);
        assert_eq!(
            cold_model(&mut function, &timer, bar).unwrap_err(),
            AttachError::PowerNotReady,
            "state {state} command {command:#x}"
        );
        assert!(function.config().writes.is_empty());
    }
}

#[test]
fn sleeping_or_undecoded_controller_is_woken_for_the_cold_entry() {
    for (state, command, from_state, decode_enabled) in [
        (3, 0x0406, Some(3), false),
        (3, 0, Some(3), true),
        (2, MASTER, Some(2), true),
        (0, 0, None, true),
    ] {
        let (mut function, timer, bar) = amd(state, command);
        let wake = wake_model(&mut function, &timer, bar).unwrap();
        assert_eq!(
            wake,
            Wake {
                from_state,
                decode_enabled
            },
            "state {state} command {command:#x}"
        );
        let config = function.config();
        let pmcsr = config.get16(0x54);
        assert_eq!(pmcsr & PM_STATE, 0);
        if state != 0 {
            // PME is disabled and pending PME evidence (W1C) is kept.
            assert_eq!(pmcsr & PM_PME_ENABLE, 0);
            assert_ne!(pmcsr & PM_PME_STATUS, 0);
        }
        let command = config.get16(regs::COMMAND);
        assert_eq!(command & (DECODE | MASTER), DECODE);
        assert_ne!(command & regs::COMMAND_INTX_DISABLE, 0);
        // A D3hot exit cleared every BAR; the saved ones are back.
        assert_eq!(config.words[4], 0xfebf_0000);
        assert_eq!(config.words[5], 0x1234_0000);
        match state {
            3 => assert!(timer.now_us() >= 10_000),
            2 => assert!(timer.now_us() >= 200),
            _ => assert_eq!(timer.now_us(), 0),
        }
        cold_model(&mut function, &timer, bar).unwrap();
    }
}

#[test]
fn awake_controller_gets_no_write() {
    let (mut function, timer, bar) = amd(0, 0x0406);
    assert_eq!(
        wake_model(&mut function, &timer, bar).unwrap(),
        Wake::default()
    );
    assert!(function.config().writes.is_empty());
    assert_eq!(timer.now_us(), 0);
}

#[test]
fn modern_intel_gets_no_write() {
    for (state, command) in [(3, 0x0406), (0, 0)] {
        let (mut function, timer, bar) = modern_pci_model();
        function.config().set16(0x54, state);
        function.config().set16(regs::COMMAND, command);
        assert_eq!(
            wake_model(&mut function, &timer, bar).unwrap(),
            Wake::default()
        );
        assert!(function.config().writes.is_empty());
        // The mapped entry still refuses before any MMIO read.
        let registers = MappedModel::new();
        // SAFETY: The PCI function and mapped byte vector are owned by this test; Rc
        // aliases are used sequentially. No firmware or background DMA can race them.
        let result = unsafe {
            prepare_with_registers(
                &mut function,
                bar,
                &timer,
                Ownership::ExclusiveLegacy,
                registers.clone(),
            )
        };
        assert_eq!(result.err(), Some(AttachError::PowerNotReady));
        assert_eq!(registers.reads.get(), 0);
    }
}

#[test]
fn denylisted_board_is_not_woken() {
    let (mut function, timer, bar) = amd(3, 0);
    // ASUS 1043:874f, on the pinned driver's unconditional denylist.
    function.config().words[11] = 0x874f_1043;
    assert_eq!(
        wake_model(&mut function, &timer, bar).unwrap_err(),
        AttachError::SubsystemExcluded
    );
    assert!(function.config().writes.is_empty());
}

#[test]
fn failed_wake_leaves_decoding_off() {
    let (mut function, timer, bar) = amd(3, 0x0406);
    function.config().reject_power = true;
    assert_eq!(
        wake_model(&mut function, &timer, bar).unwrap_err(),
        AttachError::PowerTransition
    );
    assert_eq!(
        function.config().get16(regs::COMMAND) & (DECODE | MASTER),
        0
    );

    // A command register that ignores writes stops before any power change.
    let (mut function, timer, bar) = amd(3, 0x0406);
    function.config().reject_command = true;
    assert_eq!(
        wake_model(&mut function, &timer, bar).unwrap_err(),
        AttachError::ConfigurationRejected
    );
    assert!(
        !function
            .config()
            .writes
            .iter()
            .any(|(offset, _, _, _)| *offset == 0x54)
    );
}
