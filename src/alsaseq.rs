//! Minimal ALSA sequencer client over /dev/snd/seq, written directly with
//! libc ioctls — no new crate (the `alsa` crate is already a transitive
//! dependency of cpal/midir, but adding it directly would touch Cargo.toml,
//! which this repo keeps out of feature PRs).
//!
//! Why raw: the MIDI clock sender needs SCHEDULED events — the ALSA
//! sequencer queue holds each event until its (real-time) timestamp, so
//! delivery is kernel-side and sample-accurate, unlike a sleep loop whose
//! jitter is the thread wakeup. midir's API cannot schedule.
//!
//! Every `#[repr(C)]` struct below mirrors /usr/include/sound/asequencer.h
//! on this machine (Linux UAPI, stable since 2.6). The ioctls would return
//! EINVAL/EFAULT on a layout mismatch, which the gated integration tests
//! (`ALSA` enabled only when /dev/snd/seq opens) catch on any dev machine.
//! Tests never touch hardware: they create extra kernel *virtual* clients,
//! subscribe them to our own port and read back what we scheduled.

use std::io;

// ---- ioctl request encoding (asm-generic/ioctl.h) --------------------------

const IOC_NONE: u32 = 0;
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;

const fn ioc(dir: u32, ty: u8, nr: u8, size: usize) -> u32 {
    (dir << 30) | ((size as u32) << 16) | ((ty as u32) << 8) | (nr as u32)
}
const fn iow(ty: u8, nr: u8, size: usize) -> u32 { ioc(IOC_WRITE, ty, nr, size) }
const fn ior(ty: u8, nr: u8, size: usize) -> u32 { ioc(IOC_READ, ty, nr, size) }
const fn iowr(ty: u8, nr: u8, size: usize) -> u32 { ioc(IOC_READ | IOC_WRITE, ty, nr, size) }

// ---- UAPI constants (sound/asequencer.h) ----------------------------------

pub const CLIENT_SYSTEM: u8 = 0;
pub const PORT_SYSTEM_TIMER: u8 = 0;

pub const PORT_FLG_TIMESTAMP: u32 = 1 << 1;
pub const PORT_FLG_TIME_REAL: u32 = 1 << 2;

pub const PORT_CAP_READ: u32 = 1 << 0;
pub const PORT_CAP_WRITE: u32 = 1 << 1;
pub const PORT_CAP_DUPLEX: u32 = 1 << 4;
pub const PORT_CAP_SUBS_READ: u32 = 1 << 5;
pub const PORT_CAP_SUBS_WRITE: u32 = 1 << 6;

pub const PORT_TYPE_MIDI_GENERIC: u32 = 1 << 1;
pub const PORT_TYPE_APPLICATION: u32 = 1 << 20;

/// `SNDRV_SEQ_ADDRESS_SUBSCRIBERS`: deliver to every port subscribed to the
/// source port of the event.
pub const ADDRESS_SUBSCRIBERS: u8 = 254;
/// `SNDRV_SEQ_QUEUE_DIRECT`: dispatch immediately, no queue scheduling.
pub const QUEUE_DIRECT: u8 = 253;

pub const EVENT_NOTEON: u8 = 6;
pub const EVENT_NOTEOFF: u8 = 7;
pub const EVENT_CONTROLLER: u8 = 10;
pub const EVENT_PGMCHANGE: u8 = 11;
pub const EVENT_SONGPOS: u8 = 20;
pub const EVENT_START: u8 = 30;
pub const EVENT_CONTINUE: u8 = 31;
pub const EVENT_STOP: u8 = 32;
pub const EVENT_CLOCK: u8 = 36;

const TIME_STAMP_REAL: u8 = 1 << 0;
const TIME_MODE_ABS: u8 = 0 << 1;
const TIME_MODE_REL: u8 = 1 << 1;

// ---- UAPI structs (checked field-by-field against the header) -------------

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Addr {
    pub client: u8,
    pub port: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct RealTime {
    pub sec: u32,
    pub nsec: u32,
}

impl RealTime {
    pub fn from_ns(ns: u64) -> Self {
        RealTime { sec: (ns / 1_000_000_000) as u32, nsec: (ns % 1_000_000_000) as u32 }
    }
    pub fn to_ns(self) -> u64 {
        self.sec as u64 * 1_000_000_000 + self.nsec as u64
    }
}

/// 28 bytes, exactly as the kernel expects (writes/reads are whole events).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Event {
    pub ty: u8,
    pub flags: u8,
    pub tag: i8,
    pub queue: u8,
    /// Union: tick (u32) or real time. We always use real time.
    pub time: RealTime,
    pub source: Addr,
    pub dest: Addr,
    /// Union payload; `control.value` carries the SPP for EVENT_SONGPOS.
    pub data: [u32; 3],
}

