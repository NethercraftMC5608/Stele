// SPDX-License-Identifier: GPL-2.0-or-later
//! Build-host preparation of LKL archives. Never rewrites the supplied archive.
//! Only symbol visibility/name changes; code, data and relocations retain
//! their meaning and the final linker computes the address for either target.
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

pub struct Tools {
    pub nm: PathBuf,
    pub objcopy: PathBuf,
}

fn output(command: &mut Command, action: &str) -> Result<Vec<u8>, String> {
    let result = command
        .output()
        .map_err(|e| format!("{action}: {e}; install rustup's llvm-tools component"))?;
    if !result.status.success() {
        return Err(format!(
            "{action}: {}",
            String::from_utf8_lossy(&result.stderr)
        ));
    }
    Ok(result.stdout)
}

impl Tools {
    pub fn discover() -> Result<Self, String> {
        let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let sysroot = output(
            Command::new(&rustc).args(["--print", "sysroot"]),
            "locate Rust sysroot",
        )?;
        let sysroot = String::from_utf8(sysroot).map_err(|e| e.to_string())?;
        let host = match env::var("HOST") {
            Ok(host) => host,
            Err(_) => {
                let version = output(Command::new(&rustc).arg("-vV"), "locate Rust host")?;
                let version = String::from_utf8(version).map_err(|e| e.to_string())?;
                version
                    .lines()
                    .find_map(|l| l.strip_prefix("host: "))
                    .ok_or("rustc did not report its host")?
                    .to_owned()
            }
        };
        let bin = Path::new(sysroot.trim())
            .join("lib/rustlib")
            .join(host)
            .join("bin");
        let tool = |name: &str| {
            let bundled = bin.join(name);
            if bundled.is_file() {
                bundled
            } else {
                PathBuf::from(name)
            }
        };
        Ok(Self {
            nm: tool("llvm-nm"),
            objcopy: tool("llvm-objcopy"),
        })
    }
    pub fn symbols(&self, path: &Path) -> Result<Vec<(char, String)>, String> {
        let listing = output(
            Command::new(&self.nm).arg("--defined-only").arg(path),
            "inspect LKL symbols",
        )?;
        let listing = String::from_utf8(listing).map_err(|e| e.to_string())?;
        Ok(listing
            .lines()
            .filter_map(|line| {
                let mut p = line.split_whitespace();
                let (address, kind, name) = (p.next()?, p.next()?, p.next()?);
                u64::from_str_radix(address, 16).ok()?;
                (kind.len() == 1).then(|| (kind.chars().next().unwrap(), name.to_owned()))
            })
            .collect())
    }
}

/// Return a private archive exposing exactly one `B nk_lkl_cpu` symbol.
/// Reject ambiguity instead of picking an unrelated same-named static.
pub fn prepare(source: &Path, out_dir: &Path, tools: &Tools) -> Result<PathBuf, String> {
    let symbols = tools.symbols(source)?;
    let old: Vec<_> = symbols.iter().filter(|(_, n)| n == "cpu").collect();
    let exported: Vec<_> = symbols.iter().filter(|(_, n)| n == "nk_lkl_cpu").collect();
    let already = old.is_empty() && exported.len() == 1 && exported[0].0 == 'B';
    if !already && !(exported.is_empty() && old.len() == 1 && matches!(old[0].0, 'b' | 'B')) {
        return Err(format!("expected one LKL BSS cpu (or exported nk_lkl_cpu); found {} cpu and {} nk_lkl_cpu symbols",old.len(),exported.len()));
    }
    let dest = out_dir.join("libnklkl.a");
    if dest.exists()
        && fs::canonicalize(source).map_err(|e| e.to_string())?
            == fs::canonicalize(&dest).map_err(|e| e.to_string())?
    {
        return Err("LKL input must differ from the build-owned output archive".into());
    }
    if already {
        fs::copy(source, &dest).map_err(|e| e.to_string())?;
    } else {
        output(
            Command::new(&tools.objcopy)
                .arg("--globalize-symbol=cpu")
                .arg(source)
                .arg(&dest),
            "export LKL cpu",
        )?;
        output(
            Command::new(&tools.objcopy)
                .arg("--redefine-sym=cpu=nk_lkl_cpu")
                .arg(&dest),
            "namespace LKL cpu",
        )?;
    }
    let result = tools.symbols(&dest)?;
    if result
        .iter()
        .filter(|(k, n)| *k == 'B' && n == "nk_lkl_cpu")
        .count()
        != 1
        || result.iter().any(|(_, n)| n == "cpu")
    {
        return Err("prepared LKL archive did not expose exactly one global nk_lkl_cpu".into());
    }
    Ok(dest)
}
