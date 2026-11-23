//! Show cues (#401): MIDI and OSC messages a song or a part sends when the
//! setlist enters it, for lighting desks, lyric prompters and other gear.
//!
//! A cue is session content: it lives on its [`crate::setlist::Song`] or
//! [`crate::setlist::Part`] and is written as one line of text in the app
//! and in the tools alike (`pc 1 5`, `cc 1 20 127 @desk`, `note 1 C3`,
//! `osc 192.168.1.50:8000 /cue/go 12`), so what the UI shows and what an
//! agent sets cannot differ.
//!
//! Threads: nothing here runs on the audio thread, and the audio thread
//! does no I/O. A part the engine stepped to on a foot switch is one `u32`
//! pushed into a lock-free ring; a part the app selected is a message on a
//! channel. Both end in the cue thread, which owns the ALSA sequencer port
//! ("kabelsalat: Show Cues") and the UDP socket and sends the song's cues
//! (when the song changed) and then the part's. A UI frame is never in the
//! path of a foot switch's cue (rule 31 spirit: a repaint that waits must
//! not make the lights wait).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::alsaseq::{self, Addr, SeqClient};
use crate::setlist::{note_label, parse_note};

/// Cues one song or one part may hold.
pub const MAX_CUES: usize = 16;
/// How long a note cue is held before its note-off.
pub const NOTE_LEN_NS: u64 = 100_000_000;
/// Sent messages kept for `get_cues` and the tests.
const LOG_LEN: usize = 64;
/// The cue thread's wait while a show has cues: the longest a foot switch's
/// cue sits in the ring.
const POLL_MS: u64 = 4;
/// Its wait while there is nothing to send.
const IDLE_MS: u64 = 100;
const RESCAN_SECS: f32 = 2.0;

/// One OSC argument: `T`/`F`, `i`, `f` or `s`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OscArg {
    Bool(bool),
    Int(i32),
    Float(f32),
    Str(String),
}

/// One message sent on song or part entry. MIDI channels are 1..=16 as
/// people count them; programs, controllers and notes are the wire's 0..=127.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Cue {
    Program {
        channel: u8,
        program: u8,
        /// Part of a MIDI out port's name; none = every MIDI out port.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<String>,
    },
    Cc {
        channel: u8,
        cc: u8,
        value: u8,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<String>,
    },
    Note {
        channel: u8,
        note: u8,
        velocity: u8,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<String>,
    },
    Osc {
        host: String,
        port: u16,
        address: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<OscArg>,
    },
}

const SYNTAX: &str = "write a cue as: pc <channel> <program> | cc <channel> <controller> <value> | \
note <channel> <note> [velocity] (each optionally followed by @<part of a MIDI out port name>) | \
osc <host>:<port> </address> [arguments]";

fn midi_num(word: Option<&str>, what: &str, lo: u8, hi: u8) -> Result<u8, String> {
    let w = word.ok_or_else(|| format!("{what} is missing; {SYNTAX}"))?;
    match w.parse::<u8>() {
        Ok(n) if (lo..=hi).contains(&n) => Ok(n),
        _ => Err(format!("{what} '{w}' is not a number from {lo} to {hi}")),
    }
}

/// Split OSC arguments: words, and "quoted strings" that keep their spaces.
fn osc_words(text: &str) -> Result<Vec<(String, bool)>, String> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '"' {
            chars.next();
            let mut s = String::new();
            loop {
                match chars.next() {
                    Some('"') => break,
                    Some('\\') => s.push(chars.next().ok_or("a quoted OSC string ends in a backslash")?),
                    Some(ch) => s.push(ch),
                    None => return Err("a quoted OSC string is missing its closing \"".into()),
                }
            }
            out.push((s, true));
        } else {
            let mut s = String::new();
            while let Some(&ch) = chars.peek() {
                if ch.is_whitespace() {
                    break;
                }
                s.push(ch);
                chars.next();
            }
            out.push((s, false));
        }
    }
    Ok(out)
}

fn osc_arg(word: &str, quoted: bool) -> OscArg {
    if quoted {
        return OscArg::Str(word.to_string());
    }
    match word {
        "true" => return OscArg::Bool(true),
        "false" => return OscArg::Bool(false),
        _ => {}
    }
    if let Ok(i) = word.parse::<i32>() {
        return OscArg::Int(i);
    }
    match word.parse::<f32>() {
        Ok(f) if f.is_finite() => OscArg::Float(f),
        _ => OscArg::Str(word.to_string()),
    }
}

