/// The offset of LKL's file-local `cpu` symbol within the archive.
///
/// Returns None whenever it cannot be determined -- no llvm-nm on PATH, an
/// unreadable archive, a rename upstream. The caller treats that as "no
/// check available", never as a build failure.
fn lkl_cpu_offset(archive: &std::path::Path) -> Option<u64> {
    let sysroot = std::process::Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .ok()?;
    let sysroot = String::from_utf8(sysroot.stdout).ok()?;
    let host = env::var("HOST").unwrap_or_default();
    let bundled = format!("{}/lib/rustlib/{host}/bin/llvm-nm", sysroot.trim());
    let nm = if std::path::Path::new(&bundled).exists() { bundled } else { "llvm-nm".into() };

    let out = std::process::Command::new(nm).arg(archive).output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    for line in text.lines() {
        // "0000000000024318 b cpu" -- lower-case b is a file-local bss
        // symbol, which is exactly what makes it unnameable from Rust.
        //
        // `continue`, not `?`, on a short line: the listing is two million
        // characters with blank lines and per-object headers in it, and `?`
        // on the first of those returned None for the whole function -- it
        // reported "no `cpu` symbol" for an archive that plainly has one.
        let mut parts = line.split_whitespace();
        let (Some(addr), Some(kind), Some(name)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if name == "cpu" && kind.eq_ignore_ascii_case("b") {
            return u64::from_str_radix(addr, 16).ok();
        }
    }
    None
}

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let script = match arch.as_str() {
        "x86_64" => "linker-x86_64.ld",
        _ => "linker.ld",
    };
    println!("cargo:rustc-link-arg=-T{dir}/{script}");
    if arch == "x86_64" {
        println!("cargo:rustc-link-arg=--no-pie");
        println!("cargo:rustc-link-arg=-zmax-page-size=0x1000");
    }
    println!("cargo:rerun-if-changed={script}");
    println!("cargo:rerun-if-changed=src/boot.s");
    println!("cargo:rerun-if-changed=src/boot-x86_64.s");
    println!("cargo:rerun-if-changed=src/user-x86_64.s");
    println!("cargo:rerun-if-changed=src/arch_aarch64.rs");
    println!("cargo:rerun-if-changed=src/arch_x86_64.rs");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let dest_path = out_dir.join("arch_generated.rs");
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let content = match target_arch.as_str() {
        "aarch64" => fs::read_to_string("src/arch_aarch64.rs").unwrap(),
        "x86_64" => fs::read_to_string("src/arch_x86_64.rs").unwrap(),
        _ => panic!("Unsupported target architecture: {}", target_arch),
    };
    fs::write(&dest_path, content).unwrap();

    println!("cargo:rerun-if-env-changed=NK_LINUX_LIB");
    println!("cargo::rustc-check-cfg=cfg(nk_linux)");
    println!("cargo::rustc-check-cfg=cfg(nk_lkl)");
    println!("cargo:rerun-if-env-changed=NK_LKL_LIB");
    if let Ok(lib) = env::var("NK_LKL_LIB") {
        let path = fs::canonicalize(&lib).unwrap_or_else(|e| panic!("NK_LKL_LIB={lib}: {e}"));
        let dir = path.parent().unwrap().display();
        println!("cargo:rustc-link-search=native={dir}");
        println!("cargo:rustc-link-arg=--whole-archive");
        println!("cargo:rustc-link-arg=-lnklkl");
        println!("cargo:rustc-link-arg=--no-whole-archive");
        println!("cargo:rustc-cfg=nk_lkl");
        println!("cargo:rerun-if-changed={lib}");

        // LKL's `cpu` is a file-local symbol (`b cpu`), so it cannot be named
        // from Rust and its address is hardcoded in main.rs. It has gone
        // stale more than once: an unrelated edit moves it and nothing says
        // so, because a stale address does not fault -- it reads whatever now
        // lives there as an `LklCpu`, and the wait graph then reports an
        // owner and count that are plausible and belong to something else.
        if let Some(off) = lkl_cpu_offset(&path) {
            println!("cargo:rustc-env=NK_LKL_CPU_BSS_OFFSET={off:#x}");
        }
    }
    if let Ok(lib) = env::var("NK_LINUX_LIB") {
        let path = fs::canonicalize(&lib).unwrap_or_else(|e| panic!("NK_LINUX_LIB={lib}: {e}"));
        let dir = path.parent().unwrap().display();
        println!("cargo:rustc-link-search=native={dir}");
        println!("cargo:rustc-link-arg=--whole-archive");
        println!("cargo:rustc-link-arg=-lnklinux");
        println!("cargo:rustc-link-arg=--no-whole-archive");
        println!("cargo:rustc-link-arg=--undefined=nk_linux_init");
        println!("cargo:rustc-cfg=nk_linux");
        println!("cargo:rerun-if-changed={lib}");
    }
    // The x86-64 ring-3 smoke test: a flat blob copied verbatim to
    // USER_BASE by `user::spawn`. NK_USER_BLOB names it; without it an empty
    // file is written so `include_bytes!`/`.incbin` still resolves, because
    // cfg is applied after the macro is expanded.
    println!("cargo:rerun-if-env-changed=NK_USER_BLOB");
    let blob = out_dir.join("user-blob.bin");
    match env::var("NK_USER_BLOB") {
        Ok(b) => {
            let path = fs::canonicalize(&b).unwrap_or_else(|e| panic!("NK_USER_BLOB={b}: {e}"));
            fs::copy(&path, &blob).unwrap();
            println!("cargo:rerun-if-changed={}", path.display());
        }
        Err(_) => {
            fs::write(&blob, []).unwrap();
        }
    }
    println!("cargo:rustc-env=NK_USER_BLOB_PATH={}", blob.display());

    println!("cargo:rerun-if-env-changed=NK_INIT");
    println!("cargo::rustc-check-cfg=cfg(nk_init)");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("nk-init.bin");
    if let Ok(init) = env::var("NK_INIT") {
        let path = fs::canonicalize(&init).unwrap_or_else(|e| panic!("NK_INIT={init}: {e}"));
        fs::copy(&path, &out).unwrap();
        println!("cargo:rustc-cfg=nk_init");
        println!("cargo:rerun-if-changed={}", path.display());
    } else {
        fs::write(&out, []).unwrap();
    }
}
