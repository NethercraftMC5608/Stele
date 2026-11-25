//! Fingerprint what decides the code a block translation holds: with the
//! compiler's own fingerprint it makes the persistent translation store's
//! translator id (`translations::translator_id`).
//!
//! A stored entry is the compiler's output for its admitted input bytes, PC
//! and entry context. The context describes every preparation input
//! (`Preparation::context`), and the admitted input is compared byte for
//! byte before reuse, so chip admission checks itself. What else decides a
//! block is fingerprinted here: block preparation (`prepare.rs` and the
//! constants of `block_inputs.rs`), the record format, proof and relocation
//! (`translations`, `stored.rs`), and the chip's access checks that end a
//! block (Contracts' `chip` and `cpu_features`). The rest of the engine, the
//! other crates and the lock files are not inputs: rebuilding them leaves
//! the store warm. Unit-test modules are left out. A test in
//! `translations/tests.rs` checks that these files name no other engine
//! module.
fn main() {
    // `host_memory`: POSIX mappings for generic native-code validation.
    // A std build elsewhere (Windows, for setup) has none
    // and refuses such requests, as the UEFI build does without boot RAM.
    println!("cargo::rustc-check-cfg=cfg(host_memory)");
    // `stela_ring0`: the engine runs at x86-64 ring 0 with no OS beneath it,
    // freestanding locks and system-register paths included. That is
    // stela.efi (target_os = "uefi") and nk (target_os = "none") alike;
    // only the loader around the engine differs.
    println!("cargo::rustc-check-cfg=cfg(stela_ring0)");
    // stela_fingerprint only watches the translation inputs; without this a
    // change to the cfgs above would not re-run the script.
    println!("cargo:rerun-if-changed=build.rs");
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if arch == "x86_64"
        && (os == "uefi" || os == "none")
        && std::env::var_os("CARGO_FEATURE_STD").is_none()
    {
        println!("cargo::rustc-cfg=stela_ring0");
    }
    if std::env::var_os("CARGO_FEATURE_STD").is_some()
        && std::env::var_os("CARGO_CFG_UNIX").is_some()
    {
        println!("cargo::rustc-cfg=host_memory");
    }
    stela_fingerprint::emit_with(
        "STELA_ENGINE_SOURCE_SHA256",
        &TRANSLATION_INPUTS,
        &stela_fingerprint::Options {
            skip_test_modules: true,
            locked: &[],
        },
    );
    println!(
        "cargo:rustc-env=STELA_ENGINE_TRANSLATION_INPUTS={}",
        TRANSLATION_INPUTS.join(" ")
    );
}

/// The engine's and Contracts' translation inputs, relative to this crate.
const TRANSLATION_INPUTS: [&str; 9] = [
    "build.rs",
    "src/machine/engine/block_inputs.rs",
    "src/machine/engine/prepare.rs",
    "src/machine/engine/stored.rs",
    "src/translations.rs",
    "src/translations",
    "../stela-contracts/src/chip.rs",
    "../stela-contracts/src/chip",
    "../stela-contracts/src/cpu_features.rs",
];
