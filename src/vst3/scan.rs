//! Crash-safe VST3 bundle discovery, architecture verification, and cached
//! out-of-process scanning.
//!
//! Each VST3 bundle on Linux is either a `.vst3` directory containing
//! `Contents/<arch>-linux/*.so` (native Linux plugins, or yabridge wrappers when
//! `Contents/x86_64-win` is also present) or a flat `.vst3` ELF shared library.
//! Scanning runs in an isolated `kabelsalat scan-vst3 <bundle>` child process
//! and caches results in `$XDG_CACHE_HOME/kabelsalat/vst3-scan-v1.json`.

use std::collections::{BTreeMap, HashSet};
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use vst3::Steinberg::{
    IPluginFactory, IPluginFactory2, IPluginFactory2Trait, IPluginFactoryTrait, PClassInfo,
    PClassInfo2, PFactoryInfo, TUID, kResultOk,
};
use vst3::ComPtr;

pub const VST3_KEY_PREFIX: &str = "vst3:";

const VST3_SCAN_CACHE_VERSION: u32 = 1;
const VST3_SCAN_TIMEOUT: Duration = Duration::from_secs(10);
const SCAN_STDOUT_MAX: usize = 256 * 1024;
const SCAN_ERR_TAIL: usize = 4 * 1024;
static SCAN_CACHE_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Extra bundle paths registered programmatically (tests & CLI self-tests).
static EXTRA_BUNDLES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// One audio plugin class exported by a scanned VST3 bundle.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Vst3PluginInfo {
    /// 32-character uppercase hex class ID (`TUID`).
    pub cid: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub sub_categories: Vec<String>,
    pub bundle_path: PathBuf,
    #[serde(default)]
    pub is_yabridge: bool,
}

/// Format a 16-byte VST3 `TUID` as 32 uppercase hex characters.
pub fn format_tuid(tuid: &TUID) -> String {
    let mut out = String::with_capacity(32);
    for &b in tuid {
        use std::fmt::Write;
        let _ = write!(out, "{:02X}", b as u8);
    }
    out
}

/// Parse a 32-hex-character CID string (tolerating optional `-` separators) back into a `TUID`.
pub fn parse_tuid(cid: &str) -> Option<TUID> {
    let cleaned: String = cid.chars().filter(|c| !matches!(c, '-' | '{' | '}' | ' ')).collect();
    if cleaned.len() != 32 {
        return None;
    }
    let mut out: TUID = [0; 16];
    for i in 0..16 {
        let byte = u8::from_str_radix(&cleaned[i * 2..i * 2 + 2], 16).ok()?;
        out[i] = byte as std::os::raw::c_char;
    }
    Some(out)
}

/// Unified host key used to route VST3 plugins through `ClapVoice::new_plugin` and `sandbox`.
pub fn voice_key(cid: &str) -> String {
    format!("{VST3_KEY_PREFIX}{cid}")
}

/// True when a unified host plugin key refers to a VST3 CID.
pub fn is_vst3_key(plugin_id: &str) -> bool {
    plugin_id.starts_with(VST3_KEY_PREFIX)
}

/// Extract the raw VST3 CID from a `"vst3:<cid>"` host key.
pub fn strip_vst3_key(plugin_id: &str) -> Option<&str> {
    plugin_id.strip_prefix(VST3_KEY_PREFIX)
}

/// Register an extra `.vst3` bundle path for discovery & loading (used by tests/CLI self-tests).
pub fn add_bundle(bundle: &Path) {
    let mut extra = EXTRA_BUNDLES.lock().unwrap_or_else(|e| e.into_inner());
    if !extra.iter().any(|p| p == bundle) {
        extra.push(bundle.to_path_buf());
    }
}

fn extra_bundle_list() -> Vec<PathBuf> {
    let mut out = EXTRA_BUNDLES.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Ok(env_val) = std::env::var("KABELSALAT_EXTRA_VST3_BUNDLES") {
        for part in env_val.split(':') {
            if !part.is_empty() {
                let p = PathBuf::from(part);
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        }
    }
    out
}

/// Colon-separated extra VST3 bundle paths for child workers (`plugin-worker`).
pub fn extra_bundles_env() -> Option<String> {
    let list = extra_bundle_list();
    if list.is_empty() {
        None
    } else {
        Some(
            list.into_iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(":"),
        )
    }
}

/// Standard Linux VST3 search directories plus `$VST3_PATH` entries, deduplicated in order.
pub fn vst3_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut push_unique = |p: PathBuf| {
        if !p.as_os_str().is_empty() && !dirs.contains(&p) {
            dirs.push(p);
        }
    };
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            push_unique(PathBuf::from(home).join(".vst3"));
        }
    }
    push_unique(PathBuf::from("/usr/lib/vst3"));
    push_unique(PathBuf::from("/usr/local/lib/vst3"));
    push_unique(PathBuf::from("/usr/lib64/vst3"));
    push_unique(PathBuf::from("/app/extensions/Plugins/vst3"));
    if let Ok(extra) = std::env::var("VST3_PATH") {
        for part in extra.split(':') {
            if !part.is_empty() {
                push_unique(PathBuf::from(part));
            }
        }
    }
    dirs
}