impl Event {
    pub fn empty() -> Self {
        Event {
            ty: 0,
            flags: 0,
            tag: 0,
            queue: 0,
            time: RealTime::default(),
            source: Addr { client: 0, port: 0 },
            dest: Addr { client: 0, port: 0 },
            data: [0; 3],
        }
    }
    /// Song Position Pointer in MIDI beats (16th notes), 0..=16383.
    pub fn songpos_value(&self) -> u32 { self.data[2] }
}

/// `snd_seq_client_info` (name[64] + filters; see header).
#[repr(C)]
struct ClientInfo {
    client: i32,
    client_type: i32,
    name: [u8; 64],
    filter: u32,
    multicast_filter: [u8; 8],
    event_filter: [u8; 32],
    num_ports: i32,
    event_lost: i32,
    card: i32,
    pid: i32,
    midi_version: u32,
    group_filter: u32,
    reserved: [u8; 48],
}

/// `snd_seq_client_pool`.
#[repr(C)]
struct ClientPool {
    client: i32,
    output_pool: i32,
    input_pool: i32,
    output_room: i32,
    output_free: i32,
    input_free: i32,
    reserved: [u8; 64],
}

/// `snd_seq_port_info` (kernel pointer field is 8 bytes here: keep it as a
/// usize and always leave it zero).
#[repr(C)]
struct PortInfo {
    addr: Addr,
    name: [u8; 64],
    capability: u32,
    type_: u32,
    midi_channels: i32,
    midi_voices: i32,
    synth_voices: i32,
    read_use: i32,
    write_use: i32,
    kernel: usize,
    flags: u32,
    time_queue: u8,
    direction: u8,
    ump_group: u8,
    reserved: [u8; 57],
}

/// `snd_seq_port_subscribe`.
#[repr(C)]
struct PortSubscribe {
    sender: Addr,
    dest: Addr,
    voices: u32,
    flags: u32,
    queue: u8,
    pad: [u8; 3],
    reserved: [u8; 64],
}

/// `snd_seq_queue_info`.
#[repr(C)]
struct QueueInfo {
    queue: i32,
    owner: i32,
    locked: i32,
    name: [u8; 64],
    flags: u32,
    reserved: [u8; 60],
}

/// `snd_seq_queue_tempo`.
#[repr(C)]
struct QueueTempo {
    queue: i32,
    tempo: u32,
    ppq: i32,
    skew_value: u32,
    skew_base: u32,
    tempo_base: u16,
    reserved: [u8; 22],
}

/// `snd_seq_queue_status`.
#[repr(C)]
struct QueueStatus {
    queue: i32,
    events: i32,
    tick: u32,
    time: RealTime,
    running: i32,
    flags: i32,
    reserved: [u8; 64],
}

/// `snd_seq_remove_events` — flush pending (scheduled, undelivered) events.
#[repr(C)]
struct RemoveEvents {
    remove_mode: u32,
    time: RealTime,
    queue: u8,
    dest: Addr,
    channel: u8,
    ty: i32,
    tag: i8,
    reserved: [i32; 10],
}

const REMOVE_OUTPUT: u32 = 1 << 1;

fn cstr(buf: &mut [u8], s: &str) {
    let b = s.as_bytes();
    for (i, slot) in buf.iter_mut().enumerate() {
        *slot = if i < b.len() { b[i] } else { 0 };
    }
}

fn cstr_to_string(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).trim().to_string()
}

fn ioctl(fd: i32, req: u32, arg: *mut u8) -> io::Result<i32> {
    // SAFETY: raw ioctl on our own fd with a valid pointer to a #[repr(C)]
    // UAPI struct; the kernel only reads/writes within that struct.
    let r = unsafe { libc::ioctl(fd, req as _, arg) };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

// ---- public API ------------------------------------------------------------

/// One port of the system, as `list_ports` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortDesc {
    pub client: u8,
    pub port: u8,
    pub client_name: String,
    pub port_name: String,
    pub capability: u32,
    pub type_: u32,
}

