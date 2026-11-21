//! Crash reports (#279). A fatal signal (SIGSEGV, SIGBUS, SIGILL, SIGFPE,
//! SIGABRT, SIGTRAP) or a panic writes a plain-text report into
//! `crash_dir()`. Nothing is sent anywhere. A report holds the version, the
//! cause, the thread, the plugin binaries loaded, the last engine commands
//! (their names only) and the stack as module+offset frames; a panic adds
//! its message and the symbolized backtrace. It names no folder or file of
//! the user: modules and plugins appear by file name only, and every path in
//! a panic's text is cut down to the source file or replaced by `<path>`.
//!
//! The signal handler allocates nothing and takes no lock. Everything it
//! prints was prepared by `install` (folder, version, clock) or is read
//! lock-free: the command ring of atomics and `/proc/self/maps` through
//! open/read. Its buffers are statics claimed with one atomic swap, so a
//! small signal stack is enough.

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;

use libc::c_int;

const SIGNALS: [c_int; 6] = [libc::SIGSEGV, libc::SIGBUS, libc::SIGILL, libc::SIGFPE, libc::SIGABRT, libc::SIGTRAP];
/// Frames a report shows.
const FRAMES: usize = 48;
/// Commands a report shows (runs of one command count as one).
const RING: usize = 32;
/// Bytes of a command name kept (longer names are cut).
const NAME: usize = 24;
/// Longest crash folder path the handler can open a report in.
const DIR_MAX: usize = 400;
/// Bytes of a module name kept per frame.
const MODULE: usize = 48;
/// The signal stack of the main thread (other std threads have std's).
const ALT_STACK: usize = 64 * 1024;
/// Reports kept; older ones are deleted at start.
const KEEP_REPORTS: usize = 20;

/// What `install` prepared for the handler.
struct Setup {
    dir: [u8; DIR_MAX],
    dir_len: usize,
    /// Seconds east of UTC at start, so report names use local time (rule 54).
    utc_offset: i64,
    start_ns: u64,
}

static SETUP: OnceLock<Setup> = OnceLock::new();
static COMMANDS: Ring = Ring::new();
/// Held while a report is written: the scratch buffers are one static.
static WRITING: AtomicBool = AtomicBool::new(false);
/// A panic was reported; the abort that may follow writes no second report.
static PANIC_REPORTED: AtomicBool = AtomicBool::new(false);

struct Scratch {
    chunk: [u8; 1024],
    line: [u8; LINE],
    path: [u8; DIR_MAX + 64],
    frames: [*mut c_void; FRAMES],
    module: [[u8; MODULE]; FRAMES],
    module_len: [usize; FRAMES],
    offset: [u64; FRAMES],
}

struct ScratchCell(UnsafeCell<Scratch>);
// SAFETY: only the holder of `WRITING` touches the cell.
unsafe impl Sync for ScratchCell {}

static SCRATCH: ScratchCell = ScratchCell(UnsafeCell::new(Scratch {
    chunk: [0; 1024],
    line: [0; LINE],
    path: [0; DIR_MAX + 64],
    frames: [std::ptr::null_mut(); FRAMES],
    module: [[0; MODULE]; FRAMES],
    module_len: [0; FRAMES],
    offset: [0; FRAMES],
}));

/// Where reports go: `$XDG_STATE_HOME/kabelsalat/crashes`, else
/// `~/.local/state/kabelsalat/crashes`. Tests never write there (rule 37).
pub fn crash_dir() -> PathBuf {
    #[cfg(test)]
    return std::env::temp_dir().join(format!("kabelsalat-test-crashes-{}", std::process::id()));
    #[cfg(not(test))]
    {
        let state = std::env::var_os("XDG_STATE_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into())).join(".local").join("state"));
        state.join("kabelsalat").join("crashes")
    }
}

/// The reports in `dir`, oldest first (their names sort by time).
fn reports(dir: &Path) -> Vec<PathBuf> {
    let mut all: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    all.retain(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("crash-") && n.ends_with(".txt")));
    all.sort();
    all
}

/// The report the process `pid` wrote into `dir`, if any.
pub fn report_of(dir: &Path, pid: u32) -> Option<PathBuf> {
    let suffix = format!("-{pid}.txt");
    reports(dir).into_iter().rfind(|p| p.to_string_lossy().ends_with(&suffix))
}

/// Delete all but the newest `keep` reports.
fn prune(dir: &Path, keep: usize) {
    let all = reports(dir);
    for old in &all[..all.len().saturating_sub(keep)] {
        let _ = std::fs::remove_file(old);
    }
}