fn is_vst3_entry(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("vst3"))
}

fn collect_vst3_in_dir(dir: &Path, visited: &mut HashSet<PathBuf>, out: &mut Vec<PathBuf>) {
    let Ok(canon) = std::fs::canonicalize(dir) else {
        return;
    };
    if !visited.insert(canon) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    children.sort();
    for path in children {
        if is_vst3_entry(&path) {
            out.push(path);
        } else if path.is_dir() {
            collect_vst3_in_dir(&path, visited, out);
        }
    }
}

/// Enumerate `.vst3` bundles under the given root directories recursively (stopping at `.vst3`).
pub fn vst3_bundle_paths_in(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut visited = HashSet::new();
    let mut out = Vec::new();
    for dir in dirs {
        if is_vst3_entry(dir) && dir.exists() {
            out.push(dir.clone());
        } else {
            collect_vst3_in_dir(dir, &mut visited, &mut out);
        }
    }
    for extra in extra_bundle_list() {
        if extra.exists() && !out.contains(&extra) {
            out.push(extra);
        }
    }
    out
}

/// Enumerate all `.vst3` bundles in [`vst3_search_dirs`] plus any registered extra bundles.
pub fn vst3_bundle_paths() -> Vec<PathBuf> {
    vst3_bundle_paths_in(&vst3_search_dirs())
}

/// Result of resolving a `.vst3` bundle to its runnable Linux `.so`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedVst3Binary {
    pub so_path: PathBuf,
    pub is_yabridge: bool,
}

fn native_linux_arch_dir() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "x86_64-linux"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64-linux"
    } else if cfg!(target_arch = "x86") {
        "i386-linux"
    } else {
        "linux"
    }
}

fn path_or_link_mentions_yabridge(so_path: &Path) -> bool {
    if so_path.to_string_lossy().to_ascii_lowercase().contains("yabridge") {
        return true;
    }
    if let Ok(target) = std::fs::read_link(so_path) {
        if target.to_string_lossy().to_ascii_lowercase().contains("yabridge") {
            return true;
        }
    }
    // Check compact wrapper shared objects for yabridge signature strings.
    if let Ok(meta) = std::fs::metadata(so_path) {
        if meta.len() <= 8 * 1024 * 1024 {
            if let Ok(bytes) = std::fs::read(so_path) {
                if bytes.windows(b"yabridge".len()).any(|w| w == b"yabridge") {
                    return true;
                }
            }
        }
    }
    false
}

fn pick_bundle_so(arch_dir: &Path, bundle: &Path) -> Option<PathBuf> {
    let Ok(entries) = std::fs::read_dir(arch_dir) else {
        return None;
    };
    let mut sos: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("so"))
        .collect();
    sos.sort();
    if sos.is_empty() {
        return None;
    }
    if let Some(stem) = bundle.file_stem() {
        let preferred = arch_dir.join(format!("{}.so", stem.to_string_lossy()));
        if sos.contains(&preferred) {
            return Some(preferred);
        }
    }
    sos.into_iter().next()
}