impl Cue {
    /// Read one line of cue text.
    pub fn parse(line: &str) -> Result<Cue, String> {
        let line = line.trim();
        let (kind, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let kind = kind.to_ascii_lowercase();
        if kind == "osc" {
            let rest = rest.trim();
            let (dest, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            let (host, port) = dest
                .rsplit_once(':')
                .ok_or_else(|| format!("an OSC cue needs <host>:<port> first, got '{dest}'"))?;
            let port: u16 = port
                .parse()
                .ok()
                .filter(|p| *p != 0)
                .ok_or_else(|| format!("'{port}' is not a UDP port (1 to 65535)"))?;
            if host.is_empty() {
                return Err("an OSC cue needs a host before the port".into());
            }
            let mut words = osc_words(tail)?.into_iter();
            let address = match words.next() {
                Some((a, false)) if a.starts_with('/') && a.len() > 1 => a,
                Some((a, _)) => return Err(format!("an OSC address starts with '/', got '{a}'")),
                None => return Err("an OSC cue needs an address like /cue/go".into()),
            };
            let args = words.map(|(w, q)| osc_arg(&w, q)).collect();
            return Ok(Cue::Osc { host: host.to_string(), port, address, args });
        }
        let (body, port) = match rest.split_once('@') {
            Some((b, p)) if !p.trim().is_empty() => (b, Some(p.trim().to_string())),
            Some(_) => return Err("a port name is missing after @".into()),
            None => (rest, None),
        };
        let mut w = body.split_whitespace();
        let cue = match kind.as_str() {
            "pc" | "program" => Cue::Program {
                channel: midi_num(w.next(), "channel", 1, 16)?,
                program: midi_num(w.next(), "program", 0, 127)?,
                port,
            },
            "cc" => Cue::Cc {
                channel: midi_num(w.next(), "channel", 1, 16)?,
                cc: midi_num(w.next(), "controller", 0, 127)?,
                value: midi_num(w.next(), "value", 0, 127)?,
                port,
            },
            "note" => {
                let channel = midi_num(w.next(), "channel", 1, 16)?;
                let word = w.next().ok_or_else(|| format!("note is missing; {SYNTAX}"))?;
                let note = parse_note(word).ok_or_else(|| format!("'{word}' is not a note (0 to 127, or a name like C3)"))?;
                let velocity = match w.next() {
                    Some(v) => midi_num(Some(v), "velocity", 1, 127)?,
                    None => 100,
                };
                Cue::Note { channel, note, velocity, port }
            }
            "" => return Err(format!("empty cue; {SYNTAX}")),
            other => return Err(format!("'{other}' is not a cue; {SYNTAX}")),
        };
        if let Some(extra) = w.next() {
            return Err(format!("'{extra}' is one word too many; {SYNTAX}"));
        }
        Ok(cue)
    }

    /// The line [`Cue::parse`] reads back as this cue.
    pub fn text(&self) -> String {
        let at = |p: &Option<String>| p.as_ref().map_or(String::new(), |p| format!(" @{p}"));
        match self {
            Cue::Program { channel, program, port } => format!("pc {channel} {program}{}", at(port)),
            Cue::Cc { channel, cc, value, port } => format!("cc {channel} {cc} {value}{}", at(port)),
            Cue::Note { channel, note, velocity, port } => {
                format!("note {channel} {} {velocity}{}", note_label(*note), at(port))
            }
            Cue::Osc { host, port, address, args } => {
                let mut s = format!("osc {host}:{port} {address}");
                for a in args {
                    s.push(' ');
                    match a {
                        OscArg::Bool(b) => s.push_str(if *b { "true" } else { "false" }),
                        OscArg::Int(i) => s.push_str(&i.to_string()),
                        OscArg::Float(f) => s.push_str(&format!("{f:?}")),
                        OscArg::Str(t) => {
                            let bare = !t.is_empty()
                                && !t.contains(|c: char| c.is_whitespace() || c == '"' || c == '\\')
                                && osc_arg(t, false) == OscArg::Str(t.clone());
                            if bare {
                                s.push_str(t);
                            } else {
                                s.push('"');
                                for c in t.chars() {
                                    if c == '"' || c == '\\' {
                                        s.push('\\');
                                    }
                                    s.push(c);
                                }
                                s.push('"');
                            }
                        }
                    }
                }
                s
            }
        }
    }

    /// Whatever a file holds, made sendable: channels 1..=16, data bytes
    /// 0..=127, a note velocity of at least 1.
    pub fn normalized(mut self) -> Cue {
        match &mut self {
            Cue::Program { channel, program, .. } => {
                *channel = (*channel).clamp(1, 16);
                *program = (*program).min(127);
            }
            Cue::Cc { channel, cc, value, .. } => {
                *channel = (*channel).clamp(1, 16);
                *cc = (*cc).min(127);
                *value = (*value).min(127);
            }
            Cue::Note { channel, note, velocity, .. } => {
                *channel = (*channel).clamp(1, 16);
                *note = (*note).min(127);
                *velocity = (*velocity).clamp(1, 127);
            }
            Cue::Osc { .. } => {}
        }
        self
    }

    /// The MIDI message on the wire and the port it names; `None` for OSC.
    pub fn midi(&self) -> Option<(Vec<u8>, Option<&str>)> {
        let st = |kind: u8, channel: u8| kind | (channel.clamp(1, 16) - 1);
        match self {
            Cue::Program { channel, program, port } => Some((vec![st(0xC0, *channel), program & 0x7F], port.as_deref())),
            Cue::Cc { channel, cc, value, port } => Some((vec![st(0xB0, *channel), cc & 0x7F, value & 0x7F], port.as_deref())),
            Cue::Note { channel, note, velocity, port } => {
                Some((vec![st(0x90, *channel), note & 0x7F, (*velocity).clamp(1, 127)], port.as_deref()))
            }
            Cue::Osc { .. } => None,
        }
    }
}

/// Parse a list of cue lines (a tool's array, a text field's lines); empty
/// lines are skipped. The error names the line.
pub fn parse_lines<'a>(lines: impl IntoIterator<Item = &'a str>) -> Result<Vec<Cue>, String> {
    let mut out = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        out.push(Cue::parse(line).map_err(|e| format!("cue '{}': {e}", line.trim()))?);
    }
    if out.len() > MAX_CUES {
        return Err(format!("{} cues are more than the {MAX_CUES} one song or part holds", out.len()));
    }
    Ok(out)
}