/// Install the signal handlers and the panic hook. Call once, early in
/// `main`, before threads start (the signal stack is the calling thread's).
pub fn install() {
    let dir = crash_dir();
    let _ = std::fs::create_dir_all(&dir);
    prune(&dir, KEEP_REPORTS);
    let bytes = dir.as_os_str().as_encoded_bytes();
    if bytes.len() > DIR_MAX {
        eprintln!("crash: report folder path too long, reports go to stderr");
    }
    let mut setup = Setup { dir: [0; DIR_MAX], dir_len: bytes.len().min(DIR_MAX), utc_offset: local_utc_offset(), start_ns: monotonic_ns() };
    setup.dir[..setup.dir_len].copy_from_slice(&bytes[..setup.dir_len]);
    if bytes.len() > DIR_MAX {
        setup.dir_len = 0;
    }
    if SETUP.set(setup).is_err() {
        return;
    }
    // glibc's first backtrace() loads libgcc_s (allocating); do it here.
    #[cfg(target_env = "gnu")]
    unsafe {
        let mut warm = [std::ptr::null_mut(); 4];
        libc::backtrace(warm.as_mut_ptr(), warm.len() as c_int);
    }
    unsafe {
        let stack = libc::mmap(std::ptr::null_mut(), ALT_STACK, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0);
        if stack != libc::MAP_FAILED {
            let ss = libc::stack_t { ss_sp: stack, ss_flags: 0, ss_size: ALT_STACK };
            libc::sigaltstack(&ss, std::ptr::null_mut());
        }
        for sig in SIGNALS {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_fatal as extern "C" fn(c_int, *mut libc::siginfo_t, *mut c_void) as libc::sighandler_t;
            sa.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK | libc::SA_RESETHAND;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        on_panic(info);
        previous(info);
    }));
}

/// Remember an engine command for the report: its variant name only, never
/// its values (rig and session names travel in commands). Lock-free.
pub fn note_command(cmd: &impl Debug) {
    let Some(setup) = SETUP.get() else { return };
    let mut name = [0u8; NAME];
    let len = variant_name(cmd, &mut name);
    let ms = (monotonic_ns().saturating_sub(setup.start_ns) / 1_000_000).min(u32::MAX as u64) as u32;
    COMMANDS.note(&name[..len], ms);
}

/// The name a derived `Debug` prints first: formatting stops at the first
/// byte that is not part of an identifier, before any field is formatted.
fn variant_name(value: &impl Debug, out: &mut [u8; NAME]) -> usize {
    struct Name<'a> {
        out: &'a mut [u8; NAME],
        len: usize,
    }
    impl std::fmt::Write for Name<'_> {
        fn write_str(&mut self, s: &str) -> std::fmt::Result {
            for b in s.bytes() {
                if !(b.is_ascii_alphanumeric() || b == b'_') || self.len == NAME {
                    return Err(std::fmt::Error);
                }
                self.out[self.len] = b;
                self.len += 1;
            }
            Ok(())
        }
    }
    let mut w = Name { out, len: 0 };
    let _ = std::fmt::write(&mut w, format_args!("{value:?}"));
    w.len
}

// -- the command ring --------------------------------------------------------

struct Slot {
    name: [AtomicU64; NAME / 8],
    count: AtomicU32,
    at_ms: AtomicU32,
}

/// The last `RING` commands; a command repeated back to back counts up its
/// slot. Writers claim slots with `fetch_add`, readers read atomics: a race
/// can only miscount a line, never block or tear memory.
struct Ring {
    slots: [Slot; RING],
    next: AtomicUsize,
}

impl Ring {
    const fn new() -> Self {
        #[allow(clippy::declare_interior_mutable_const)]
        const EMPTY: Slot = Slot { name: [const { AtomicU64::new(0) }; NAME / 8], count: AtomicU32::new(0), at_ms: AtomicU32::new(0) };
        Ring { slots: [EMPTY; RING], next: AtomicUsize::new(0) }
    }

    fn note(&self, name: &[u8], at_ms: u32) {
        let mut words = [0u64; NAME / 8];
        for (i, b) in name.iter().take(NAME).enumerate() {
            words[i / 8] |= (*b as u64) << (8 * (i % 8));
        }
        let next = self.next.load(Ordering::Acquire);
        if next > 0 {
            let last = &self.slots[(next - 1) % RING];
            if last.name.iter().zip(words).all(|(w, v)| w.load(Ordering::Relaxed) == v) {
                last.count.fetch_add(1, Ordering::Relaxed);
                last.at_ms.store(at_ms, Ordering::Relaxed);
                return;
            }
        }
        let slot = &self.slots[self.next.fetch_add(1, Ordering::AcqRel) % RING];
        slot.count.store(0, Ordering::Relaxed);
        for (w, v) in slot.name.iter().zip(words) {
            w.store(v, Ordering::Relaxed);
        }
        slot.at_ms.store(at_ms, Ordering::Relaxed);
        slot.count.store(1, Ordering::Release);
    }

    /// Oldest first: (name, count, ms since start of the last one).
    fn each(&self, mut f: impl FnMut(&[u8], u32, u32)) {
        let next = self.next.load(Ordering::Acquire);
        for i in next.saturating_sub(RING)..next {
            let slot = &self.slots[i % RING];
            let count = slot.count.load(Ordering::Acquire);
            if count == 0 {
                continue;
            }
            let mut name = [0u8; NAME];
            for (i, w) in slot.name.iter().enumerate() {
                name[i * 8..i * 8 + 8].copy_from_slice(&w.load(Ordering::Relaxed).to_le_bytes());
            }
            let len = name.iter().position(|&b| b == 0).unwrap_or(NAME);
            f(&name[..len], count, slot.at_ms.load(Ordering::Relaxed));
        }
    }
}