/// Resolve a `.vst3` bundle (directory or flat file) to a loadable Linux `.so`
/// and diagnose wrong-CPU, unbridged Windows, or macOS bundles cleanly.
pub fn resolve_vst3_binary(bundle: &Path) -> std::result::Result<ResolvedVst3Binary, String> {
    if !bundle.exists() {
        return Err(format!("VST3 bundle not found: {}", bundle.display()));
    }
    if bundle.is_file() {
        crate::binarch::ensure_loadable(bundle).map_err(|e| e.to_string())?;
        return Ok(ResolvedVst3Binary {
            so_path: bundle.to_path_buf(),
            is_yabridge: path_or_link_mentions_yabridge(bundle),
        });
    }

    let contents = bundle.join("Contents");
    let has_win = contents.join("x86_64-win").is_dir()
        || contents.join("Win64").is_dir()
        || contents.join("Win32").is_dir()
        || contents.join("i386-win").is_dir();
    let has_macos = contents.join("MacOS").is_dir();

    let arch_dir = contents.join(native_linux_arch_dir());
    if let Some(so) = pick_bundle_so(&arch_dir, bundle) {
        crate::binarch::ensure_loadable(&so).map_err(|e| e.to_string())?;
        let is_yabridge = has_win || path_or_link_mentions_yabridge(&so);
        return Ok(ResolvedVst3Binary { so_path: so, is_yabridge });
    }

    // Look for any other `*-linux/*.so` to surface an explicit CPU mismatch message.
    if let Ok(entries) = std::fs::read_dir(&contents) {
        let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        dirs.sort();
        for dir in dirs {
            let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.ends_with("-linux") {
                if let Some(so) = pick_bundle_so(&dir, bundle) {
                    if let Some(reason) = crate::binarch::foreign(&so) {
                        return Err(reason);
                    }
                    crate::binarch::ensure_loadable(&so).map_err(|e| e.to_string())?;
                    let is_yabridge = has_win || path_or_link_mentions_yabridge(&so);
                    return Ok(ResolvedVst3Binary { so_path: so, is_yabridge });
                }
            }
        }
    }

    if has_win {
        return Err(format!(
            "{} contains a Windows VST3 plugin with no Linux binary — bridge it with yabridge to use it on Linux",
            bundle.display()
        ));
    }
    if has_macos {
        return Err(format!(
            "{} is a macOS VST3 bundle and cannot run on Linux",
            bundle.display()
        ));
    }
    Err(format!(
        "{} contains no Linux VST3 binary under Contents/{}",
        bundle.display(),
        native_linux_arch_dir()
    ))
}

pub(crate) fn cstr_slice_to_string(slice: &[c_char]) -> String {
    let bytes: Vec<u8> = slice
        .iter()
        .take_while(|&&b| b != 0)
        .map(|&b| b as u8)
        .collect();
    String::from_utf8_lossy(&bytes).trim().to_string()
}

