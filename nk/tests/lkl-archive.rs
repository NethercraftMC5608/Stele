// SPDX-License-Identifier: GPL-2.0-or-later
//! Exercise the actual build helper on small x86/aarch64 ELF archives.
#[path = "../core/lkl_archive.rs"]
mod archive;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn run(command: &mut Command) {
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
fn object(root: &Path, target: &str, name: &str, source: &str) -> PathBuf {
    let c = root.join(format!("{name}.c"));
    let o = root.join(format!("{name}.o"));
    fs::write(&c, source).unwrap();
    run(Command::new("clang")
        .args([
            "-ffreestanding",
            "-fno-stack-protector",
            "-c",
            "-target",
            target,
        ])
        .arg(c)
        .arg("-o")
        .arg(&o));
    o
}
#[test]
fn prepares_both_targets_without_mutating_input_and_rejects_ambiguous_symbols() {
    let tools = archive::Tools::discover().unwrap();
    let ar = tools.nm.parent().unwrap().join("llvm-ar");
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let temp =
        Temp(std::env::temp_dir().join(format!("nk-lkl-archive-{}-{stamp}", std::process::id())));
    fs::create_dir(&temp.0).unwrap();
    for target in ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"] {
        let root = temp.0.join(target);
        fs::create_dir(&root).unwrap();
        let obj = object(
            &root,
            target,
            "lkl",
            "static unsigned char cpu[64]; void *cpu_ref(void) {return cpu;}",
        );
        let source = root.join("input.a");
        run(Command::new(&ar).arg("crs").arg(&source).arg(&obj));
        let before = fs::read(&source).unwrap();
        let out = root.join("out");
        fs::create_dir(&out).unwrap();
        let prepared = archive::prepare(&source, &out, &tools).unwrap();
        assert_eq!(before, fs::read(&source).unwrap());
        let symbols = tools.symbols(&prepared).unwrap();
        assert!(symbols.contains(&('B', "nk_lkl_cpu".into())));
        assert!(!symbols.iter().any(|(_, n)| n == "cpu"));
        let next = root.join("second");
        fs::create_dir(&next).unwrap();
        let copy = archive::prepare(&prepared, &next, &tools).unwrap();
        assert_eq!(fs::read(&prepared).unwrap(), fs::read(copy).unwrap());
        assert!(archive::prepare(&prepared, &out, &tools)
            .unwrap_err()
            .contains("input must differ"));

        let extra = object(
            &root,
            target,
            "extra",
            "static unsigned char cpu[8]; void *other_ref(void) {return cpu;}",
        );
        let duplicate = root.join("duplicate.a");
        run(Command::new(&ar)
            .arg("crs")
            .arg(&duplicate)
            .arg(&obj)
            .arg(&extra));
        let duplicate_before = fs::read(&duplicate).unwrap();
        assert!(archive::prepare(&duplicate, &out, &tools)
            .unwrap_err()
            .contains("found 2 cpu"));
        assert_eq!(duplicate_before, fs::read(&duplicate).unwrap());
        let missing = root.join("missing.a");
        let obj = object(&root, target, "missing", "int marker;");
        run(Command::new(&ar).arg("crs").arg(&missing).arg(obj));
        assert!(archive::prepare(&missing, &out, &tools)
            .unwrap_err()
            .contains("found 0 cpu"));
    }
}