// -- text without allocation -------------------------------------------------

trait Out {
    fn put(&mut self, bytes: &[u8]);
}

struct Fd(c_int);

impl Out for Fd {
    fn put(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let n = unsafe { libc::write(self.0, bytes.as_ptr().cast(), bytes.len()) };
            if n <= 0 {
                return;
            }
            bytes = &bytes[n as usize..];
        }
    }
}

/// A buffered writer into an `Out`.
struct Text<'a, O: Out> {
    out: O,
    buf: &'a mut [u8],
    len: usize,
}

impl<'a, O: Out> Text<'a, O> {
    fn new(out: O, buf: &'a mut [u8]) -> Self {
        Text { out, buf, len: 0 }
    }

    fn bytes(&mut self, b: &[u8]) -> &mut Self {
        for &c in b {
            if self.len == self.buf.len() {
                self.flush();
            }
            self.buf[self.len] = c;
            self.len += 1;
        }
        self
    }

    fn str(&mut self, s: &str) -> &mut Self {
        self.bytes(s.as_bytes())
    }

    fn dec(&mut self, mut n: u64) -> &mut Self {
        let mut d = [0u8; 20];
        let mut i = d.len();
        loop {
            i -= 1;
            d[i] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        self.bytes(&d[i..])
    }

    /// `n` with at least `width` digits.
    fn dec_pad(&mut self, n: u64, width: usize) -> &mut Self {
        let mut digits = 1;
        let mut m = n;
        while m >= 10 {
            m /= 10;
            digits += 1;
        }
        for _ in digits..width {
            self.bytes(b"0");
        }
        self.dec(n)
    }

    fn hex(&mut self, n: u64) -> &mut Self {
        self.str("0x");
        let mut d = [0u8; 16];
        let mut i = d.len();
        let mut m = n;
        loop {
            i -= 1;
            d[i] = b"0123456789abcdef"[(m & 15) as usize];
            m >>= 4;
            if m == 0 {
                break;
            }
        }
        self.bytes(&d[i..])
    }

    /// Milliseconds as seconds with three decimals.
    fn secs(&mut self, ms: u64) -> &mut Self {
        self.dec(ms / 1000).bytes(b".").dec_pad(ms % 1000, 3)
    }

    fn flush(&mut self) {
        self.out.put(&self.buf[..self.len]);
        self.len = 0;
    }
}

// -- /proc/self/maps ---------------------------------------------------------

/// Bytes of a maps line kept: the fields and the end of the path (where the
/// file name is) survive any length.
const LINE: usize = 512;
const LINE_HEAD: usize = 80;

struct MapLine<'a> {
    start: u64,
    end: u64,
    offset: u64,
    path: &'a [u8],
}

/// Read maps text through `read` (bytes read into the slice, 0 at the end)
/// and call `f` per line.
fn scan_maps(mut read: impl FnMut(&mut [u8]) -> usize, chunk: &mut [u8], line: &mut [u8; LINE], mut f: impl FnMut(&MapLine)) {
    let mut len = 0;
    loop {
        let n = read(chunk);
        if n == 0 {
            break;
        }
        for &b in &chunk[..n] {
            if b == b'\n' {
                if let Some(l) = parse_map_line(&line[..len]) {
                    f(&l);
                }
                len = 0;
            } else if len < LINE {
                line[len] = b;
                len += 1;
            } else {
                line.copy_within(LINE_HEAD + 1.., LINE_HEAD);
                line[LINE - 1] = b;
            }
        }
    }
    if len > 0 {
        if let Some(l) = parse_map_line(&line[..len]) {
            f(&l);
        }
    }
}

fn parse_map_line(line: &[u8]) -> Option<MapLine<'_>> {
    let mut fields = line.splitn(6, |&b| b == b' ');
    let range = fields.next()?;
    let dash = range.iter().position(|&b| b == b'-')?;
    let start = parse_hex(&range[..dash])?;
    let end = parse_hex(&range[dash + 1..])?;
    let _perms = fields.next()?;
    let offset = parse_hex(fields.next()?)?;
    let _dev = fields.next()?;
    let _inode = fields.next()?;
    let rest = fields.next().unwrap_or(&[]);
    let skip = rest.iter().position(|&b| b != b' ').unwrap_or(rest.len());
    Some(MapLine { start, end, offset, path: &rest[skip..] })
}

fn parse_hex(s: &[u8]) -> Option<u64> {
    if s.is_empty() || s.len() > 16 {
        return None;
    }
    s.iter().try_fold(0u64, |acc, &b| Some(acc << 4 | (b as char).to_digit(16)? as u64))
}