/// Load one VST3 bundle and extract all `Audio Module Class` descriptors.
pub fn scan_vst3_bundle(bundle: &Path) -> Result<Vec<Vst3PluginInfo>> {
    let resolved = resolve_vst3_binary(bundle).map_err(|e| anyhow!("{e}"))?;
    let c_so = CString::new(resolved.so_path.to_string_lossy().as_bytes())
        .context("invalid VST3 binary path")?;

    let dl = unsafe { libc::dlopen(c_so.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
    if dl.is_null() {
        let err = unsafe {
            let p = libc::dlerror();
            if p.is_null() {
                "dlopen failed".to_string()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        bail!("failed to open VST3 binary {}: {err}", resolved.so_path.display());
    }

    let get_factory_sym = unsafe { libc::dlsym(dl, c"GetPluginFactory".as_ptr()) };
    if get_factory_sym.is_null() {
        bail!("VST3 binary {} does not export GetPluginFactory", resolved.so_path.display());
    }
    let module_entry_sym = unsafe { libc::dlsym(dl, c"ModuleEntry".as_ptr()) };
    let module_exit_sym = unsafe { libc::dlsym(dl, c"ModuleExit".as_ptr()) };

    type ModuleEntryFn = unsafe extern "C" fn(*mut c_void) -> bool;
    type ModuleExitFn = unsafe extern "C" fn() -> bool;
    type GetFactoryFn = unsafe extern "system" fn() -> *mut IPluginFactory;

    let entered = if !module_entry_sym.is_null() {
        let entry_fn: ModuleEntryFn = unsafe { std::mem::transmute(module_entry_sym) };
        if !unsafe { entry_fn(dl) } {
            bail!("ModuleEntry returned false for {}", resolved.so_path.display());
        }
        true
    } else {
        false
    };

    let res = (|| -> Result<Vec<Vst3PluginInfo>> {
        let get_factory: GetFactoryFn = unsafe { std::mem::transmute(get_factory_sym) };
        let raw_factory = unsafe { get_factory() };
        let factory = unsafe { ComPtr::<IPluginFactory>::from_raw(raw_factory) }
            .ok_or_else(|| anyhow!("GetPluginFactory returned null for {}", bundle.display()))?;

        let mut f_info: PFactoryInfo = unsafe { std::mem::zeroed() };
        let factory_vendor = if unsafe { factory.getFactoryInfo(&mut f_info) } == kResultOk {
            cstr_slice_to_string(&f_info.vendor)
        } else {
            String::new()
        };

        let factory2 = factory.cast::<IPluginFactory2>();
        let count = unsafe { factory.countClasses() }.max(0);
        let mut out = Vec::new();

        for idx in 0..count {
            if let Some(ref f2) = factory2 {
                let mut info2: PClassInfo2 = unsafe { std::mem::zeroed() };
                if unsafe { f2.getClassInfo2(idx, &mut info2) } == kResultOk {
                    let cat = cstr_slice_to_string(&info2.category);
                    if cat == "Audio Module Class" {
                        let name = cstr_slice_to_string(&info2.name);
                        let v = cstr_slice_to_string(&info2.vendor);
                        let vendor = if v.is_empty() { factory_vendor.clone() } else { v };
                        let version = cstr_slice_to_string(&info2.version);
                        let raw_sub = cstr_slice_to_string(&info2.subCategories);
                        let sub_categories = raw_sub
                            .split('|')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .collect();
                        out.push(Vst3PluginInfo {
                            cid: format_tuid(&info2.cid),
                            name,
                            vendor,
                            version,
                            sub_categories,
                            bundle_path: bundle.to_path_buf(),
                            is_yabridge: resolved.is_yabridge,
                        });
                        continue;
                    }
                }
            }
            let mut info: PClassInfo = unsafe { std::mem::zeroed() };
            if unsafe { factory.getClassInfo(idx, &mut info) } == kResultOk {
                let cat = cstr_slice_to_string(&info.category);
                if cat == "Audio Module Class" {
                    out.push(Vst3PluginInfo {
                        cid: format_tuid(&info.cid),
                        name: cstr_slice_to_string(&info.name),
                        vendor: factory_vendor.clone(),
                        version: String::new(),
                        sub_categories: Vec::new(),
                        bundle_path: bundle.to_path_buf(),
                        is_yabridge: resolved.is_yabridge,
                    });
                }
            }
        }
        Ok(out)
    })();

    if entered && !module_exit_sym.is_null() {
        let exit_fn: ModuleExitFn = unsafe { std::mem::transmute(module_exit_sym) };
        let _ = unsafe { exit_fn() };
    }
    // Never dlclose: C++ VST3 libraries & yabridge keep static state that races on unload.
    res
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct BundleStamp {
    mtime_secs: i64,
    mtime_nanos: u32,
    size: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CachedVst3Bundle {
    stamp: BundleStamp,
    plugins: Vec<Vst3PluginInfo>,
    error: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Vst3ScanCache {
    version: u32,
    #[serde(default)]
    app_version: String,
    entries: BTreeMap<String, CachedVst3Bundle>,
}

impl Default for Vst3ScanCache {
    fn default() -> Self {
        Self {
            version: VST3_SCAN_CACHE_VERSION,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            entries: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug)]
enum ScannerCommand {
    #[cfg_attr(test, allow(dead_code))]
    CurrentExe,
    #[cfg_attr(not(test), allow(dead_code))]
    Custom { program: PathBuf, args: Vec<String> },
    #[cfg(test)]
    InProcess,
}

impl ScannerCommand {
    fn default_for_process() -> Self {
        #[cfg(test)]
        {
            Self::InProcess
        }
        #[cfg(not(test))]
        {
            Self::CurrentExe
        }
    }
}

fn file_stamp(path: &Path) -> Option<BundleStamp> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let (mtime_secs, mtime_nanos) = match modified.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
        Err(e) => {
            let d = e.duration();
            (-(d.as_secs() as i64), d.subsec_nanos())
        }
    };
    Some(BundleStamp {
        mtime_secs,
        mtime_nanos,
        size: meta.len(),
    })
}

fn vst3_scan_cache_path() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    let xdg = std::env::var_os("XDG_CACHE_HOME");
    let home = std::env::var_os("HOME");
    let base = xdg
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.filter(|v| !v.is_empty()).map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("kabelsalat").join("vst3-scan-v1.json"))
}

fn load_scan_cache(path: &Path) -> Vst3ScanCache {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vst3ScanCache::default();
    };
    match serde_json::from_str::<Vst3ScanCache>(&raw) {
        Ok(mut c) if c.version == VST3_SCAN_CACHE_VERSION => {
            if c.app_version != env!("CARGO_PKG_VERSION") {
                c.entries.retain(|_, e| e.error.is_none());
                c.app_version = env!("CARGO_PKG_VERSION").to_string();
            }
            c
        }
        _ => Vst3ScanCache::default(),
    }
}

fn save_scan_cache(path: &Path, cache: &Vst3ScanCache) -> Result<()> {
    let Some(parent) = path.parent() else {
        bail!("cache path has no parent: {}", path.display());
    };
    std::fs::create_dir_all(parent)?;
    let body = serde_json::to_string_pretty(cache)?;
    let seq = SCAN_CACHE_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".vst3-scan.{}.{seq}.tmp", std::process::id()));
    std::fs::write(&tmp, body)?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

/// Move fd 1 to a duplicate fd for structured JSON output and route process
/// stdout to stderr so noisy plugin C++ constructors cannot corrupt our output.
fn reroute_stdout_to_stderr() -> Result<std::fs::File> {
    use std::os::unix::io::FromRawFd;
    unsafe {
        let saved = libc::dup(libc::STDOUT_FILENO);
        if saved < 0 {
            return Err(std::io::Error::last_os_error()).context("dup(stdout) failed");
        }
        let _ = libc::fcntl(saved, libc::F_SETFD, libc::FD_CLOEXEC);
        if libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO) < 0 {
            let e = std::io::Error::last_os_error();
            libc::close(saved);
            return Err(e).context("dup2(stderr, stdout) failed");
        }
        Ok(std::fs::File::from_raw_fd(saved))
    }
}

/// CLI entrypoint for `kabelsalat scan-vst3 <bundle-path>`.
pub fn run_scan_vst3(path: &Path) -> Result<()> {
    use std::io::Write;
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
        libc::alarm(30);
    }
    let mut json_out = reroute_stdout_to_stderr()?;
    let plugins = scan_vst3_bundle(path)?;
    let json = serde_json::to_string(&plugins)?;
    writeln!(json_out, "{json}")?;
    json_out.flush()?;
    Ok(())
}