fn osc_pad(out: &mut Vec<u8>) {
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

fn osc_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(s.as_bytes());
    out.push(0);
    osc_pad(out);
}

/// One OSC 1.0 message: the address, the type tags, the arguments, each
/// padded to four bytes, numbers big-endian.
pub fn osc_packet(address: &str, args: &[OscArg]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    osc_str(&mut out, address);
    let mut tags = String::from(",");
    for a in args {
        tags.push(match a {
            OscArg::Bool(true) => 'T',
            OscArg::Bool(false) => 'F',
            OscArg::Int(_) => 'i',
            OscArg::Float(_) => 'f',
            OscArg::Str(_) => 's',
        });
    }
    osc_str(&mut out, &tags);
    for a in args {
        match a {
            OscArg::Bool(_) => {}
            OscArg::Int(i) => out.extend_from_slice(&i.to_be_bytes()),
            OscArg::Float(f) => out.extend_from_slice(&f.to_be_bytes()),
            OscArg::Str(s) => osc_str(&mut out, s),
        }
    }
    out
}

// ---- the table and the dispatcher ----------------------------------------

/// The cues of one part, at the part's index in `Setlist::show_parts` (the
/// index the engine steps by).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CueSlot {
    pub song: usize,
    pub song_cues: Vec<Cue>,
    pub part_cues: Vec<Cue>,
}

pub type CueTable = Arc<[CueSlot]>;

/// A message that went out (or was tried): what the tools report and the
/// tests read.
#[derive(Clone, Debug, PartialEq)]
pub enum Sent {
    Midi { bytes: Vec<u8>, port: Option<String> },
    Osc { to: String, packet: Vec<u8> },
}

#[derive(Clone, Debug, Default)]
pub struct CueStats {
    pub midi_sent: u64,
    pub osc_sent: u64,
    /// The newest messages, oldest first.
    pub log: Vec<Sent>,
    pub last_error: String,
    /// "ok", "unavailable" (no ALSA sequencer: OSC still goes out) or
    /// "none" (no cue thread: tests and headless runs).
    pub alsa: &'static str,
    /// Live MIDI out ports by stable name (rule 58).
    pub ports: Vec<String>,
}

/// Where MIDI cues go. `port` is the part of a port name a cue gave.
pub trait MidiSink: Send {
    fn send(&mut self, bytes: &[u8], port: Option<&str>) -> Result<(), String>;
}

/// No MIDI output: tests and headless runs (what was sent is in the log).
struct NoMidi;

impl MidiSink for NoMidi {
    fn send(&mut self, _bytes: &[u8], _port: Option<&str>) -> Result<(), String> {
        Ok(())
    }
}

pub enum CueMsg {
    /// A new table, and the active part in it (entering nothing).
    Table { table: CueTable, active: Option<usize> },
    /// The setlist entered the part at this index.
    Enter(usize),
    /// Send these now (the Test button, the `test` flag of `set_cues`).
    Fire(Vec<Cue>),
    /// A new engine's ring of stepped parts (the audio stream was reopened).
    Ring(rtrb::Consumer<u32>),
}

/// The dispatcher: knows the table and the song it is in, sends cues, and
/// releases note cues after [`NOTE_LEN_NS`].
pub struct CueEngine {
    table: CueTable,
    song: Option<usize>,
    midi: Box<dyn MidiSink>,
    sock: Option<UdpSocket>,
    resolved: HashMap<String, SocketAddr>,
    /// Note-offs due: (when, message, port).
    offs: Vec<(u64, [u8; 3], Option<String>)>,
    /// The parts the engine stepped to on a foot switch.
    ring: Option<rtrb::Consumer<u32>>,
    stats: Arc<Mutex<CueStats>>,
}

impl CueEngine {
    fn new(midi: Box<dyn MidiSink>, stats: Arc<Mutex<CueStats>>) -> Self {
        CueEngine {
            table: Arc::from(Vec::new()),
            song: None,
            midi,
            sock: None,
            resolved: HashMap::new(),
            offs: Vec::new(),
            ring: None,
            stats,
        }
    }

