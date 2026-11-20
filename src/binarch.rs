//! What a plugin binary was built for, read from the file itself.
//!
//! glibc's `dlopen` answers "cannot open shared object file: No such file or
//! directory" for an ELF of another CPU (#135), so the host asks here first
//! and can say what the file is and why it will never load.

use std::path::Path;

/// The CPU names `std::env::consts::ARCH` uses, by ELF `e_machine` and
/// `EI_CLASS` (1 = 32-bit, 2 = 64-bit).
fn elf_arch(machine: u16, class: u8) -> Option<&'static str> {
    Some(match (machine, class) {
        (3, _) => "x86",
        (62, _) => "x86_64",
        (40, _) => "arm",
        (183, _) => "aarch64",
        (243, 1) => "riscv32",
        (243, _) => "riscv64",
        (20, _) => "powerpc",
        (21, _) => "powerpc64",
        (22, _) => "s390x",
        (258, _) => "loongarch64",
        _ => return None,
    })
}

/// What the binary starting with `header` was built for: a CPU name for an
/// ELF, "Windows" or "macOS" for their formats. `None` when the bytes are
/// none of these (the loader then gives its own reason).
pub fn built_for(header: &[u8]) -> Option<String> {
    if header.len() >= 20 && header[..4] == [0x7f, b'E', b'L', b'F'] {
        let raw = [header[18], header[19]];
        let machine = if header[5] == 2 { u16::from_be_bytes(raw) } else { u16::from_le_bytes(raw) };
        return Some(match elf_arch(machine, header[4]) {
            Some(name) => name.to_string(),
            None => format!("an unknown CPU (ELF machine {machine})"),
        });
    }
    if header.len() >= 2 && header[..2] == *b"MZ" {
        return Some("Windows".into());
    }
    if let Some(magic) = header.get(..4) {
        let magic = u32::from_be_bytes([magic[0], magic[1], magic[2], magic[3]]);
        if matches!(magic, 0xfeed_face | 0xfeed_facf | 0xcefa_edfe | 0xcffa_edfe | 0xcafe_babe) {
            return Some("macOS".into());
        }
    }
    None
}

/// The sentence for a binary that cannot run on `host`, `None` when it can
/// (or when the header says nothing).
pub fn mismatch(file_name: &str, header: &[u8], host: &str) -> Option<String> {
    let built = built_for(header)?;
    if built == host {
        return None;
    }
    Some(match built.as_str() {
        "Windows" | "macOS" => format!("{file_name} is a {built} plugin; this machine runs Linux on {host}, so it cannot be loaded"),
        _ => format!("{file_name} is built for {built}; this machine is {host}, so it cannot be loaded"),
    })
}

fn file_mismatch(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut header = [0u8; 20];
    let n = std::fs::File::open(path).ok()?.read(&mut header).ok()?;
    let name = path.file_name()?.to_string_lossy();
    mismatch(&name, &header[..n], std::env::consts::ARCH)
}

/// Why the plugin at `path` can never load on this machine, `None` when
/// nothing speaks against it. A folder (an LV2 bundle) is judged by the
/// `.so` files directly in it: foreign when it has some and all are.
pub fn foreign(path: &Path) -> Option<String> {
    if !path.is_dir() {
        return file_mismatch(path);
    }
    let mut first = None;
    for entry in std::fs::read_dir(path).ok()?.flatten() {
        let p = entry.path();
        if p.extension().is_some_and(|e| e == "so") {
            match file_mismatch(&p) {
                Some(reason) => first = first.or(Some(reason)),
                None => return None,
            }
        }
    }
    first
}

/// `Err` with the reason when `path` cannot load here; call before `dlopen`.
pub fn ensure_loadable(path: &Path) -> anyhow::Result<()> {
    match foreign(path) {
        Some(reason) => Err(anyhow::anyhow!(reason)),
        None => Ok(()),
    }
}

/// A minimal ELF header (64-bit, little-endian) of `machine`, for tests of
/// the paths that meet a foreign plugin.
#[cfg(test)]
pub fn fake_elf(machine: u16) -> Vec<u8> {
    let mut h = vec![0u8; 64];
    h[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    h[4] = 2; // ELFCLASS64
    h[5] = 1; // little-endian
    h[6] = 1; // EV_CURRENT
    h[16] = 3; // ET_DYN
    h[18..20].copy_from_slice(&machine.to_le_bytes());
    h
}

/// An `e_machine` that is not this machine's: x86_64, or aarch64 on x86_64.
#[cfg(test)]
pub fn other_machine() -> (u16, &'static str) {
    if std::env::consts::ARCH == "x86_64" { (183, "aarch64") } else { (62, "x86_64") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_elf_of_another_cpu_names_both_cpus() {
        let x86 = fake_elf(62);
        assert_eq!(built_for(&x86).as_deref(), Some("x86_64"));
        assert_eq!(
            mismatch("Vital.clap", &x86, "aarch64").as_deref(),
            Some("Vital.clap is built for x86_64; this machine is aarch64, so it cannot be loaded")
        );
        assert_eq!(mismatch("Vital.clap", &x86, "x86_64"), None);
        assert_eq!(
            mismatch("a.so", &fake_elf(183), "x86_64").as_deref(),
            Some("a.so is built for aarch64; this machine is x86_64, so it cannot be loaded")
        );
    }

    #[test]
    fn a_big_endian_header_is_read_in_its_own_byte_order() {
        let mut h = fake_elf(0);
        h[5] = 2;
        h[18..20].copy_from_slice(&22u16.to_be_bytes());
        assert_eq!(built_for(&h).as_deref(), Some("s390x"));
    }

    #[test]
    fn windows_and_macos_binaries_are_named_and_other_bytes_are_left_to_the_loader() {
        assert!(mismatch("Vital.clap", b"MZ\x90\x00", "x86_64").unwrap().contains("is a Windows plugin"));
        assert!(mismatch("Vital.clap", &[0xcf, 0xfa, 0xed, 0xfe], "aarch64").unwrap().contains("is a macOS plugin"));
        assert_eq!(built_for(b"#!/bin/sh\n"), None);
        assert_eq!(built_for(&[0x7f, b'E', b'L']), None);
        assert!(built_for(&fake_elf(9999)).unwrap().contains("ELF machine 9999"));
    }

    #[test]
    fn this_very_binary_is_loadable_and_a_foreign_file_is_not() {
        let me = std::env::current_exe().unwrap();
        assert_eq!(foreign(&me), None, "the test binary runs here");
        let dir = std::env::temp_dir().join(format!("kabelsalat-binarch-{}", std::process::id()));
        let bundle = dir.join("Foreign.lv2");
        std::fs::create_dir_all(&bundle).unwrap();
        let (machine, name) = other_machine();
        std::fs::write(bundle.join("foreign.so"), fake_elf(machine)).unwrap();
        std::fs::write(bundle.join("manifest.ttl"), "").unwrap();
        let reason = foreign(&bundle).expect("a folder of foreign binaries is foreign");
        assert!(reason.starts_with(&format!("foreign.so is built for {name}; this machine is {}", std::env::consts::ARCH)), "{reason}");
        assert!(ensure_loadable(&bundle.join("foreign.so")).is_err());
        // One binary that runs here makes the bundle usable.
        std::os::unix::fs::symlink(&me, bundle.join("native.so")).unwrap();
        assert_eq!(foreign(&bundle), None);
        assert_eq!(foreign(&dir.join("missing.clap")), None, "a missing file is the loader's to report");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