fn parse_scan_output(stdout: &[u8]) -> Result<Vec<Vst3PluginInfo>> {
    let text = std::str::from_utf8(stdout).context("vst3 scanner stdout is not UTF-8")?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        bail!("vst3 scanner produced empty stdout");
    }
    serde_json::from_str::<Vec<Vst3PluginInfo>>(trimmed).context("vst3 scanner produced invalid JSON")
}

fn spawn_pipe_reader<R: std::io::Read + Send + 'static>(
    reader: Option<R>,
    keep: usize,
    tail: bool,
) -> std::sync::mpsc::Receiver<(Vec<u8>, bool)> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (mut buf, mut truncated) = (Vec::new(), false);
        if let Some(mut r) = reader {
            let mut chunk = [0u8; 8192];
            loop {
                let n = match r.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                if tail {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.len() > keep {
                        buf.drain(..buf.len() - keep);
                        truncated = true;
                    }
                } else if buf.len() < keep {
                    let take = n.min(keep - buf.len());
                    buf.extend_from_slice(&chunk[..take]);
                    truncated |= take < n;
                } else {
                    truncated = true;
                }
            }
        }
        let _ = tx.send((buf, truncated));
    });
    rx
}

#[derive(Debug)]
enum ScanFail {
    Verdict(String),
    Setup(String),
}

impl std::fmt::Display for ScanFail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScanFail::Verdict(m) | ScanFail::Setup(m) => f.write_str(m),
        }
    }
}

fn kill_child_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn run_scan_subprocess(
    program: &Path,
    prefix_args: &[String],
    bundle: &Path,
    timeout: Duration,
) -> Result<Vec<Vst3PluginInfo>, ScanFail> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(program);
    cmd.args(prefix_args)
        .arg(bundle)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| ScanFail::Setup(format!("failed to spawn vst3 scanner {}: {e}", program.display())))?;
    let stdout_rx = spawn_pipe_reader(child.stdout.take(), SCAN_STDOUT_MAX, false);
    let stderr_rx = spawn_pipe_reader(child.stderr.take(), SCAN_ERR_TAIL, true);

    let deadline = Instant::now() + timeout;
    let timed_out = || ScanFail::Verdict(format!("timed out after {:.1}s", timeout.as_secs_f32()));
    let pid = child.id() as libc::pid_t;
    let info = loop {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let r = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if r != 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            kill_child_tree(&mut child);
            return Err(ScanFail::Setup(format!("failed waiting on vst3 scanner: {e}")));
        }
        if unsafe { info.si_pid() } != 0 {
            break info;
        }
        if Instant::now() >= deadline {
            kill_child_tree(&mut child);
            return Err(timed_out());
        }
        std::thread::sleep(Duration::from_millis(15));
    };

    let remaining = || deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(200));
    let stdout = stdout_rx.recv_timeout(remaining());
    let exited_ok = info.si_code == libc::CLD_EXITED && unsafe { info.si_status() } == 0;
    let stderr = if exited_ok { None } else { stderr_rx.recv_timeout(remaining()).ok() };
    unsafe { libc::kill(-pid, libc::SIGKILL) };
    let status = child.wait().map_err(|e| ScanFail::Setup(format!("failed reaping vst3 scanner: {e}")))?;
    if !status.success() {
        let (err, truncated) = stderr.unwrap_or_default();
        let err_tail = String::from_utf8_lossy(&err).trim().to_string();
        if err_tail.is_empty() {
            return Err(ScanFail::Verdict(format!("scanner exited with {status}")));
        }
        let more = if truncated { "…" } else { "" };
        return Err(ScanFail::Verdict(format!("scanner exited with {status}: {more}{err_tail}")));
    }
    let Ok((stdout, stdout_truncated)) = stdout else {
        return Err(timed_out());
    };
    if stdout_truncated {
        return Err(ScanFail::Verdict(format!("scanner stdout exceeded {SCAN_STDOUT_MAX} bytes")));
    }
    parse_scan_output(&stdout).map_err(|e| ScanFail::Verdict(format!("{e:#}")))
}