    fn has_cues(&self) -> bool {
        self.table.iter().any(|s| !s.song_cues.is_empty() || !s.part_cues.is_empty())
    }

    pub fn handle(&mut self, msg: CueMsg, now_ns: u64) {
        match msg {
            CueMsg::Table { table, active } => {
                self.song = active.and_then(|i| table.get(i)).map(|s| s.song);
                self.table = table;
            }
            CueMsg::Enter(index) => {
                let table = self.table.clone();
                let Some(slot) = table.get(index) else { return };
                if self.song != Some(slot.song) {
                    self.song = Some(slot.song);
                    for c in &slot.song_cues {
                        self.send(c, now_ns);
                    }
                }
                for c in &slot.part_cues {
                    self.send(c, now_ns);
                }
            }
            CueMsg::Fire(cues) => {
                for c in &cues {
                    self.send(c, now_ns);
                }
            }
            CueMsg::Ring(rx) => self.ring = Some(rx),
        }
    }

    /// Send the cues of the parts the engine stepped to, and the note-offs
    /// that are due.
    pub fn tick(&mut self, now_ns: u64) {
        let mut stepped = Vec::new();
        if let Some(rx) = self.ring.as_mut() {
            while let Ok(i) = rx.pop() {
                stepped.push(i as usize);
            }
        }
        for i in stepped {
            self.handle(CueMsg::Enter(i), now_ns);
        }
        let mut i = 0;
        while i < self.offs.len() {
            if self.offs[i].0 <= now_ns {
                let (_, bytes, port) = self.offs.remove(i);
                self.send_midi(&bytes, port.as_deref());
            } else {
                i += 1;
            }
        }
    }

    fn note(&mut self, f: impl FnOnce(&mut CueStats)) {
        if let Ok(mut s) = self.stats.lock() {
            f(&mut s);
            let extra = s.log.len().saturating_sub(LOG_LEN);
            s.log.drain(..extra);
        }
    }

    fn send_midi(&mut self, bytes: &[u8], port: Option<&str>) {
        let res = self.midi.send(bytes, port);
        let sent = Sent::Midi { bytes: bytes.to_vec(), port: port.map(str::to_string) };
        self.note(|s| {
            s.log.push(sent);
            match res {
                Ok(()) => s.midi_sent += 1,
                Err(e) => s.last_error = e,
            }
        });
    }

    fn osc_dest(&mut self, host: &str, port: u16) -> Result<SocketAddr, String> {
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(SocketAddr::new(ip, port));
        }
        let key = format!("{host}:{port}");
        if let Some(a) = self.resolved.get(&key) {
            return Ok(*a);
        }
        let addr = (host, port)
            .to_socket_addrs()
            .map_err(|e| format!("OSC host '{host}': {e}"))?
            .next()
            .ok_or_else(|| format!("OSC host '{host}' has no address"))?;
        self.resolved.insert(key, addr);
        Ok(addr)
    }

    fn send_osc(&mut self, host: &str, port: u16, address: &str, args: &[OscArg]) {
        let packet = osc_packet(address, args);
        let res = self.osc_dest(host, port).and_then(|dest| {
            if self.sock.is_none() {
                let s = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("OSC socket: {e}"))?;
                // Lighting desks are often addressed by broadcast.
                let _ = s.set_broadcast(true);
                let _ = s.set_nonblocking(true);
                self.sock = Some(s);
            }
            let sock = self.sock.as_ref().ok_or("OSC socket missing")?;
            sock.send_to(&packet, dest).map(|_| ()).map_err(|e| format!("OSC to {dest}: {e}"))
        });
        let sent = Sent::Osc { to: format!("{host}:{port}"), packet };
        self.note(|s| {
            s.log.push(sent);
            match res {
                Ok(()) => s.osc_sent += 1,
                Err(e) => s.last_error = e,
            }
        });
    }

    fn send(&mut self, cue: &Cue, now_ns: u64) {
        match cue {
            Cue::Osc { host, port, address, args } => self.send_osc(host, *port, address, args),
            _ => {
                let Some((bytes, port)) = cue.midi() else { return };
                self.send_midi(&bytes, port);
                if let Cue::Note { .. } = cue {
                    let off = [0x80 | (bytes[0] & 0x0F), bytes[1], 0];
                    self.offs.push((now_ns + NOTE_LEN_NS, off, port.map(str::to_string)));
                }
            }
        }
    }
}

/// The app's handle on the cues: the UI, the agent tools and the cue thread
/// share it. Without a thread (tests, headless) it dispatches inline into
/// the log, with OSC really sent.
pub struct CueControl {
    tx: Mutex<Option<mpsc::Sender<CueMsg>>>,
    inline: Mutex<CueEngine>,
    stats: Arc<Mutex<CueStats>>,
    /// The table last published and the active part then, so a frame
    /// sends only a change.
    sent: Mutex<(CueTable, Option<usize>)>,
}

