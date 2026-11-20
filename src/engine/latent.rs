//! The latent test plugin (`fixture/latent.c`): an LV2 unit that delays by
//! its Delay control and reports that as its latency. No plugin on a CI
//! runner reports a latency, and none lets a test set it, so the tests and
//! `analysis-test` build this one with the system's C compiler and load it
//! by path.

use crate::control::{Lv2Uri, NodeKind, ParamId};
use std::path::PathBuf;
use std::sync::OnceLock;

pub const URI: &str = "urn:kabelsalat:fixture:latent";
/// Its one control: the delay in frames, 0 to `pdc::MAX_FRAMES`.
pub const P_DELAY: ParamId = 0;
/// The delay it starts with (`lv2:default`).
pub const DEFAULT_FRAMES: usize = 240;

const FILES: [(&str, &str); 3] = [
    ("latent.c", include_str!("fixture/latent.c")),
    ("manifest.ttl", include_str!("fixture/manifest.ttl")),
    ("latent.ttl", include_str!("fixture/latent.ttl")),
];

/// The node kind of the fixture.
pub fn kind() -> NodeKind {
    NodeKind::Lv2 { uri: Lv2Uri::new(URI) }
}

/// The Delay control's value for `frames`.
pub fn delay_value(frames: usize) -> f64 {
    frames as f64 / super::pdc::MAX_FRAMES as f64
}

/// Build the bundle once per source version and let the LV2 host find it.
/// Safe to call from several tests and processes at once: it is built
/// beside its final place and moved there whole.
pub fn install() -> anyhow::Result<()> {
    static BUILT: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    match BUILT.get_or_init(|| build().map_err(|e| format!("{e:#}"))) {
        Ok(dir) => {
            crate::lv2::add_bundle(dir);
            Ok(())
        }
        Err(e) => anyhow::bail!("the latent test plugin could not be built (it needs a C compiler, `cc`): {e}"),
    }
}

fn build() -> anyhow::Result<PathBuf> {
    use anyhow::Context;
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    FILES.hash(&mut hash);
    std::env::consts::ARCH.hash(&mut hash);
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("kabelsalat")
        .join("fixtures");
    let dir = cache.join(format!("latent-{:016x}.lv2", hash.finish()));
    if dir.join("latent.so").exists() {
        return Ok(dir);
    }
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let work = cache.join(format!(".latent-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;
    for (name, text) in FILES {
        std::fs::write(work.join(name), text)?;
    }
    let cc = std::process::Command::new("cc")
        .args(["-shared", "-fPIC", "-O1", "-o", "latent.so", "latent.c"])
        .current_dir(&work)
        .output()
        .context("running cc")?;
    anyhow::ensure!(cc.status.success(), "cc: {}", String::from_utf8_lossy(&cc.stderr));
    // Another process may have put the same bundle there meanwhile.
    let moved = std::fs::rename(&work, &dir);
    if moved.is_err() {
        let _ = std::fs::remove_dir_all(&work);
    }
    anyhow::ensure!(dir.join("latent.so").exists(), "moving the bundle to {}: {moved:?}", dir.display());
    Ok(dir)
}