fn scan_one_bundle(
    cmd: &ScannerCommand,
    bundle: &Path,
    timeout: Duration,
) -> Result<Vec<Vst3PluginInfo>, ScanFail> {
    match cmd {
        ScannerCommand::CurrentExe => {
            let args = vec!["scan-vst3".to_string()];
            run_scan_subprocess(Path::new("/proc/self/exe"), &args, bundle, timeout)
        }
        ScannerCommand::Custom { program, args } => {
            run_scan_subprocess(program, args, bundle, timeout)
        }
        #[cfg(test)]
        ScannerCommand::InProcess => scan_vst3_bundle(bundle).map_err(|e| ScanFail::Verdict(format!("{e:#}"))),
    }
}

fn scan_bundles_with(
    paths: &[PathBuf],
    cmd: &ScannerCommand,
    cache_path: Option<&Path>,
    timeout: Duration,
    parallelism: usize,
    retry_failed: bool,
) -> (Vec<Option<Vec<Vst3PluginInfo>>>, Vec<String>) {
    let mut cache = cache_path.map(load_scan_cache).unwrap_or_default();
    let mut per_bundle: Vec<Option<Vec<Vst3PluginInfo>>> = vec![None; paths.len()];
    let mut uncached: Vec<(usize, &PathBuf, Option<BundleStamp>)> = Vec::new();
    let mut errors = Vec::new();

    for (idx, path) in paths.iter().enumerate() {
        let resolved = match resolve_vst3_binary(path) {
            Ok(r) => r,
            Err(reason) => {
                eprintln!("skip VST3 bundle {}: {reason}", path.display());
                errors.push(reason);
                continue;
            }
        };
        let stamp = file_stamp(&resolved.so_path).or_else(|| file_stamp(path));
        let key = path.to_string_lossy();
        if let (Some(st), Some(cached)) = (&stamp, cache.entries.get(key.as_ref())) {
            if &cached.stamp == st {
                match &cached.error {
                    Some(_) if retry_failed => {}
                    Some(e) => {
                        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("plugin.vst3");
                        eprintln!("skip VST3 bundle {} (cached failure): {e}", path.display());
                        errors.push(format!("{name}: {e}"));
                        continue;
                    }
                    None => {
                        per_bundle[idx] = Some(cached.plugins.clone());
                        continue;
                    }
                }
            }
        }
        uncached.push((idx, path, stamp));
    }

    if !uncached.is_empty() {
        let workers = uncached.len().min(parallelism.max(1));
        let cursor = AtomicUsize::new(0);
        let results: Mutex<Vec<(usize, Option<BundleStamp>, Result<Vec<Vst3PluginInfo>, ScanFail>)>> =
            Mutex::new(Vec::with_capacity(uncached.len()));

        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| loop {
                    let i = cursor.fetch_add(1, Ordering::Relaxed);
                    let Some(&(idx, path, ref stamp)) = uncached.get(i) else {
                        break;
                    };
                    let res = scan_one_bundle(cmd, path, timeout);
                    results.lock().unwrap().push((idx, stamp.clone(), res));
                });
            }
        });

        let mut cache_dirty = false;
        for (idx, stamp, res) in results.into_inner().unwrap() {
            let path = &paths[idx];
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("plugin.vst3");
            let key = path.to_string_lossy().into_owned();
            match res {
                Ok(plugins) => {
                    per_bundle[idx] = Some(plugins.clone());
                    if let Some(st) = stamp {
                        cache.entries.insert(
                            key,
                            CachedVst3Bundle { stamp: st, plugins, error: None },
                        );
                        cache_dirty = true;
                    }
                }
                Err(fail) => {
                    eprintln!("skip VST3 bundle {}: {fail}", path.display());
                    errors.push(format!("{name}: {fail}"));
                    if let (ScanFail::Verdict(msg), Some(st)) = (fail, stamp) {
                        cache.entries.insert(
                            key,
                            CachedVst3Bundle { stamp: st, plugins: Vec::new(), error: Some(msg) },
                        );
                        cache_dirty = true;
                    }
                }
            }
        }
        if cache_dirty {
            if let Some(cp) = cache_path {
                if let Err(e) = save_scan_cache(cp, &cache) {
                    eprintln!("warning: could not save VST3 scan cache {}: {e}", cp.display());
                }
            }
        }
    }

    (per_bundle, errors)
}