impl CueControl {
    pub fn new() -> Arc<Self> {
        let stats = Arc::new(Mutex::new(CueStats { alsa: "none", ..CueStats::default() }));
        Arc::new(CueControl {
            tx: Mutex::new(None),
            inline: Mutex::new(CueEngine::new(Box::new(NoMidi), stats.clone())),
            stats,
            sent: Mutex::new((Arc::from(Vec::new()), None)),
        })
    }

    fn post(&self, msg: CueMsg) {
        let tx = self.tx.lock().ok().and_then(|t| t.clone());
        match tx {
            Some(tx) => {
                let _ = tx.send(msg);
            }
            None => {
                if let Ok(mut e) = self.inline.lock() {
                    let now = alsaseq::mono_ns();
                    e.handle(msg, now);
                    // No thread releases a note later: release it at once.
                    e.tick(u64::MAX);
                }
            }
        }
    }

    /// Hand the dispatcher the setlist's cues when they changed. Entering
    /// nothing: an edit, a load or a reorder sends no cue.
    pub fn publish(&self, setlist: &crate::setlist::Setlist) {
        let now = setlist.cue_table();
        let Ok(mut sent) = self.sent.lock() else { return };
        let active = setlist.active_index();
        if sent.0[..] == now[..] {
            sent.1 = active;
            return;
        }
        let table: CueTable = Arc::from(now);
        *sent = (table.clone(), active);
        drop(sent);
        self.post(CueMsg::Table { table, active });
    }

    /// The setlist entered the part at `index` of `Setlist::show_parts`.
    pub fn enter(&self, index: usize) {
        self.post(CueMsg::Enter(index));
    }

    /// Give the cue thread a new engine's ring of stepped parts, and return
    /// the producer for the engine. Inline (no thread) the ring is polled by
    /// `tick`, which the tests call.
    pub fn engine_ring(&self) -> rtrb::Producer<u32> {
        let (tx, rx) = rtrb::RingBuffer::<u32>::new(64);
        self.post_raw(CueMsg::Ring(rx));
        tx
    }

    fn post_raw(&self, msg: CueMsg) {
        let tx = self.tx.lock().ok().and_then(|t| t.clone());
        match tx {
            Some(tx) => {
                let _ = tx.send(msg);
            }
            None => {
                if let Ok(mut e) = self.inline.lock() {
                    e.handle(msg, alsaseq::mono_ns());
                }
            }
        }
    }

    /// Run the inline dispatcher's pending work (no thread: tests).
    #[cfg(test)]
    fn poll_inline(&self) {
        if let Ok(mut e) = self.inline.lock() {
            e.tick(alsaseq::mono_ns());
        }
    }

    /// Send these cues now.
    pub fn fire(&self, cues: Vec<Cue>) {
        self.post(CueMsg::Fire(cues));
    }