impl PortDesc {
    /// midir-style full name ("kabelsalat:MIDI Clock 137:0") — what the rest
    /// of the app composes for hardware names.
    pub fn full_name(&self) -> String {
        format!("{}:{} {}:{}",
            if self.client_name.is_empty() { "Client".into() } else { self.client_name.clone() },
            if self.port_name.is_empty() { format!("port {}", self.port) } else { self.port_name.clone() },
            self.client, self.port)
    }
    /// True when other clients may subscribe to write events into this port
    /// (a destination, e.g. a hardware MIDI out or another app's input).
    pub fn can_receive(&self) -> bool {
        self.capability & PORT_CAP_WRITE != 0 && self.capability & PORT_CAP_SUBS_WRITE != 0
    }
    /// True when other clients may subscribe to read this port's events
    /// (a source, e.g. a hardware MIDI in or another app's output).
    pub fn can_send(&self) -> bool {
        self.capability & PORT_CAP_READ != 0 && self.capability & PORT_CAP_SUBS_READ != 0
    }
}

/// An open ALSA sequencer client (one file descriptor, one kernel client).
pub struct SeqClient {
    fd: i32,
    client: u8,
    /// Events written but not yet picked up by the kernel pool counter.
    written: usize,
    /// `CLOCK_MONOTONIC` ns at which our queue's own clock read 0: a queue's
    /// real-time stamps count from ITS start, not from the system's.
    queue_epoch_ns: i64,
}