fn default_parallelism() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8)
}

fn flatten_results(per_bundle: Vec<Option<Vec<Vst3PluginInfo>>>) -> Vec<Vst3PluginInfo> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for entry in per_bundle.into_iter().flatten() {
        for p in entry {
            if seen.insert(p.cid.clone()) {
                out.push(p);
            }
        }
    }
    out
}

/// Discover all VST3 plugins across search paths out of process (cached).
pub fn list_vst3_plugins() -> Vec<Vst3PluginInfo> {
    let paths = vst3_bundle_paths();
    let cache_path = vst3_scan_cache_path();
    let (res, _) = scan_bundles_with(
        &paths,
        &ScannerCommand::default_for_process(),
        cache_path.as_deref(),
        VST3_SCAN_TIMEOUT,
        default_parallelism(),
        false,
    );
    flatten_results(res)
}

/// Same as [`list_vst3_plugins`], retrying previously failed bundles.
pub fn list_vst3_plugins_retry_failed() -> Vec<Vst3PluginInfo> {
    let paths = vst3_bundle_paths();
    let cache_path = vst3_scan_cache_path();
    let (res, _) = scan_bundles_with(
        &paths,
        &ScannerCommand::default_for_process(),
        cache_path.as_deref(),
        VST3_SCAN_TIMEOUT,
        default_parallelism(),
        true,
    );
    flatten_results(res)
}

/// List VST3 plugins and return diagnostics for any unloadable bundles
/// (e.g., wrong CPU, unbridged Windows `.vst3`, macOS bundle, or scanner crash).
pub fn list_vst3_plugins_with_errors() -> (Vec<Vst3PluginInfo>, Vec<String>) {
    let paths = vst3_bundle_paths();
    let cache_path = vst3_scan_cache_path();
    let (res, errors) = scan_bundles_with(
        &paths,
        &ScannerCommand::default_for_process(),
        cache_path.as_deref(),
        VST3_SCAN_TIMEOUT,
        default_parallelism(),
        false,
    );
    (flatten_results(res), errors)
}