    pub fn stats(&self) -> CueStats {
        self.stats.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// What `get_cues` says about the output side.
    pub fn status_json(&self) -> serde_json::Value {
        let s = self.stats();
        serde_json::json!({
            "midi_sent": s.midi_sent,
            "osc_sent": s.osc_sent,
            "midi": match s.alsa {
                "ok" => "ok",
                "unavailable" => "no ALSA sequencer on this machine: MIDI cues are not sent, OSC cues are",
                _ => "no cue output in this run (headless): cues are logged, OSC is sent",
            },
            "virtual_port": "kabelsalat: Show Cues",
            "midi_out_ports": s.ports,
            "last_error": s.last_error,
        })
    }
}

/// The one cue control of the process.
pub fn global() -> Arc<CueControl> {
    static CUES: std::sync::OnceLock<Arc<CueControl>> = std::sync::OnceLock::new();
    CUES.get_or_init(CueControl::new).clone()
}

// ---- the cue thread --------------------------------------------------------

/// MIDI cues through the ALSA sequencer: our "Show Cues" port (apps
/// subscribe to it) and, directly, the hardware MIDI out ports.
struct AlsaSink {
    seq: SeqClient,
    port: u8,
    /// Live MIDI out ports: stable name and address.
    outs: Vec<(String, Addr)>,
}

impl MidiSink for AlsaSink {
    fn send(&mut self, bytes: &[u8], port: Option<&str>) -> Result<(), String> {
        let mut events = Vec::with_capacity(self.outs.len() + 1);
        match port {
            None => {
                events.extend(self.seq.midi_event(self.port, None, bytes));
                for (_, addr) in &self.outs {
                    events.extend(self.seq.midi_event(self.port, Some(*addr), bytes));
                }
            }
            Some(name) => {
                let want = name.to_lowercase();
                let hits: Vec<Addr> =
                    self.outs.iter().filter(|(k, _)| k.to_lowercase().contains(&want)).map(|(_, a)| *a).collect();
                if hits.is_empty() {
                    let names: Vec<&str> = self.outs.iter().map(|(k, _)| k.as_str()).collect();
                    return Err(format!("no MIDI out port '{name}' (ports: {})", names.join(", ")));
                }
                for addr in hits {
                    events.extend(self.seq.midi_event(self.port, Some(addr), bytes));
                }
            }
        }
        self.seq.write_events(&events).map_err(|e| e.to_string())
    }
}

/// Spawn the cue thread: it owns the sequencer client and the UDP socket,
/// takes the app's messages and the parts the engine stepped to (`steps`),
/// and ends with the app.
pub fn spawn_cue_thread(ctl: Arc<CueControl>) {
    let (tx, rx) = mpsc::channel::<CueMsg>();
    // The table the app published before the thread existed.
    if let Ok(sent) = ctl.sent.lock() {
        let _ = tx.send(CueMsg::Table { table: sent.0.clone(), active: sent.1 });
    }
    let stats = ctl.stats.clone();
    let spawned = std::thread::Builder::new().name("kabelsalat-cues".into()).spawn({
        let ctl = ctl.clone();
        move || {
            let opened = SeqClient::open().and_then(|mut s| {
                let port = s.create_port("Show Cues", alsaseq::PORT_CAP_READ | alsaseq::PORT_CAP_SUBS_READ)?;
                Ok((s, port))
            });
            // The sink stays on this thread; `Shared` is the engine's boxed
            // view of it, so the rescan below can refresh its port list.
            let slot: Arc<Mutex<Option<AlsaSink>>> = Arc::new(Mutex::new(None));
            let midi: Box<dyn MidiSink> = match opened {
                Ok((seq, port)) => {
                    if let Ok(mut s) = stats.lock() {
                        s.alsa = "ok";
                    }
                    if let Ok(mut a) = slot.lock() {
                        *a = Some(AlsaSink { seq, port, outs: Vec::new() });
                    }
                    Box::new(Shared(slot.clone()))
                }
                Err(e) => {
                    eprintln!("show cues: ALSA sequencer unavailable ({e}): MIDI cues are not sent, OSC cues are");
                    if let Ok(mut s) = stats.lock() {
                        s.alsa = "unavailable";
                    }
                    Box::new(Unavailable)
                }
            };
            let mut engine = CueEngine::new(midi, stats.clone());
            // The ring the engine was given before this thread existed.
            engine.ring = ctl.inline.lock().ok().and_then(|mut e| e.ring.take());
            let mut last_rescan = std::time::Instant::now() - std::time::Duration::from_secs(10);
            loop {
                if Arc::strong_count(&ctl) < 2 {
                    break;
                }
                // Parked while there is nothing to send (rule 47); a show
                // with cues is looked at every few ms for a foot switch.
                let busy = engine.has_cues() || !engine.offs.is_empty() || engine.ring.is_some();
                let wait = std::time::Duration::from_millis(if busy { POLL_MS } else { IDLE_MS });
                match rx.recv_timeout(wait) {
                    Ok(msg) => {
                        engine.handle(msg, alsaseq::mono_ns());
                        while let Ok(msg) = rx.try_recv() {
                            engine.handle(msg, alsaseq::mono_ns());
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
                engine.tick(alsaseq::mono_ns());
                if last_rescan.elapsed().as_secs_f32() >= RESCAN_SECS {
                    last_rescan = std::time::Instant::now();
                    if let Ok(mut a) = slot.lock() {
                        if let Some(a) = a.as_mut() {
                            a.outs = a.seq.gear_ports(true).into_iter().map(|p| {
                                (crate::clocksend::midi_port_key(&p.full_name()).to_string(), Addr { client: p.client, port: p.port })
                            }).collect();
                            if let Ok(mut s) = stats.lock() {
                                s.ports = a.outs.iter().map(|(k, _)| k.clone()).collect();
                            }
                        }
                    }
                }
            }
        }
    });
    match spawned {
        Ok(_) => {
            if let Ok(mut t) = ctl.tx.lock() {
                *t = Some(tx);
            }
        }
        Err(e) => eprintln!("show cues: thread failed to start: {e}"),
    }
}

/// The engine's boxed view of the thread's ALSA sink.
struct Shared(Arc<Mutex<Option<AlsaSink>>>);

impl MidiSink for Shared {
    fn send(&mut self, bytes: &[u8], port: Option<&str>) -> Result<(), String> {
        match self.0.lock().map_err(|_| "cue output lock".to_string())?.as_mut() {
            Some(a) => a.send(bytes, port),
            None => Err("no MIDI output".into()),
        }
    }
}

struct Unavailable;

impl MidiSink for Unavailable {
    fn send(&mut self, _bytes: &[u8], _port: Option<&str>) -> Result<(), String> {
        Err("no ALSA sequencer on this machine: the MIDI cue was not sent".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setlist::{Part, PartRig, Setlist, SetlistPos, Song};

    #[test]
    fn a_cue_line_reads_back_as_itself_and_a_wrong_one_says_why() {
        for line in [
            "pc 1 5",
            "pc 16 127 @MIDIMATE eX",
            "cc 2 20 127",
            "note 10 C3 100",
            "note 1 F#2 64 @desk",
            "osc 192.168.1.50:8000 /cue/go",
            "osc localhost:9000 /lyrics/page 12 1.5 chorus \"two words\" true false \"12\"",
        ] {
            let cue = Cue::parse(line).unwrap_or_else(|e| panic!("{line}: {e}"));
            assert_eq!(cue.text(), line);
            assert_eq!(Cue::parse(&cue.text()).unwrap(), cue);
            let json = serde_json::to_string(&cue).unwrap();
            assert_eq!(serde_json::from_str::<Cue>(&json).unwrap(), cue, "{json}");
        }
        assert_eq!(Cue::parse("PROGRAM 3 0").unwrap(), Cue::Program { channel: 3, program: 0, port: None });
        assert_eq!(Cue::parse("note 1 60").unwrap(), Cue::Note { channel: 1, note: 60, velocity: 100, port: None });
        assert_eq!(
            Cue::parse("osc 10.0.0.2:7700 /a 1.0 -3").unwrap(),
            Cue::Osc { host: "10.0.0.2".into(), port: 7700, address: "/a".into(), args: vec![OscArg::Float(1.0), OscArg::Int(-3)] }
        );
        for (bad, why) in [
            ("pc 0 5", "channel"),
            ("pc 1 128", "program"),
            ("cc 1 20", "value is missing"),
            ("cc 1 20 127 9", "too many"),
            ("note 1 H2", "not a note"),
            ("pc 1 5 @", "port name"),
            ("osc 10.0.0.2 /a", "<host>:<port>"),
            ("osc 10.0.0.2:0 /a", "UDP port"),
            ("osc 10.0.0.2:9000 cue", "starts with '/'"),
            ("osc 10.0.0.2:9000 /a \"open", "closing"),
            ("dmx 1 255", "not a cue"),
        ] {
            let e = Cue::parse(bad).unwrap_err();
            assert!(e.contains(why), "{bad}: {e}");
        }
        assert!(parse_lines(["pc 1 1"; MAX_CUES + 1]).unwrap_err().contains("more than"));
        assert_eq!(parse_lines(["", "pc 1 1", "  "]).unwrap().len(), 1);
    }

    #[test]
    fn midi_cues_are_the_bytes_on_the_wire() {
        let bytes = |line: &str| Cue::parse(line).unwrap().midi().unwrap().0;
        assert_eq!(bytes("pc 1 5"), [0xC0, 5]);
        assert_eq!(bytes("pc 16 127"), [0xCF, 127]);
        assert_eq!(bytes("cc 2 20 127"), [0xB1, 20, 127]);
        assert_eq!(bytes("note 10 C3 100"), [0x99, 48, 100]);
        assert_eq!(Cue::parse("cc 1 7 1 @desk").unwrap().midi().unwrap().1, Some("desk"));
        assert!(Cue::parse("osc 1.2.3.4:1 /a").unwrap().midi().is_none());
        // A hand-edited file cannot put a status byte into a data byte.
        let wild = Cue::Note { channel: 40, note: 200, velocity: 0, port: None }.normalized();
        assert_eq!(wild.midi().unwrap().0, [0x9F, 127, 1]);
    }

    #[test]
    fn an_osc_packet_is_padded_tagged_and_big_endian() {
        assert_eq!(osc_packet("/cue/go", &[]), b"/cue/go\0,\0\0\0");
        let p = osc_packet(
            "/a",
            &[OscArg::Int(1), OscArg::Float(1.5), OscArg::Str("hey".into()), OscArg::Bool(true), OscArg::Bool(false)],
        );
        let mut want = Vec::new();
        want.extend_from_slice(b"/a\0\0");
        want.extend_from_slice(b",ifsTF\0\0");
        want.extend_from_slice(&[0, 0, 0, 1]);
        want.extend_from_slice(&[0x3F, 0xC0, 0, 0]);
        want.extend_from_slice(b"hey\0");
        assert_eq!(p, want);
        // A string whose length is a multiple of four still ends in a NUL.
        assert_eq!(osc_packet("/abc", &[OscArg::Str("four".into())]), b"/abc\0\0\0\0,s\0\0four\0\0\0\0");
    }

    fn show() -> Setlist {
        let part = |name: &str, cues: &[&str]| Part {
            cues: parse_lines(cues.iter().copied()).unwrap(),
            ..Part::new(name, vec![PartRig::new(0)])
        };
        let mut sl = Setlist::default();
        sl.songs = vec![
            Song {
                cues: parse_lines(["pc 1 10"]).unwrap(),
                ..Song::new("One", vec![part("Verse", &["cc 1 20 1"]), part("Chorus", &["cc 1 20 2", "note 2 C3 90"])])
            },
            Song { cues: parse_lines(["pc 1 11 @desk"]).unwrap(), ..Song::new("Two", vec![part("Main", &[])]) },
        ];
        sl.active = Some(SetlistPos { song: 0, part: 0 });
        sl
    }

    fn midi_log(ctl: &CueControl) -> Vec<Vec<u8>> {
        ctl.stats().log.iter().filter_map(|s| match s {
            Sent::Midi { bytes, .. } => Some(bytes.clone()),
            Sent::Osc { .. } => None,
        }).collect()
    }

    #[test]
    fn entering_a_part_sends_its_cues_and_the_songs_only_when_the_song_changes() {
        let sl = show();
        let ctl = CueControl::new();
        ctl.publish(&sl);
        assert!(midi_log(&ctl).is_empty(), "publishing the table (a load, an edit) sends nothing");
        // Verse → Chorus: the same song, so only the part's cues, the note
        // with its release.
        ctl.enter(1);
        assert_eq!(midi_log(&ctl), [vec![0xB0, 20, 2], vec![0x91, 48, 90], vec![0x81, 48, 0]]);
        // Chorus → song Two: the song's cue (to the port it names), no part cue.
        ctl.enter(2);
        assert_eq!(midi_log(&ctl).len(), 4);
        assert_eq!(ctl.stats().log[3], Sent::Midi { bytes: vec![0xC0, 11], port: Some("desk".into()) });
        // Back into song One at its first part: song cue, then part cue.
        ctl.enter(0);
        assert_eq!(midi_log(&ctl)[4..], [vec![0xC0, 10], vec![0xB0, 20, 1]]);
        // An index past the table (a stale step) sends nothing.
        ctl.enter(9);
        assert_eq!(midi_log(&ctl).len(), 6);
        assert_eq!(ctl.stats().midi_sent, 6);
    }

    #[test]
    fn a_reorder_republishes_without_sending_and_keeps_the_song_it_is_in() {
        let mut sl = show();
        let ctl = CueControl::new();
        ctl.publish(&sl);
        // Song One moves behind song Two: the active part's index changes.
        sl.move_song(0, 1);
        assert_eq!(sl.active_index(), Some(1));
        ctl.publish(&sl);
        assert!(midi_log(&ctl).is_empty());
        // Verse → Chorus is still inside song One: no song cue.
        ctl.enter(2);
        assert_eq!(midi_log(&ctl)[0], [0xB0, 20, 2]);
        assert!(!midi_log(&ctl).contains(&vec![0xC0, 10]));
    }

    #[test]
    fn an_osc_cue_arrives_on_a_local_udp_socket() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let port = rx.local_addr().unwrap().port();
        let mut sl = show();
        sl.songs[1].parts[0].cues = parse_lines([format!("osc 127.0.0.1:{port} /cue/go 7 \"Song Two\"").as_str()]).unwrap();
        let ctl = CueControl::new();
        ctl.publish(&sl);
        ctl.enter(2);
        let mut buf = [0u8; 256];
        let (n, _) = rx.recv_from(&mut buf).expect("the OSC cue arrives");
        let mut want = Vec::new();
        want.extend_from_slice(b"/cue/go\0,is\0");
        want.extend_from_slice(&[0, 0, 0, 7]);
        want.extend_from_slice(b"Song Two\0\0\0\0");
        assert_eq!(&buf[..n], &want[..]);
        let stats = ctl.stats();
        assert_eq!(stats.osc_sent, 1);
        assert!(stats.last_error.is_empty(), "{}", stats.last_error);
        // The song's MIDI cue went first, then the part's OSC.
        assert!(matches!(stats.log[..], [Sent::Midi { .. }, Sent::Osc { .. }]));
    }

    /// The thread path: a part the engine stepped to arrives through the
    /// ring, a part the app selected through the channel.
    #[test]
    fn the_cue_thread_sends_what_the_engine_ring_and_the_app_enter() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
        let port = rx.local_addr().unwrap().port();
        let mut sl = show();
        for (i, s) in sl.songs.iter_mut().enumerate() {
            for (j, p) in s.parts.iter_mut().enumerate() {
                p.cues = parse_lines([format!("osc 127.0.0.1:{port} /part {i} {j}").as_str()]).unwrap();
            }
        }
        let ctl = CueControl::new();
        ctl.publish(&sl);
        spawn_cue_thread(ctl.clone());
        let mut steps_tx = ctl.engine_ring();
        let mut next = || {
            let mut buf = [0u8; 64];
            let (n, _) = rx.recv_from(&mut buf).expect("a cue arrives");
            buf[..n].to_vec()
        };
        steps_tx.push(1).unwrap();
        assert_eq!(next(), osc_packet("/part", &[OscArg::Int(0), OscArg::Int(1)]));
        ctl.enter(2);
        assert_eq!(next(), osc_packet("/part", &[OscArg::Int(1), OscArg::Int(0)]));
        ctl.fire(parse_lines([format!("osc 127.0.0.1:{port} /test").as_str()]).unwrap());
        assert_eq!(next(), osc_packet("/test", &[]));
    }
}