/// The file name of a mapped path, without " (deleted)".
fn file_name(path: &[u8]) -> &[u8] {
    let path = path.strip_suffix(b" (deleted)").unwrap_or(path);
    match path.iter().rposition(|&b| b == b'/') {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

/// The plugin bundle a mapped file belongs to: the path component that ends
/// in `.clap`, `.lv2` or `.vst3`.
fn plugin_bundle(path: &[u8]) -> Option<&[u8]> {
    path.split(|&b| b == b'/').find(|c| [&b".clap"[..], b".lv2", b".vst3"].iter().any(|ext| c.len() > ext.len() && c.ends_with(ext)))
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, &b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

fn open_maps_reader() -> Option<impl FnMut(&mut [u8]) -> usize> {
    let fd = unsafe { libc::open(c"/proc/self/maps".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    let mut done = false;
    Some(move |buf: &mut [u8]| {
        if done {
            return 0;
        }
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            done = true;
            unsafe { libc::close(fd) };
            return 0;
        }
        n as usize
    })
}

/// "plugins loaded:" from the mapped files, one bundle per line.
fn write_plugins<O: Out>(t: &mut Text<O>, maps: impl FnMut(&mut [u8]) -> usize, chunk: &mut [u8], line: &mut [u8; LINE]) {
    t.str("\nplugins loaded:\n");
    let mut last = 0u64;
    let mut any = false;
    scan_maps(maps, chunk, line, |l| {
        if let Some(bundle) = plugin_bundle(l.path) {
            let h = fnv(bundle);
            if h != last {
                last = h;
                any = true;
                t.str("  ").bytes(bundle).str("\n");
            }
        }
    });
    if !any {
        t.str("  none\n");
    }
}

fn write_commands<O: Out>(t: &mut Text<O>, ring: &Ring) {
    t.str("\nlast engine commands (oldest first, seconds since start):\n");
    let mut any = false;
    ring.each(|name, count, ms| {
        any = true;
        t.str("  ").secs(ms as u64).str("  ").bytes(name);
        if count > 1 {
            t.str(" x").dec(count as u64);
        }
        t.str("\n");
    });
    if !any {
        t.str("  none\n");
    }
}

/// Frames as `module+offset` (the module by file name, the offset from its
/// load address, as `addr2line -e <module>` takes it).
fn write_frames<O: Out>(t: &mut Text<O>, frames: &[*mut c_void], maps: impl FnMut(&mut [u8]) -> usize, s: &mut ScratchMaps) {
    for i in 0..frames.len() {
        s.module_len[i] = 0;
    }
    let (mut prev, mut base) = (0u64, 0u64);
    scan_maps(maps, &mut *s.chunk, &mut *s.line, |l| {
        let h = fnv(l.path);
        base = if l.offset == 0 || h != prev { l.start.wrapping_sub(l.offset) } else { base };
        prev = h;
        if l.path.is_empty() {
            return;
        }
        let name = file_name(l.path);
        for (i, &pc) in frames.iter().enumerate() {
            let pc = pc as u64;
            if pc >= l.start && pc < l.end {
                let n = name.len().min(MODULE);
                s.module[i][..n].copy_from_slice(&name[..n]);
                s.module_len[i] = n;
                s.offset[i] = pc - base;
            }
        }
    });
    t.str("\nframes (module+offset):\n");
    for (i, &pc) in frames.iter().enumerate() {
        t.str("  #").dec(i as u64).str("  ");
        if s.module_len[i] > 0 {
            t.bytes(&s.module[i][..s.module_len[i]]).str("+").hex(s.offset[i]);
        } else {
            t.str("?").str(" ").hex(pc as u64);
        }
        t.str("\n");
    }
}

/// The parts of `Scratch` a maps scan for frames uses.
struct ScratchMaps<'a> {
    chunk: &'a mut [u8],
    line: &'a mut [u8; LINE],
    module: &'a mut [[u8; MODULE]; FRAMES],
    module_len: &'a mut [usize; FRAMES],
    offset: &'a mut [u64; FRAMES],
}

// -- the report --------------------------------------------------------------

fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

fn local_utc_offset() -> i64 {
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            0
        } else {
            tm.tm_gmtoff
        }
    }
}

/// (year, month, day, hour, minute, second) of a Unix time.
fn civil(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400) as u32;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d, tod / 3600, tod / 60 % 60, tod % 60)
}

/// `YYYYMMDD-HHMMSS` (compact) or `YYYY-MM-DD HH:MM:SS`.
fn write_time<O: Out>(t: &mut Text<O>, local: i64, compact: bool) {
    let (y, mo, d, h, mi, s) = civil(local);
    let (date_sep, mid, time_sep): (&str, &str, &str) = if compact { ("", "-", "") } else { ("-", " ", ":") };
    t.dec_pad(y.max(0) as u64, 4).str(date_sep).dec_pad(mo as u64, 2).str(date_sep).dec_pad(d as u64, 2).str(mid);
    t.dec_pad(h as u64, 2).str(time_sep).dec_pad(mi as u64, 2).str(time_sep).dec_pad(s as u64, 2);
}

fn signal_name(sig: c_int) -> &'static str {
    match sig {
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGBUS => "SIGBUS",
        libc::SIGILL => "SIGILL",
        libc::SIGFPE => "SIGFPE",
        libc::SIGABRT => "SIGABRT",
        libc::SIGTRAP => "SIGTRAP",
        _ => "signal",
    }
}