/// Locate the `.vst3` bundle and metadata for one VST3 class CID.
pub fn bundle_with_cid(target_cid: &str) -> Option<Vst3PluginInfo> {
    let target = strip_vst3_key(target_cid).unwrap_or(target_cid);
    for extra in extra_bundle_list() {
        if let Ok(list) = scan_vst3_bundle(&extra) {
            if let Some(found) = list.into_iter().find(|p| p.cid.eq_ignore_ascii_case(target)) {
                return Some(found);
            }
        }
    }
    list_vst3_plugins()
        .into_iter()
        .find(|p| p.cid.eq_ignore_ascii_case(target))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tuid_hex_round_trips() {
        let raw = vst3::uid(0x12345678, 0x9ABCDEF0, 0x11223344, 0x55667788);
        let hex = format_tuid(&raw);
        assert_eq!(hex.len(), 32);
        let parsed = parse_tuid(&hex).unwrap();
        assert_eq!(parsed, raw);
        assert!(parse_tuid("too-short").is_none());
    }

    #[test]
    fn voice_key_round_trips() {
        let key = voice_key("ABCDEF0123456789ABCDEF0123456789");
        assert!(is_vst3_key(&key));
        assert_eq!(strip_vst3_key(&key), Some("ABCDEF0123456789ABCDEF0123456789"));
        assert!(!is_vst3_key("org.surge-synthesizer.surge-xt"));
    }

    #[test]
    fn resolve_vst3_binary_rejects_unbridged_windows_and_foreign_cpu() {
        let base = std::env::temp_dir().join(format!("kabelsalat-vst3-resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);

        // Unbridged Windows VST3 bundle
        let win_bundle = base.join("WinOnly.vst3");
        std::fs::create_dir_all(win_bundle.join("Contents/x86_64-win")).unwrap();
        std::fs::write(win_bundle.join("Contents/x86_64-win/WinOnly.vst3"), b"MZdummy").unwrap();
        let win_err = resolve_vst3_binary(&win_bundle).unwrap_err();
        assert!(win_err.contains("yabridge"), "expected yabridge hint: {win_err}");

        // macOS VST3 bundle
        let mac_bundle = base.join("MacOnly.vst3");
        std::fs::create_dir_all(mac_bundle.join("Contents/MacOS")).unwrap();
        let mac_err = resolve_vst3_binary(&mac_bundle).unwrap_err();
        assert!(mac_err.contains("macOS"), "expected macOS error: {mac_err}");

        // Foreign Linux CPU ELF
        let cpu_bundle = base.join("ForeignCpu.vst3");
        let arch_dir = cpu_bundle.join("Contents").join(native_linux_arch_dir());
        std::fs::create_dir_all(&arch_dir).unwrap();
        let fake_so = arch_dir.join("ForeignCpu.so");
        std::fs::write(&fake_so, crate::binarch::fake_elf(crate::binarch::other_machine().0)).unwrap();
        let cpu_err = resolve_vst3_binary(&cpu_bundle).unwrap_err();
        assert!(cpu_err.contains("built for"), "expected foreign CPU error: {cpu_err}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn recursive_bundle_discovery_stops_at_vst3_dirs() {
        let base = std::env::temp_dir().join(format!("kabelsalat-vst3-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let nested_a = base.join("yabridge").join("VendorA").join("Synth.vst3");
        std::fs::create_dir_all(nested_a.join("Contents/x86_64-linux")).unwrap();
        std::fs::create_dir_all(nested_a.join("Contents/Resources/Inner.vst3")).unwrap();
        let nested_b = base.join("Flat.vst3");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(&nested_b, b"").unwrap();

        let found = vst3_bundle_paths_in(&[base.clone()]);
        assert_eq!(found, vec![nested_b, nested_a]);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn subprocess_scan_handles_cache_timeout_and_crash_isolation() {
        let base = std::env::temp_dir().join(format!("kabelsalat-vst3-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let b1 = base.join("Good.vst3");
        let b2 = base.join("Bad.vst3");
        let script = base.join("fake-scan.sh");
        let cache_file = base.join("cache.json");

        // Create loadable native ELF stubs just to satisfy resolve_vst3_binary in the cache test.
        #[cfg(target_arch = "x86_64")]
        const NATIVE_EM: u16 = 62;
        #[cfg(target_arch = "aarch64")]
        const NATIVE_EM: u16 = 183;
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        const NATIVE_EM: u16 = 62;

        std::fs::write(&b1, crate::binarch::fake_elf(NATIVE_EM)).unwrap();
        std::fs::write(&b2, crate::binarch::fake_elf(NATIVE_EM)).unwrap();

        std::fs::write(
            &script,
            "#!/bin/sh\ncase \"$1\" in\n*Good.vst3) printf '[{\"cid\":\"00000000000000000000000000000001\",\"name\":\"FakeGood\",\"vendor\":\"Test\",\"version\":\"1.0\",\"sub_categories\":[\"Fx\"],\"bundle_path\":\"'\"$1\"'\",\"is_yabridge\":false}]\\n' ;;\n*) exit 139 ;;\nesac\n",
        ).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let cmd = ScannerCommand::Custom { program: script.clone(), args: vec![] };
        let (res, errs) = scan_bundles_with(
            &[b1.clone(), b2.clone()],
            &cmd,
            Some(&cache_file),
            Duration::from_secs(2),
            2,
            false,
        );
        assert_eq!(res[0].as_ref().unwrap()[0].name, "FakeGood");
        assert!(res[1].is_none());
        assert_eq!(errs.len(), 1);
        assert!(cache_file.exists());

        // Second run with missing script hits the cache cleanly for both good and cached-failure!
        std::fs::remove_file(&script).unwrap();
        let (res2, errs2) = scan_bundles_with(
            &[b1, b2],
            &cmd,
            Some(&cache_file),
            Duration::from_secs(2),
            2,
            false,
        );
        assert_eq!(res2[0].as_ref().unwrap()[0].name, "FakeGood");
        assert!(res2[1].is_none());
        assert_eq!(errs2.len(), 1);

        let _ = std::fs::remove_dir_all(&base);
    }
}