fn open_fd() -> io::Result<i32> {
    // SAFETY: plain open(2) on a fixed device path with a constant flags arg.
    let fd = unsafe {
        libc::open(
            b"/dev/snd/seq\0".as_ptr() as *const libc::c_char,
            libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 { Err(io::Error::last_os_error()) } else { Ok(fd) }
}

/// Can this machine run the sequencer at all? (CI containers have no
/// /dev/snd — everything ALSA-backed is gated on this.)
pub fn available() -> bool {
    open_fd().map(|fd| {
        // SAFETY: close(2) on the fd we just opened.
        unsafe { libc::close(fd); }
        true
    }).unwrap_or(false)
}

impl SeqClient {
    pub fn open() -> anyhow::Result<SeqClient> {
        SeqClient::open_named("kabelsalat")
    }

    /// Open with a specific kernel client name (tests use their own name so
    /// port scans can tell them from the app's client).
    pub fn open_named(name: &str) -> anyhow::Result<SeqClient> {
        let fd = open_fd().map_err(|e| anyhow::anyhow!("cannot open /dev/snd/seq: {e}"))?;
        let mut this = SeqClient { fd, client: 0, written: 0, queue_epoch_ns: 0 };
        let mut id: i32 = 0;
        ioctl(fd, ior(b'S', 0x01, 4), &mut id as *mut i32 as *mut u8)
            .map_err(|e| anyhow::anyhow!("CLIENT_ID failed: {e}"))?;
        this.client = id as u8;
        this.set_client_name(name)?;
        this.set_pool(512, 256)?;
        Ok(this)
    }

    pub fn client_id(&self) -> u8 { self.client }

    fn set_client_name(&mut self, name: &str) -> anyhow::Result<()> {
        let mut info: ClientInfo = unsafe { std::mem::zeroed() };
        info.client = self.client as i32;
        // The kernel rejects a set whose type differs from the client's own
        // (EINVAL), so start from what it holds.
        ioctl(self.fd, iowr(b'S', 0x10, std::mem::size_of::<ClientInfo>()),
              &mut info as *mut ClientInfo as *mut u8)
            .map_err(|e| anyhow::anyhow!("GET_CLIENT_INFO failed: {e}"))?;
        cstr(&mut info.name, name);
        ioctl(self.fd, iow(b'S', 0x11, std::mem::size_of::<ClientInfo>()),
              &mut info as *mut ClientInfo as *mut u8)
            .map_err(|e| anyhow::anyhow!("SET_CLIENT_INFO failed: {e}"))?;
        Ok(())
    }

    fn set_pool(&mut self, output: i32, input: i32) -> anyhow::Result<()> {
        let mut pool: ClientPool = unsafe { std::mem::zeroed() };
        pool.client = self.client as i32;
        pool.output_pool = output;
        pool.input_pool = input;
        pool.output_room = 16;
        ioctl(self.fd, iow(b'S', 0x4c, std::mem::size_of::<ClientPool>()),
              &mut pool as *mut ClientPool as *mut u8)
            .map_err(|e| anyhow::anyhow!("SET_CLIENT_POOL failed: {e}"))?;
        Ok(())
    }

    /// Create a port; returns its number. `caps` is a PORT_CAP_* bitmask.
    pub fn create_port(&mut self, name: &str, caps: u32) -> anyhow::Result<u8> {
        let mut info: PortInfo = unsafe { std::mem::zeroed() };
        info.addr = Addr { client: self.client, port: 0 };
        cstr(&mut info.name, name);
        info.capability = caps;
        info.type_ = PORT_TYPE_MIDI_GENERIC | PORT_TYPE_APPLICATION;
        ioctl(self.fd, iowr(b'S', 0x20, std::mem::size_of::<PortInfo>()),
              &mut info as *mut PortInfo as *mut u8)
            .map_err(|e| anyhow::anyhow!("CREATE_PORT failed: {e}"))?;
        Ok(info.addr.port)
    }

    /// Have the kernel stamp every event delivered to our `port` with
    /// `queue`'s real time (what the receiver sees, free of its own
    /// scheduling latency) — the self-test's measuring instrument.
    pub fn stamp_port(&mut self, port: u8, queue: u8) -> anyhow::Result<()> {
        let mut info: PortInfo = unsafe { std::mem::zeroed() };
        info.addr = Addr { client: self.client, port };
        ioctl(self.fd, iowr(b'S', 0x22, std::mem::size_of::<PortInfo>()),
              &mut info as *mut PortInfo as *mut u8)
            .map_err(|e| anyhow::anyhow!("GET_PORT_INFO failed: {e}"))?;
        info.flags |= PORT_FLG_TIMESTAMP | PORT_FLG_TIME_REAL;
        info.time_queue = queue;
        ioctl(self.fd, iow(b'S', 0x23, std::mem::size_of::<PortInfo>()),
              &mut info as *mut PortInfo as *mut u8)
            .map_err(|e| anyhow::anyhow!("SET_PORT_INFO failed: {e}"))?;
        Ok(())
    }

    /// Create a timing queue and start it running. Events with an absolute
    /// real timestamp on this queue are delivered by the kernel exactly at
    /// that time (monotonic clock, same base as `Instant`).
    pub fn create_running_queue(&mut self, name: &str) -> anyhow::Result<u8> {
        let mut info: QueueInfo = unsafe { std::mem::zeroed() };
        cstr(&mut info.name, name);
        ioctl(self.fd, iowr(b'S', 0x32, std::mem::size_of::<QueueInfo>()),
              &mut info as *mut QueueInfo as *mut u8)
            .map_err(|e| anyhow::anyhow!("CREATE_QUEUE failed: {e}"))?;
        let queue = info.queue as u8;
        // A tempo is required before a queue can run; irrelevant for real
        // timestamps but set one sane MIDI-ish value (120 BPM at 24 ppqn).
        let mut tempo: QueueTempo = unsafe { std::mem::zeroed() };
        tempo.queue = queue as i32;
        tempo.tempo = 500_000; // µs per quarter note
        tempo.ppq = 24;
        ioctl(self.fd, iow(b'S', 0x42, std::mem::size_of::<QueueTempo>()),
              &mut tempo as *mut QueueTempo as *mut u8)
            .map_err(|e| anyhow::anyhow!("SET_QUEUE_TEMPO failed: {e}"))?;
        self.queue_control(queue, EVENT_START)?;
        // Calibrate the queue clock against CLOCK_MONOTONIC once it runs:
        // queue_time = monotonic - epoch (the midpoint of the status read).
        for _ in 0..200 {
            let t0 = mono_ns();
            let (running, qt) = self.queue_status(queue)?;
            let t1 = mono_ns();
            if running {
                self.queue_epoch_ns = ((t0 + t1) / 2) as i64 - qt as i64;
                return Ok(queue);
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        anyhow::bail!("queue {queue} did not start running")
    }

    /// Queue control (start/continue/stop), the way alsa-lib sends it: an
    /// event of the "timer message" class addressed to the system timer
    /// port, naming the affected queue in the payload. The kernel processes
    /// queue control events on arrival, not on their timestamp.
    fn queue_control(&mut self, queue: u8, ty: u8) -> anyhow::Result<()> {
        let mut ev = Event::empty();
        ev.ty = ty;
        ev.flags = TIME_STAMP_REAL | TIME_MODE_REL;
        ev.queue = queue;
        ev.time = RealTime::default();
        ev.source = Addr { client: self.client, port: 0 };
        ev.dest = Addr { client: CLIENT_SYSTEM, port: PORT_SYSTEM_TIMER };
        ev.data[0] = queue as u32; // snd_seq_ev_queue_control.queue
        self.write_events(&[ev])
    }

    /// Is the queue running, and its current monotonic time?
    pub fn queue_status(&mut self, queue: u8) -> anyhow::Result<(bool, u64)> {
        let mut st: QueueStatus = unsafe { std::mem::zeroed() };
        st.queue = queue as i32;
        ioctl(self.fd, iowr(b'S', 0x40, std::mem::size_of::<QueueStatus>()),
              &mut st as *mut QueueStatus as *mut u8)
            .map_err(|e| anyhow::anyhow!("GET_QUEUE_STATUS failed: {e}"))?;
        Ok((st.running != 0, st.time.to_ns()))
    }

    /// All ports of all clients, cursor-style as the ioctl protocol expects
    /// (query with number N returns the nearest entry with number ≥ N).
    pub fn list_ports(&mut self) -> Vec<PortDesc> {
        let mut out = Vec::new();
        // QUERY_NEXT_CLIENT / QUERY_NEXT_PORT return the entry AFTER the one
        // named (client -1 / port 255 = "before the first": the kernel adds
        // one to the unsigned port and wraps to 0).
        let mut client = -1i32;
        loop {
            let mut ci: ClientInfo = unsafe { std::mem::zeroed() };
            ci.client = client;
            if ioctl(self.fd, iowr(b'S', 0x51, std::mem::size_of::<ClientInfo>()),
                     &mut ci as *mut ClientInfo as *mut u8).is_err() {
                break;
            }
            if ci.client <= client { break; } // safety: no forward progress
            client = ci.client;
            let client_name = cstr_to_string(&ci.name);
            let mut last_port: Option<u8> = None;
            loop {
                let mut pi: PortInfo = unsafe { std::mem::zeroed() };
                pi.addr = Addr { client: ci.client as u8, port: last_port.unwrap_or(255) };
                if ioctl(self.fd, iowr(b'S', 0x52, std::mem::size_of::<PortInfo>()),
                         &mut pi as *mut PortInfo as *mut u8).is_err() {
                    break;
                }
                if pi.addr.client as i32 != ci.client || last_port.is_some_and(|l| pi.addr.port <= l) {
                    break; // no forward progress
                }
                last_port = Some(pi.addr.port);
                out.push(PortDesc {
                    client: pi.addr.client,
                    port: pi.addr.port,
                    client_name: client_name.clone(),
                    port_name: cstr_to_string(&pi.name),
                    capability: pi.capability,
                    type_: pi.type_,
                });
                if out.len() > 4096 { break; } // paranoia
            }
            if client > 250 { break; } // client ids are u8
        }
        out
    }

    /// Subscribe `sender` → `dest`: every event the sender port emits is
    /// delivered to the dest port (both directions see it).
    pub fn subscribe(&mut self, sender: Addr, dest: Addr) -> anyhow::Result<()> {
        let mut sub: PortSubscribe = unsafe { std::mem::zeroed() };
        sub.sender = sender;
        sub.dest = dest;
        ioctl(self.fd, iow(b'S', 0x30, std::mem::size_of::<PortSubscribe>()),
              &mut sub as *mut PortSubscribe as *mut u8)
            .or_else(|e| if e.raw_os_error() == Some(libc::EBUSY) { Ok(0) } else { Err(e) })
            .map_err(|e| anyhow::anyhow!("SUBSCRIBE_PORT {e}"))?;
        Ok(())
    }

    pub fn unsubscribe(&mut self, sender: Addr, dest: Addr) -> anyhow::Result<()> {
        let mut sub: PortSubscribe = unsafe { std::mem::zeroed() };
        sub.sender = sender;
        sub.dest = dest;
        ioctl(self.fd, iow(b'S', 0x31, std::mem::size_of::<PortSubscribe>()),
              &mut sub as *mut PortSubscribe as *mut u8)
            .map_err(|e| anyhow::anyhow!("UNSUBSCRIBE_PORT {e}"))?;
        Ok(())
    }

    /// Write events into the kernel pool. `EAGAIN` means the pool is full:
    /// the caller drops (a full clock tick, not a catastrophe) — we surface
    /// it as an error the caller may ignore.
    pub fn write_events(&mut self, events: &[Event]) -> anyhow::Result<()> {
        if events.is_empty() { return Ok(()); }
        let bytes = events.len() * std::mem::size_of::<Event>();
        // SAFETY: write(2) of a &-borrowed immutable struct slice.
        let n = unsafe {
            libc::write(self.fd, events.as_ptr() as *const libc::c_void, bytes)
        };
        if n < 0 {
            return Err(anyhow::anyhow!("seq write: {}", io::Error::last_os_error()));
        }
        self.written += (n as usize) / std::mem::size_of::<Event>();
        if (n as usize) < bytes {
            return Err(anyhow::anyhow!("seq write truncated: {n} of {bytes} bytes"));
        }
        Ok(())
    }

    /// Read up to `buf.len()` pending input events (non-blocking).
    /// Returns how many were read.
    pub fn read_events(&mut self, buf: &mut [Event]) -> usize {
        let bytes = buf.len() * std::mem::size_of::<Event>();
        if bytes == 0 { return 0; }
        // SAFETY: read(2) into our own struct slice.
        let n = unsafe {
            libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, bytes)
        };
        if n <= 0 { return 0; }
        (n as usize) / std::mem::size_of::<Event>()
    }

    /// Wait up to `timeout_ms` for input events; true when readable.
    pub fn poll_in(&self, timeout_ms: i32) -> bool {
        let mut pfd = libc::pollfd { fd: self.fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: poll(2) on our own fd.
        let r = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        r > 0 && pfd.revents & libc::POLLIN != 0
    }

    /// A clock-family event for the subscribers of `from_port`, delivered at
    /// absolute monotonic time `at_ns` via `queue`.
    pub fn timed_clock_event(&mut self, queue: u8, from_port: u8, ty: u8, at_ns: u64, spp: u32) -> Event {
        let mut ev = Event::empty();
        ev.ty = ty;
        ev.flags = TIME_STAMP_REAL | TIME_MODE_ABS;
        ev.queue = queue;
        // Absolute monotonic time -> this queue's own clock.
        ev.time = RealTime::from_ns((at_ns as i64 - self.queue_epoch_ns).max(0) as u64);
        ev.source = Addr { client: self.client, port: from_port };
        ev.dest = Addr { client: ADDRESS_SUBSCRIBERS, port: ADDRESS_SUBSCRIBERS };
        // snd_seq_ev_ctrl { channel, pad[3], param(u32), value(i32) }:
        // SONGPOS rides in `value`.
        ev.data[2] = spp;
        ev
    }

    /// Same event, dispatched immediately (no queue).
    pub fn direct_clock_event(&mut self, from_port: u8, ty: u8) -> Event {
        let mut ev = Event::empty();
        ev.ty = ty;
        ev.flags = 0;
        ev.queue = QUEUE_DIRECT;
        ev.source = Addr { client: self.client, port: from_port };
        ev.dest = Addr { client: ADDRESS_SUBSCRIBERS, port: ADDRESS_SUBSCRIBERS };
        ev
    }

    /// A channel message (note on/off, controller, program change) given
    /// as its wire bytes, dispatched at once from `from_port`: to `dest`,
    /// or to the port's subscribers. `None` for any other message.
    pub fn midi_event(&self, from_port: u8, dest: Option<Addr>, bytes: &[u8]) -> Option<Event> {
        let mut ev = Event::empty();
        ev.queue = QUEUE_DIRECT;
        ev.source = Addr { client: self.client, port: from_port };
        ev.dest = dest.unwrap_or(Addr { client: ADDRESS_SUBSCRIBERS, port: ADDRESS_SUBSCRIBERS });
        fill_midi(&mut ev, bytes).then_some(ev)
    }

    /// The monotonic time the kernel stamped a received event with (a port
    /// set up with `stamp_port` on this client's own queue); `None` when
    /// the event carries no real-time stamp.
    pub fn event_mono_ns(&self, ev: &Event) -> Option<u64> {
        (ev.flags & TIME_STAMP_REAL != 0 && self.queue_epoch_ns != 0)
            .then(|| (self.queue_epoch_ns + ev.time.to_ns() as i64).max(0) as u64)
    }

    /// The ports of other clients that are gear or apps: MIDI outs to send
    /// to (`outs`) or MIDI ins to listen to. Never the system client, our
    /// own clients, or "Midi Through" unless `KABELSALAT_MIDI_THROUGH` asks.
    pub fn gear_ports(&mut self, outs: bool) -> Vec<PortDesc> {
        let own = self.client;
        let through_ok = std::env::var("KABELSALAT_MIDI_THROUGH").is_ok_and(|v| !v.is_empty() && v != "0");
        self.list_ports()
            .into_iter()
            .filter(|p| if outs { p.can_receive() } else { p.can_send() })
            .filter(|p| p.client != CLIENT_SYSTEM && p.client != own)
            .filter(|p| !p.client_name.contains("kabelsalat"))
            .filter(|p| through_ok || !p.client_name.contains("Midi Through"))
            .collect()
    }

    /// Total events handed to the kernel (diagnostics).
    pub fn events_written(&self) -> usize { self.written }

    /// Drop every event of ours still pending on `queue` (undelivered
    /// scheduled ticks/starts) and in the output pool — used when the clock
    /// stops or a pre-roll is cancelled.
    pub fn flush_pending(&mut self, queue: u8) -> anyhow::Result<()> {
        let mut rm: RemoveEvents = unsafe { std::mem::zeroed() };
        rm.remove_mode = REMOVE_OUTPUT;
        rm.queue = queue;
        ioctl(self.fd, iow(b'S', 0x4e, std::mem::size_of::<RemoveEvents>()),
              &mut rm as *mut RemoveEvents as *mut u8)
            .map_err(|e| anyhow::anyhow!("REMOVE_EVENTS: {e}"))?;
        Ok(())
    }
}

/// Fill `ev` from a channel message's wire bytes. `snd_seq_ev_note` is
/// { channel, note, velocity, off_velocity: u8, duration: u32 } and
/// `snd_seq_ev_ctrl` is { channel: u8, pad[3], param: u32, value: i32 }.
fn fill_midi(ev: &mut Event, bytes: &[u8]) -> bool {
    let Some(&status) = bytes.first() else { return false };
    let ch = (status & 0x0F) as u32;
    let d = |i: usize| bytes.get(i).map(|b| (b & 0x7F) as u32);
    match (status & 0xF0, d(1), d(2)) {
        (0x90, Some(note), Some(vel)) if vel > 0 => {
            ev.ty = EVENT_NOTEON;
            ev.data[0] = u32::from_ne_bytes([ch as u8, note as u8, vel as u8, 0]);
        }
        (0x80 | 0x90, Some(note), Some(vel)) => {
            ev.ty = EVENT_NOTEOFF;
            ev.data[0] = u32::from_ne_bytes([ch as u8, note as u8, vel as u8, 0]);
        }
        (0xB0, Some(cc), Some(value)) => {
            ev.ty = EVENT_CONTROLLER;
            ev.data = [u32::from_ne_bytes([ch as u8, 0, 0, 0]), cc, value];
        }
        (0xC0, Some(program), _) => {
            ev.ty = EVENT_PGMCHANGE;
            ev.data = [u32::from_ne_bytes([ch as u8, 0, 0, 0]), 0, program];
        }
        _ => return false,
    }
    true
}

impl Drop for SeqClient {
    fn drop(&mut self) {
        // SAFETY: close(2) on our own fd.
        unsafe { libc::close(self.fd); }
    }
}

// SAFETY: the client is a plain fd + counters; sharing it across the clock
// thread and test threads is sound (all methods take &mut self).
unsafe impl Send for SeqClient {}
unsafe impl Sync for SeqClient {}

/// Monotonic clock in nanoseconds — the sequencer queue's time base (the
/// default "system timer" is CLOCK_MONOTONIC).
pub fn mono_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime into a local struct.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts); }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_messages_become_sequencer_events_field_by_field() {
        let ev = |bytes: &[u8]| {
            let mut e = Event::empty();
            fill_midi(&mut e, bytes).then_some(e)
        };
        let first = |e: &Event| e.data[0].to_ne_bytes();
        let on = ev(&[0x99, 48, 100]).unwrap();
        assert_eq!((on.ty, first(&on)), (EVENT_NOTEON, [9, 48, 100, 0]));
        let off = ev(&[0x81, 48, 0]).unwrap();
        assert_eq!((off.ty, first(&off)), (EVENT_NOTEOFF, [1, 48, 0, 0]));
        assert_eq!(ev(&[0x90, 48, 0]).unwrap().ty, EVENT_NOTEOFF, "a note-on at velocity 0 is a note-off");
        let cc = ev(&[0xB1, 20, 127]).unwrap();
        assert_eq!((cc.ty, first(&cc)[0], cc.data[1], cc.data[2]), (EVENT_CONTROLLER, 1, 20, 127));
        let pc = ev(&[0xCF, 5]).unwrap();
        assert_eq!((pc.ty, first(&pc)[0], pc.data[2]), (EVENT_PGMCHANGE, 15, 5));
        assert!(ev(&[0xF8]).is_none() && ev(&[0xB0, 1]).is_none() && ev(&[]).is_none());
    }

    fn seq_or_skip() -> Option<SeqClient> {
        if !available() {
            eprintln!("skipping: no /dev/snd/seq on this machine");
            return None;
        }
        SeqClient::open().ok()
    }

    #[test]
    fn uapi_struct_sizes_are_the_kernel_layout() {
        // sizeof() of the structs in /usr/include/sound/asequencer.h (gcc, aarch64).
        assert_eq!(std::mem::size_of::<Event>(), 28);
        assert_eq!(std::mem::size_of::<ClientInfo>(), 188);
        assert_eq!(std::mem::size_of::<ClientPool>(), 88);
        assert_eq!(std::mem::size_of::<PortInfo>(), 168);
        assert_eq!(std::mem::size_of::<PortSubscribe>(), 80);
        assert_eq!(std::mem::size_of::<QueueInfo>(), 140);
        assert_eq!(std::mem::size_of::<QueueTempo>(), 44);
        assert_eq!(std::mem::size_of::<QueueStatus>(), 92);
        assert_eq!(std::mem::size_of::<RemoveEvents>(), 64);
    }

    #[test]
    fn open_client_create_port_and_list_ports() {
        let Some(mut c) = seq_or_skip() else { return };
        let port = c.create_port("MIDI Clock",
            PORT_CAP_READ | PORT_CAP_SUBS_READ).expect("create port");
        let ports = c.list_ports();
        // Client 0 ("System") with its timer/announce ports always exists.
        assert!(ports.iter().any(|p| p.client == 0 && p.port_name.contains("Timer")),
            "system timer port not found: {:?}", ports.iter().map(|p| p.full_name()).collect::<Vec<_>>());
        let mine = ports.iter().find(|p| p.client == c.client_id() && p.port == port).unwrap();
        assert_eq!(mine.port_name, "MIDI Clock");
        assert!(mine.capability & PORT_CAP_READ != 0);
        // Our own port is a source, not a destination.
        assert!(!mine.can_receive());
    }

    #[test]
    fn queue_runs_and_delivers_timed_events_sample_accurately() {
        let Some(mut sender) = seq_or_skip() else { return };
        let sp = sender.create_port("MIDI Clock", PORT_CAP_READ | PORT_CAP_SUBS_READ).unwrap();
        let q = sender.create_running_queue("clock-test").unwrap();

        // Test double: a second kernel client subscribed to our port.
        let mut rx = SeqClient::open().unwrap();
        let rx_port = rx.create_port("clock-listen", PORT_CAP_WRITE | PORT_CAP_SUBS_WRITE).unwrap();
        rx.subscribe(Addr { client: sender.client_id(), port: sp },
                     Addr { client: rx.client_id(), port: rx_port }).expect("subscribe");

        // The queue must be running (the alsa-lib queue-control recipe).
        let (running, _) = sender.queue_status(q).unwrap();
        assert!(running, "queue did not start");

        // Schedule Start + 5 ticks + a songpos, then a stop, spread over
        // 300 ms, all at absolute monotonic times (8 events).
        let now = mono_ns();
        let mut evs = vec![sender.timed_clock_event(q, sp, EVENT_START, now + 60_000_000, 0)];
        for i in 0..5u64 {
            evs.push(sender.timed_clock_event(q, sp, EVENT_CLOCK, now + 100_000_000 + i * 20_000_000, 0));
        }
        evs.push(sender.timed_clock_event(q, sp, EVENT_SONGPOS, now + 80_000_000, 96));
        evs.push(sender.direct_clock_event(sp, EVENT_STOP));
        sender.write_events(&evs).expect("write");

        // Collect from the receiver with timestamps; deadline 1.5 s.
        // The ARRIVAL time (monotonic, taken the moment poll wakes) is what
        // gear sees, not the stamp the kernel echoes back.
        let mut seen: Vec<(u64, u8, u32)> = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
        while std::time::Instant::now() < deadline && seen.len() < evs.len() {
            if !rx.poll_in(50) { continue; }
            let arrived = mono_ns();
            let mut buf = [Event::empty(); 16];
            let n = rx.read_events(&mut buf);
            for ev in &buf[..n] {
                if ev.source != (Addr { client: sender.client_id(), port: sp }) { continue; }
                seen.push((arrived, ev.ty, ev.songpos_value()));
            }
        }
        assert_eq!(seen.len(), 8, "expected Start + 5 ticks + SPP + Stop, got {seen:?}");
        // First arrival lands at its scheduled time within a few ms; the
        // Stop (direct) is the only one that may precede its logical order.
        let start = seen.iter().find(|(_, ty, _)| *ty == EVENT_START).expect("Start event");
        let drift = (start.0 as i64 - (now + 60_000_000) as i64).abs();
        assert!(drift < 5_000_000, "Start drifted {drift} ns — queue not timing delivery");
        // Ticks: 4 measured intervals of 20 ms, each within 2 ms.
        let ticks: Vec<u64> = seen.iter().filter(|(_, ty, _)| *ty == EVENT_CLOCK).map(|(t, _, _)| *t).collect();
        assert_eq!(ticks.len(), 5);
        for w in ticks.windows(2) {
            let dt = (w[1] - w[0]) as i64;
            assert!((dt - 20_000_000).abs() < 2_000_000, "tick interval {dt} ns, want 20 ms");
        }
        // SPP arrives with its 16th-note value.
        let spp = seen.iter().find(|(_, ty, _)| *ty == EVENT_SONGPOS).expect("SPP event");
        assert_eq!(spp.2, 96);
    }
}