/// Open `<dir>/crash-<local time>-<pid>.txt`; stderr when that fails.
fn open_report(setup: &Setup, path: &mut [u8; DIR_MAX + 64], now: i64) -> c_int {
    if setup.dir_len == 0 {
        return 2;
    }
    struct PathOut<'a>(&'a mut [u8], usize);
    impl Out for PathOut<'_> {
        fn put(&mut self, b: &[u8]) {
            let n = b.len().min(self.0.len() - 1 - self.1);
            self.0[self.1..self.1 + n].copy_from_slice(&b[..n]);
            self.1 += n;
        }
    }
    let mut buf = [0u8; 64];
    let mut t = Text::new(PathOut(&mut path[..], 0), &mut buf);
    t.bytes(&setup.dir[..setup.dir_len]).str("/crash-");
    write_time(&mut t, now + setup.utc_offset, true);
    t.str("-").dec(unsafe { libc::getpid() } as u64).str(".txt");
    t.flush();
    let end = t.out.1;
    path[end] = 0;
    let fd = unsafe { libc::open(path.as_ptr().cast(), libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_CLOEXEC, 0o600) };
    if fd < 0 {
        2
    } else {
        fd
    }
}

/// Version, time, thread and uptime: the head of every report.
fn write_head<O: Out>(t: &mut Text<O>, setup: &Setup, now: i64) {
    t.str("kabelsalat crash report\n");
    t.str("Written on this computer and sent nowhere. It holds no folder or file names of yours.\n\n");
    t.str("version: ").str(crate::VERSION).str(if cfg!(debug_assertions) { " debug" } else { "" }).str("\n");
    t.str("system: ").str(std::env::consts::OS).str(" ").str(std::env::consts::ARCH).str("\n");
    t.str("time: ");
    write_time(t, now + setup.utc_offset, false);
    t.str("\n");
}

fn write_thread_and_uptime<O: Out>(t: &mut Text<O>, setup: &Setup) {
    let mut name = [0u8; 17];
    unsafe { libc::prctl(libc::PR_GET_NAME, name.as_mut_ptr()) };
    let len = name.iter().position(|&b| b == 0).unwrap_or(16);
    let main = unsafe { libc::syscall(libc::SYS_gettid) as i32 == libc::getpid() };
    t.str("thread: ").bytes(&name[..len]).str(if main { " (main)" } else { "" }).str("\n");
    t.str("uptime: ").secs(monotonic_ns().saturating_sub(setup.start_ns) / 1_000_000).str(" s\n");
}

/// The sections every report ends with: plugins, commands, frames.
fn write_tail<O: Out>(t: &mut Text<O>, s: &mut Scratch) {
    if let Some(maps) = open_maps_reader() {
        write_plugins(t, maps, &mut s.chunk, &mut s.line);
    }
    write_commands(t, &COMMANDS);
    t.flush();
    #[cfg(target_env = "gnu")]
    let n = unsafe { libc::backtrace(s.frames.as_mut_ptr(), FRAMES as c_int) }.max(0) as usize;
    #[cfg(not(target_env = "gnu"))]
    let n = 0;
    if let Some(maps) = open_maps_reader() {
        let frames = s.frames;
        let mut m = ScratchMaps { chunk: &mut s.chunk, line: &mut s.line, module: &mut s.module, module_len: &mut s.module_len, offset: &mut s.offset };
        write_frames(t, &frames[..n], maps, &mut m);
    }
    t.flush();
}

extern "C" fn on_fatal(sig: c_int, info: *mut libc::siginfo_t, _ctx: *mut c_void) {
    // SA_RESETHAND made the action the default again: once this returns, the
    // raised (or re-executed) signal ends the process as it would have.
    let reraise = || unsafe {
        libc::raise(sig);
    };
    let Some(setup) = SETUP.get() else { return reraise() };
    if sig == libc::SIGABRT && PANIC_REPORTED.load(Ordering::Acquire) {
        return reraise();
    }
    // Another thread may be writing a report: a crash ends the process when
    // it is done, a panic hook lets go of the buffers.
    let mut ours = !WRITING.swap(true, Ordering::Acquire);
    for _ in 0..200 {
        if ours {
            break;
        }
        let ts = libc::timespec { tv_sec: 0, tv_nsec: 10_000_000 };
        unsafe { libc::nanosleep(&ts, std::ptr::null_mut()) };
        ours = !WRITING.swap(true, Ordering::Acquire);
    }
    if !ours {
        return reraise();
    }
    // An unwinder stuck on a broken stack must not keep the show down.
    unsafe { libc::alarm(5) };
    // SAFETY: WRITING is ours.
    let s = unsafe { &mut *SCRATCH.0.get() };
    let now = unsafe { libc::time(std::ptr::null_mut()) } as i64;
    let fd = open_report(setup, &mut s.path, now);
    let mut buf = [0u8; 256];
    let mut t = Text::new(Fd(fd), &mut buf);
    write_head(&mut t, setup, now);
    t.str("cause: ").str(signal_name(sig)).str(" (signal ").dec(sig as u64).str(")");
    if !info.is_null() {
        let code = unsafe { (*info).si_code };
        if code <= 0 {
            t.str(", sent by process ").dec(unsafe { (*info).si_pid() } as u64);
        } else if matches!(sig, libc::SIGSEGV | libc::SIGBUS | libc::SIGILL | libc::SIGFPE) {
            t.str(", fault address ").hex(unsafe { (*info).si_addr() } as u64);
        }
    }
    t.str("\n");
    write_thread_and_uptime(&mut t, setup);
    write_tail(&mut t, s);
    if fd != 2 {
        unsafe {
            libc::fsync(fd);
            libc::close(fd);
        }
        let mut msg = [0u8; 128];
        let mut e = Text::new(Fd(2), &mut msg);
        let len = s.path.iter().position(|&b| b == 0).unwrap_or(0);
        e.str("kabelsalat crashed (").str(signal_name(sig)).str("), report: ").bytes(&s.path[..len]).str("\n");
        e.flush();
    }
    reraise();
}

fn on_panic(info: &std::panic::PanicHookInfo) {
    let Some(setup) = SETUP.get() else { return };
    if WRITING.swap(true, Ordering::Acquire) {
        return;
    }
    let message = info
        .payload()
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| info.payload().downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "(no message)".into());
    let at = info.location().map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column())).unwrap_or_default();
    let backtrace = short_backtrace(&std::backtrace::Backtrace::force_capture().to_string());
    let home = std::env::var("HOME").ok();
    let home = home.as_deref();
    // SAFETY: WRITING is ours.
    let s = unsafe { &mut *SCRATCH.0.get() };
    let now = unsafe { libc::time(std::ptr::null_mut()) } as i64;
    let fd = open_report(setup, &mut s.path, now);
    let mut buf = [0u8; 256];
    let mut t = Text::new(Fd(fd), &mut buf);
    write_head(&mut t, setup, now);
    t.str("cause: panic: ").str(&scrub(&message, home)).str("\n");
    t.str("at: ").str(&scrub(&at, home)).str("\n");
    write_thread_and_uptime(&mut t, setup);
    t.str("\nbacktrace:\n").str(&scrub(&backtrace, home));
    write_tail(&mut t, s);
    if fd != 2 {
        unsafe { libc::close(fd) };
        let len = s.path.iter().position(|&b| b == 0).unwrap_or(0);
        eprintln!("kabelsalat: panic report: {}", String::from_utf8_lossy(&s.path[..len]));
    }
    PANIC_REPORTED.store(true, Ordering::Release);
    WRITING.store(false, Ordering::Release);
}

/// The frames between std's short-backtrace markers, as `RUST_BACKTRACE=1`
/// prints them: no capture or hook frames above, no runtime start below.
fn short_backtrace(full: &str) -> String {
    let mut frames: Vec<String> = Vec::new();
    for line in full.lines() {
        let head = line.trim_start();
        let starts = head.split_once(": ").is_some_and(|(n, _)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        match frames.last_mut() {
            Some(f) if !starts => {
                f.push_str(line);
                f.push('\n');
            }
            _ => frames.push(format!("{line}\n")),
        }
    }
    let end = frames.iter().rposition(|f| f.contains("__rust_end_short_backtrace")).map_or(0, |i| i + 1);
    let begin = frames.iter().position(|f| f.contains("__rust_begin_short_backtrace")).filter(|&b| b > end).unwrap_or(frames.len());
    frames[end..begin].concat()
}

// -- scrubbing ---------------------------------------------------------------

/// Text with every path cut to what is the app's or a library's own:
/// a source file keeps `src/…` (behind its crate and version when it comes
/// from crates.io or the standard library), any other path becomes
/// `<path>`. What is left of `home` after that becomes `~`.
pub fn scrub(text: &str, home: Option<&str>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(['/', '~']) {
        let starts_path = rest[..i].chars().next_back().is_none_or(|c| c.is_whitespace() || "'\"([<=,:".contains(c));
        let is_path = starts_path && (rest[i..].starts_with('/') || rest[i..].starts_with("~/"));
        if !is_path {
            out.push_str(&rest[..i + 1]);
            rest = &rest[i + 1..];
            continue;
        }
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        // A quoted path (Debug of a path or string) may hold spaces.
        let end = match rest[..i].chars().next_back().filter(|&c| c == '"' || c == '\'') {
            Some(q) => tail.find([q, '\n']),
            None => tail.find(|c: char| c.is_whitespace() || "'\"()<>,;".contains(c)),
        }
        .unwrap_or(tail.len());
        out.push_str(&source_path(&tail[..end]).unwrap_or_else(|| "<path>".into()));
        rest = &tail[end..];
    }
    out.push_str(rest);
    match home.filter(|h| h.len() > 1) {
        Some(h) => out.replace(h, "~"),
        None => out,
    }
}

/// `…/<crate>/src/<file>` → `src/<file>`, keeping `<crate>` when it is a
/// published crate (`name-1.2.3`) or part of the standard library.
fn source_path(path: &str) -> Option<String> {
    let i = path.rfind("/src/")?;
    let file = &path[i + 1..];
    let krate = path[..i].rsplit('/').next().unwrap_or("");
    let semver = |v: &str| v.split('.').count() >= 3 && v.split('.').take(2).all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    let published = krate.rsplit_once('-').is_some_and(|(n, v)| !n.is_empty() && semver(v));
    let std = ["std", "core", "alloc", "proc_macro", "test", "panic_unwind", "panic_abort", "backtrace"].contains(&krate);
    Some(if published || std { format!("{krate}/{file}") } else { file.to_string() })
}

/// The commands section of a report, for `ring`.
#[cfg(test)]
fn commands_text(ring: &Ring) -> String {
    let mut v = Vec::new();
    let mut buf = [0u8; 64];
    let mut t = Text::new(&mut v, &mut buf);
    write_commands(&mut t, ring);
    t.flush();
    String::from_utf8(v).unwrap()
}

#[cfg(test)]
impl Out for &mut Vec<u8> {
    fn put(&mut self, bytes: &[u8]) {
        self.extend_from_slice(bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    #[allow(dead_code)]
    enum Cmd {
        NoteOn { note: u8 },
        LoadRigs { names: Vec<String> },
        Stop,
        Tuple(u32),
    }

    fn noted(ring: &Ring, cmds: &[Cmd]) {
        for (i, c) in cmds.iter().enumerate() {
            let mut n = [0u8; NAME];
            let len = variant_name(c, &mut n);
            ring.note(&n[..len], 1000 + i as u32);
        }
    }

    #[test]
    fn a_report_names_commands_but_never_their_values() {
        let ring = Ring::new();
        noted(&ring, &[
            Cmd::LoadRigs { names: vec!["Secret Gig".into()] },
            Cmd::NoteOn { note: 60 },
            Cmd::NoteOn { note: 64 },
            Cmd::NoteOn { note: 67 },
            Cmd::Tuple(7),
            Cmd::Stop,
        ]);
        let text = commands_text(&ring);
        assert!(!text.contains("Secret") && !text.contains("60"), "{text}");
        let lines: Vec<&str> = text.lines().skip(2).map(str::trim).collect();
        assert_eq!(lines, ["1.000  LoadRigs", "1.003  NoteOn x3", "1.004  Tuple", "1.005  Stop"], "{text}");
    }

    #[test]
    fn the_ring_keeps_the_last_commands_oldest_first() {
        let ring = Ring::new();
        for i in 0..RING + 5 {
            ring.note(if i % 2 == 0 { b"NoteOn" } else { b"NoteOff" }, i as u32 * 1000);
        }
        let text = commands_text(&ring);
        let lines: Vec<&str> = text.lines().skip(2).collect();
        assert_eq!(lines.len(), RING);
        assert!(lines[0].trim().starts_with("5.000  NoteOff"), "{text}");
        assert!(lines[RING - 1].trim().starts_with("36.000  NoteOn"), "{text}");
    }

    #[test]
    fn panic_text_keeps_source_files_and_drops_every_other_path() {
        let home = "/home/ada";
        let text = "\
called `Result::unwrap()` on an `Err` value: \"/home/ada/kabelsalat-sessions/Secret Gig.ksalat: bad\"
   0: std::panicking::begin_panic
             at /rustc/90b35a6239c3d8bdabc530a6a0816f7ff89a0aaf/library/std/src/panicking.rs:12:5
   1: egui::context::Context::run
             at /home/ada/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/egui-0.37.0/src/context.rs:800:9
   2: kabelsalat::ui::DemoApp::frame_ui
             at /home/ada/code/my-band/kabelsalat/src/ui.rs:31760:5
   3: plugin at ~/.clap/Surge.clap, opened (/opt/ada-tools/lib.so)
home is /home/ada, a/b stays";
        let out = scrub(text, Some(home));
        assert!(out.contains("std/src/panicking.rs:12:5"), "{out}");
        assert!(out.contains("egui-0.37.0/src/context.rs:800:9"), "{out}");
        assert!(out.contains("at src/ui.rs:31760:5"), "{out}");
        assert!(out.contains("plugin at <path>, opened (<path>)"), "{out}");
        assert!(out.contains("home is <path>, a/b stays"), "{out}");
        for gone in ["ada", "Secret", "Gig", "my-band", "rustc", "registry", "index.crates"] {
            assert!(!out.contains(gone), "{gone} left in: {out}");
        }
    }

    /// Lines of this machine's real /proc/self/maps (rule 63), a home plugin
    /// and an LV2 bundle added in the same shape.
    const MAPS: &str = "\
b675165b0000-b67516e7c000 r-xp 00000000 08:01 130159                     /usr/lib/cargo/bin/coreutils/cat
b67516e8f000-b67516fe0000 r--p 008cf000 08:01 130159                     /usr/lib/cargo/bin/coreutils/cat
b6753a89b000-b6753aa00000 rw-p 00000000 00:00 0                          [heap]
f2cccb330000-f2cccb4d9000 r-xp 00000000 08:01 130724                     /usr/lib/aarch64-linux-gnu/libc.so.6
f2cccb4d9000-f2cccb4ed000 ---p 001a9000 08:01 130724                     /usr/lib/aarch64-linux-gnu/libc.so.6
f2cccb4f2000-f2cccb4fe000 rw-p 00000000 00:00 0
f2cccb500000-f2cccb5af000 r--p 00000000 08:01 130752                     /home/ada/.clap/Surge XT.clap
f2cccb5af000-f2cccb5bf000 r-xp 000af000 08:01 130752                     /home/ada/.clap/Surge XT.clap
f2cccb600000-f2cccb601000 r-xp 00000000 08:01 133179                     /home/ada/.lv2/b_synth.lv2/b_synth.so (deleted)";

    fn reader(text: &str) -> impl FnMut(&mut [u8]) -> usize + '_ {
        let mut pos = 0;
        move |buf: &mut [u8]| {
            // Small reads, so lines span chunks.
            let n = buf.len().min(37).min(text.len() - pos);
            buf[..n].copy_from_slice(&text.as_bytes()[pos..pos + n]);
            pos += n;
            n
        }
    }

    #[test]
    fn plugins_and_frames_come_from_the_maps_by_file_name_only() {
        let mut v = Vec::new();
        let mut buf = [0u8; 64];
        let mut t = Text::new(&mut v, &mut buf);
        let (mut chunk, mut line) = ([0u8; 1024], [0u8; LINE]);
        write_plugins(&mut t, reader(MAPS), &mut chunk, &mut line);
        let frames = [0xb675165b1234usize as *mut c_void, 0xf2cccb4d9010usize as *mut _, 0xf2cccb5af100usize as *mut _, 0x10usize as *mut _];
        let (mut module, mut module_len, mut offset) = ([[0u8; MODULE]; FRAMES], [0usize; FRAMES], [0u64; FRAMES]);
        let mut m = ScratchMaps { chunk: &mut chunk, line: &mut line, module: &mut module, module_len: &mut module_len, offset: &mut offset };
        write_frames(&mut t, &frames, reader(MAPS), &mut m);
        t.flush();
        let text = String::from_utf8(v).unwrap();
        assert!(text.contains("plugins loaded:\n  Surge XT.clap\n  b_synth.lv2\n"), "{text}");
        assert!(text.contains("#0  cat+0x1234\n"), "{text}");
        assert!(text.contains("#1  libc.so.6+0x1a9010\n"), "{text}");
        assert!(text.contains("#2  Surge XT.clap+0xaf100\n"), "{text}");
        assert!(text.contains("#3  ? 0x10\n"), "{text}");
        assert!(!text.contains("ada") && !text.contains("/usr"), "{text}");
    }

    #[test]
    fn a_maps_line_longer_than_the_buffer_keeps_its_file_name() {
        let long = format!("f2cccb600000-f2cccb601000 r-xp 00000000 08:01 1 /{}/libdeep.so\n", "d".repeat(2000));
        let mut names = Vec::new();
        let (mut chunk, mut line) = ([0u8; 1024], [0u8; LINE]);
        scan_maps(reader(&long), &mut chunk, &mut line, |l| names.push((l.start, String::from_utf8_lossy(file_name(l.path)).into_owned())));
        assert_eq!(names, [(0xf2cccb600000, "libdeep.so".to_string())]);
    }

    #[test]
    fn a_panic_backtrace_starts_at_the_panic_and_ends_at_main() {
        let full = "   0: std::backtrace::Backtrace::create
             at std/src/backtrace.rs:331:13
   1: kabelsalat::crash::on_panic
   2: std::sys::backtrace::__rust_end_short_backtrace
             at std/src/sys/backtrace.rs:182:18
   3: core::panicking::panic_fmt
   4: kabelsalat::ui::headless::run
             at src/ui/headless.rs:144:33
   5: std::sys::backtrace::__rust_begin_short_backtrace
   6: main
";
        let short = short_backtrace(full);
        assert_eq!(short, "   3: core::panicking::panic_fmt\n   4: kabelsalat::ui::headless::run\n             at src/ui/headless.rs:144:33\n");
        assert_eq!(short_backtrace("   0: a\n   1: b\n"), "   0: a\n   1: b\n", "without markers every frame stays");
    }

    #[test]
    fn old_reports_are_pruned_and_a_pid_finds_its_own() {
        let dir = std::env::temp_dir().join(format!("kabelsalat-test-prune-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (i, pid) in [(1, 300), (2, 31), (3, 300), (4, 7)] {
            std::fs::write(dir.join(format!("crash-2026112{i}-120000-{pid}.txt")), "").unwrap();
        }
        std::fs::write(dir.join("notes.txt"), "").unwrap();
        assert_eq!(report_of(&dir, 300).unwrap().file_name().unwrap(), "crash-20261123-120000-300.txt");
        assert_eq!(report_of(&dir, 30), None, "31 is not 30");
        prune(&dir, 2);
        let mut left: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["crash-20261123-120000-300.txt", "crash-20261124-120000-7.txt", "notes.txt"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn report_times_are_calendar_times() {
        let mut v = Vec::new();
        let mut buf = [0u8; 64];
        let mut t = Text::new(&mut v, &mut buf);
        write_time(&mut t, 1_795_305_600 + 3600 + 62, false); // 2026-11-22 01:01:02
        t.str(" ");
        write_time(&mut t, 951_782_400, true); // 2000-02-29
        t.flush();
        assert_eq!(String::from_utf8(v).unwrap(), "2026-11-22 01:01:02 20000229-000000");
    }
}
