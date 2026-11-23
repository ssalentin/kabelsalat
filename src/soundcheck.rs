//! Virtual Soundcheck (#400): a recorded gig take plays through the live
//! rack in place of the stage. Each Audio In node hears its recorded raw
//! input instead of its device, the recorded MIDI plays the instruments, and
//! the recorded song/part switches step the setlist, so every knob, fader
//! and preset change is heard through the exact live signal path while
//! nobody is on stage. The player jumps to any marker and loops a section.
//!
//! Threads: a reader thread ([`start`]) streams the take from disk into
//! fixed-size [`Chunk`]s taken from a pool the engine owns, and the engine
//! ([`Feed`], on the audio thread) consumes them a block at a time and hands
//! every chunk back. Nothing is allocated or freed on the audio thread: the
//! pool is allocated once ([`channel`]) and only circulates. A seek, a loop
//! change or a stop starts a new reader generation; the engine drops chunks
//! of an older one back into the pool, and releases what was held.
//!
//! Timing: an event recorded at take frame F (the first frame of the block
//! it was applied in) is applied in the replay block that contains F. With
//! the block size the take was recorded at and a start at a block boundary
//! (frame 0, or a marker, which is a block start too), every event lands in
//! the same block as live, and the rack's response is the same render.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

use rtrb::{Consumer, Producer, RingBuffer};
use serde::Serialize;
use serde_json::{json, Value};

use crate::matrix::SourceId;
use crate::record::{MAX_TAKE_INPUTS, NO_INPUT};
use crate::take::{Take, TakeEv, Timed, WavStream};

/// Frames per chunk.
pub const CHUNK: usize = 1024;
/// Input lanes a chunk carries, one per recorded Audio In.
pub const LANES: usize = MAX_TAKE_INPUTS;
/// Events a chunk carries at most; the reader ends a chunk early when more
/// fall into its frames.
pub const CHUNK_EVENTS: usize = 64;
/// Events the engine applies in one block at most (the rest the next block).
pub const BLOCK_EVENTS: usize = 512;
/// Chunks in circulation: about 0.5 s at 48 kHz ahead of the engine.
const POOL: usize = 24;
/// Largest engine block (frames), as `GraphEngine::render`'s cap.
const MAX_BLOCK: usize = 8192;

/// The live app's soundcheck, published once at startup (like
/// `record::LIVE_RECORDER`), so tools find it without an Arc per signature.
pub static LIVE: OnceLock<Arc<SoundcheckShared>> = OnceLock::new();

/// One piece of the take: frames `pos..pos + frames` of every input lane and
/// the events that fall into them.
pub struct Chunk {
    gen: u64,
    pos: u64,
    frames: usize,
    /// Lanes with a file (others are silent).
    lanes: usize,
    data: [f32; CHUNK * 2 * LANES],
    n_events: usize,
    events: [Option<Timed>; CHUNK_EVENTS],
}

impl Chunk {
    fn boxed() -> Box<Chunk> {
        Box::new(Chunk { gen: 0, pos: 0, frames: 0, lanes: 0, data: [0.0; CHUNK * 2 * LANES], n_events: 0, events: [None; CHUNK_EVENTS] })
    }
}

/// The reader's ends of the rings, and the chunks it held when it stopped.
struct ReaderEnds {
    chunk_tx: Producer<Box<Chunk>>,
    pool_rx: Consumer<Box<Chunk>>,
    spares: Vec<Box<Chunk>>,
}

/// What a frontend reads and sets. Atomics cross to the audio thread; the
/// mutexes are never touched there.
pub struct SoundcheckShared {
    sample_rate: u32,
    ends: Mutex<Option<ReaderEnds>>,
    /// Bumped by every start, seek, loop change and stop.
    gen: AtomicU64,
    /// The engine plays the feed of generation `gen`.
    active: AtomicBool,
    /// The reader read the take's (or the loop's) last frame and stopped.
    finished: AtomicBool,
    /// The engine played everything a finished reader read.
    ended: AtomicBool,
    /// The Audio In node each lane was recorded from.
    lane_nodes: [AtomicU32; LANES],
    /// Where the engine is, in take frames.
    pos: AtomicU64,
    /// Blocks the engine wanted take audio the reader had not read yet.
    underruns: AtomicU64,
    /// Chunks read ahead: pushed by the reader, not yet taken by the engine.
    queued: AtomicU64,
    stop: Arc<AtomicBool>,
    reader: Mutex<Option<JoinHandle<()>>>,
    state: Mutex<State>,
}

#[derive(Clone, Debug, Default)]
struct State {
    take: Option<Take>,
    loop_range: Option<(u64, u64)>,
    error: Option<String>,
    /// MIDI hardware ports now, by scan index (to map the take's devices).
    ports: Vec<String>,
}

/// Engine side: owned by `GraphEngine`, used on the audio thread only.
pub struct Feed {
    shared: Arc<SoundcheckShared>,
    chunk_rx: Consumer<Box<Chunk>>,
    pool_tx: Producer<Box<Chunk>>,
    cur: Option<Box<Chunk>>,
    off: usize,
    ev_at: usize,
    gen: u64,
    expect: u64,
    started: bool,
    playing: bool,
    /// The recorded node of each lane, as the generation began.
    pub lane_nodes: [u32; LANES],
    /// The live Audio In node each lane feeds (`NO_INPUT`: none), set by the
    /// engine from `lane_nodes` and its own Audio In list.
    pub lane_dest: [u32; LANES],
    lanes: usize,
    /// This block's input, lane after lane, `MAX_BLOCK * 2` samples each.
    inbuf: Vec<f32>,
    /// This block's events, applied by the engine before it renders.
    pub events: [Option<Timed>; BLOCK_EVENTS],
    pub n_events: usize,
}

/// What `Feed::begin_block` found.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BlockInfo {
    /// The feed plays this block.
    pub playing: bool,
    /// The take jumped (start, seek, loop wrap, stop, end): release every
    /// note the old position held.
    pub release: bool,
    /// A new generation began: map the lanes to the rack's Audio In nodes.
    pub remap: bool,
}

/// The pool, the rings and the shared state; allocate it off the audio
/// thread, before the engine starts rendering.
pub fn channel(sample_rate: u32) -> (Feed, Arc<SoundcheckShared>) {
    let (chunk_tx, chunk_rx) = RingBuffer::new(POOL);
    let (mut pool_tx, pool_rx) = RingBuffer::new(POOL);
    for _ in 0..POOL {
        let _ = pool_tx.push(Chunk::boxed());
    }
    let shared = Arc::new(SoundcheckShared {
        sample_rate,
        ends: Mutex::new(Some(ReaderEnds { chunk_tx, pool_rx, spares: Vec::new() })),
        gen: AtomicU64::new(0),
        active: AtomicBool::new(false),
        finished: AtomicBool::new(false),
        ended: AtomicBool::new(false),
        lane_nodes: std::array::from_fn(|_| AtomicU32::new(NO_INPUT)),
        pos: AtomicU64::new(0),
        underruns: AtomicU64::new(0),
        queued: AtomicU64::new(0),
        stop: Arc::new(AtomicBool::new(false)),
        reader: Mutex::new(None),
        state: Mutex::new(State::default()),
    });
    let feed = Feed {
        shared: shared.clone(),
        chunk_rx,
        pool_tx,
        cur: None,
        off: 0,
        ev_at: 0,
        gen: 0,
        expect: 0,
        started: false,
        playing: false,
        lane_nodes: [NO_INPUT; LANES],
        lane_dest: [NO_INPUT; LANES],
        lanes: 0,
        inbuf: vec![0.0; LANES * MAX_BLOCK * 2],
        events: [None; BLOCK_EVENTS],
        n_events: 0,
    };
    (feed, shared)
}

impl Feed {
    fn give_back(&mut self, c: Box<Chunk>) {
        // The pool ring holds every chunk there is: this never fails.
        let _ = self.pool_tx.push(c);
    }

    /// Take this block's input and events from the reader's chunks.
    /// Allocation- and lock-free.
    pub fn begin_block(&mut self, frames: usize) -> BlockInfo {
        let frames = frames.min(MAX_BLOCK);
        self.n_events = 0;
        let mut info = BlockInfo::default();
        let gen = self.shared.gen.load(Ordering::Acquire);
        if gen != self.gen {
            if let Some(c) = self.cur.take() {
                self.give_back(c);
            }
            self.gen = gen;
            self.started = false;
            info.release = self.playing;
            info.remap = true;
            for (n, a) in self.lane_nodes.iter_mut().zip(&self.shared.lane_nodes) {
                *n = a.load(Ordering::Acquire);
            }
        }
        let active = self.shared.active.load(Ordering::Acquire);
        if !active {
            // Every chunk left is of a stopped generation.
            while let Ok(c) = self.chunk_rx.pop() {
                self.shared.queued.fetch_sub(1, Ordering::AcqRel);
                self.give_back(c);
            }
            if let Some(c) = self.cur.take() {
                self.give_back(c);
            }
            info.release |= self.playing;
            self.playing = false;
            return info;
        }
        let mut filled = 0usize;
        let mut lanes = self.lanes;
        while filled < frames {
            if self.cur.is_none() {
                let popped = self.chunk_rx.pop();
                if popped.is_ok() {
                    self.shared.queued.fetch_sub(1, Ordering::AcqRel);
                }
                match popped {
                    Ok(c) if c.gen != self.gen => {
                        self.give_back(c);
                        continue;
                    }
                    Ok(c) => {
                        if self.started && c.pos != self.expect {
                            info.release = true; // a loop wrapped
                        }
                        self.started = true;
                        self.expect = c.pos;
                        lanes = c.lanes;
                        self.off = 0;
                        self.ev_at = 0;
                        self.cur = Some(c);
                    }
                    Err(_) => {
                        if self.shared.finished.load(Ordering::Acquire) {
                            if self.playing && !self.shared.ended.swap(true, Ordering::AcqRel) {
                                info.release = true;
                            }
                        } else if self.started {
                            self.shared.underruns.fetch_add(1, Ordering::Relaxed);
                        }
                        break;
                    }
                }
            }
            let Some(c) = self.cur.as_ref() else { break };
            let k = (frames - filled).min(c.frames - self.off);
            for lane in 0..c.lanes {
                let src = &c.data[lane * CHUNK * 2 + self.off * 2..lane * CHUNK * 2 + (self.off + k) * 2];
                self.inbuf[lane * MAX_BLOCK * 2 + filled * 2..lane * MAX_BLOCK * 2 + (filled + k) * 2].copy_from_slice(src);
            }
            let until = c.pos + (self.off + k) as u64;
            while self.ev_at < c.n_events && self.n_events < BLOCK_EVENTS {
                let Some(ev) = c.events[self.ev_at] else { break };
                if c.frames != 0 && ev.frame >= until {
                    break;
                }
                self.events[self.n_events] = Some(ev);
                self.n_events += 1;
                self.ev_at += 1;
            }
            self.off += k;
            filled += k;
            self.expect += k as u64;
            let done = self.off >= c.frames && self.ev_at >= c.n_events;
            if done {
                let c = self.cur.take().expect("checked above");
                self.give_back(c);
            } else if k == 0 {
                break; // more events than a block takes: the rest next block
            }
        }
        // What the reader has not delivered yet is silence, never stale input.
        for lane in 0..lanes {
            self.inbuf[lane * MAX_BLOCK * 2 + filled * 2..lane * MAX_BLOCK * 2 + frames * 2].fill(0.0);
        }
        self.lanes = lanes;
        self.playing = self.started;
        self.shared.pos.store(self.expect, Ordering::Release);
        info.playing = self.playing;
        info
    }

    /// The take's input for live Audio In node `node` this block, if a lane
    /// feeds it.
    pub fn input_for(&self, node: u32, frames: usize) -> Option<&[f32]> {
        if !self.playing {
            return None;
        }
        let lane = self.lane_dest[..self.lanes.min(LANES)].iter().position(|d| *d == node)?;
        Some(&self.inbuf[lane * MAX_BLOCK * 2..lane * MAX_BLOCK * 2 + frames.min(MAX_BLOCK) * 2])
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }
}

/// Map the lanes to the rack's Audio In nodes: a lane goes to the node it
/// was recorded from when that node is still an Audio In, else to the next
/// Audio In no lane took yet, in rack order (a rack rebuilt since the gig
/// renumbers its nodes, rule 44). Allocation-free.
pub fn map_lanes(recorded: &[u32; LANES], live: &[u32]) -> [u32; LANES] {
    let mut dest = [NO_INPUT; LANES];
    for (d, r) in dest.iter_mut().zip(recorded) {
        if *r != NO_INPUT && live.contains(r) {
            *d = *r;
        }
    }
    for i in 0..LANES {
        if recorded[i] == NO_INPUT || dest[i] != NO_INPUT {
            continue;
        }
        if let Some(n) = live.iter().find(|n| !dest.contains(n)) {
            dest[i] = *n;
        }
    }
    dest
}

/// Where and how a replay starts.
#[derive(Clone, Debug, Default)]
pub struct Play {
    pub from: u64,
    /// Loop these frames (start, end) instead of playing on.
    pub loop_range: Option<(u64, u64)>,
}

impl SoundcheckShared {
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// The MIDI hardware ports now, by scan index: a replay maps the take's
    /// devices onto them by name.
    pub fn set_ports(&self, ports: Vec<String>) {
        if let Ok(mut s) = self.state.lock() {
            s.ports = ports;
        }
    }

    fn stop_reader(&self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.reader.lock().ok().and_then(|mut r| r.take()) {
            let _ = h.join();
        }
        self.stop.store(false, Ordering::Release);
    }

    /// Play `take` from `play.from`. Replaces what played before.
    pub fn start(self: &Arc<Self>, take: Take, play: Play) -> Result<(), String> {
        self.stop_reader();
        self.active.store(false, Ordering::Release);
        let frames = take.manifest.frames;
        if frames == 0 {
            return Err(format!("{} holds no audio", take.folder.display()));
        }
        if take.manifest.sample_rate != self.sample_rate {
            return Err(format!(
                "the take runs at {} Hz and the engine at {} Hz: Virtual Soundcheck plays a take at its own rate",
                take.manifest.sample_rate, self.sample_rate
            ));
        }
        let loop_range = play.loop_range.map(|(a, b)| (a.min(frames), b.min(frames))).filter(|(a, b)| b > a);
        if play.loop_range.is_some() && loop_range.is_none() {
            return Err("the loop is empty".into());
        }
        let from = play.from.min(frames);
        let from = match loop_range {
            Some((a, b)) if from < a || from >= b => a,
            _ => from,
        };
        let mut streams = Vec::new();
        for i in take.manifest.inputs.iter().take(LANES) {
            streams.push(WavStream::open(&take.folder.join(&i.file))?);
        }
        let ports = self.state.lock().map(|s| s.ports.clone()).unwrap_or_default();
        let hw_map: Vec<Option<u32>> = take
            .manifest
            .midi_ports
            .iter()
            .map(|name| ports.iter().position(|p| p == name).map(|i| i as u32))
            .collect();
        let ends = self.ends.lock().map_err(|_| "soundcheck state poisoned".to_string())?.take();
        let Some(ends) = ends else { return Err("the soundcheck reader did not hand its rings back".into()) };
        for (lane, a) in self.lane_nodes.iter().enumerate() {
            let node = take.manifest.inputs.get(lane).map_or(NO_INPUT, |i| i.node);
            a.store(node, Ordering::Release);
        }
        self.finished.store(false, Ordering::Release);
        self.ended.store(false, Ordering::Release);
        self.underruns.store(0, Ordering::Relaxed);
        self.pos.store(from, Ordering::Release);
        let gen = self.gen.fetch_add(1, Ordering::AcqRel) + 1;
        let reader = Reader {
            gen,
            pos: from,
            end: frames,
            loop_range,
            streams,
            events: take.events.clone(),
            markers: take.manifest.markers.iter().map(|m| (m.frame, m.part)).collect(),
            hw_map,
            ends,
        };
        if let Ok(mut s) = self.state.lock() {
            s.take = Some(take);
            s.loop_range = loop_range;
            s.error = None;
        }
        let shared = self.clone();
        let stop = self.stop.clone();
        let handle = std::thread::Builder::new()
            .name("kabelsalat-soundcheck".into())
            .spawn(move || reader_loop(reader, &shared, &stop))
            .map_err(|e| e.to_string())?;
        if let Ok(mut r) = self.reader.lock() {
            *r = Some(handle);
        }
        Ok(())
    }

    /// Wait until the reader has read ahead (or read everything), then let
    /// the engine play. A live start does not wait: the engine pauses the
    /// take on an underrun instead of skipping it.
    pub fn wait_primed(&self, timeout: std::time::Duration) -> bool {
        let t0 = std::time::Instant::now();
        while !self.active.load(Ordering::Acquire) {
            if t0.elapsed() > timeout {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        true
    }

    /// Wait until the reader is `chunks` ahead or read everything: a test
    /// that renders faster than real time paces on it, so the take never
    /// underruns. The live engine never waits (an underrun pauses the take).
    pub fn wait_ahead(&self, chunks: u64, timeout: std::time::Duration) -> bool {
        let t0 = std::time::Instant::now();
        while self.queued.load(Ordering::Acquire) < chunks && !self.finished.load(Ordering::Acquire) {
            if t0.elapsed() > timeout {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
        true
    }

    /// The live inputs and MIDI come back; the take stops.
    pub fn stop(&self) {
        self.stop_reader();
        self.active.store(false, Ordering::Release);
        self.gen.fetch_add(1, Ordering::AcqRel);
        if let Ok(mut s) = self.state.lock() {
            s.take = None;
            s.loop_range = None;
        }
    }

    /// The take playing, and where.
    pub fn status(&self) -> Status {
        let s = self.state.lock().map(|s| s.clone()).unwrap_or_default();
        let pos = self.pos.load(Ordering::Acquire);
        let sr = self.sample_rate.max(1) as f64;
        let Some(take) = s.take else {
            return Status { error: s.error, ..Status::default() };
        };
        let marker = take.marker_at(pos);
        Status {
            active: true,
            playing: self.active.load(Ordering::Acquire) && !self.ended.load(Ordering::Acquire),
            ended: self.ended.load(Ordering::Acquire),
            take: Some(take.folder.to_string_lossy().into_owned()),
            seconds: pos as f64 / sr,
            length_seconds: take.seconds(),
            marker,
            marker_label: marker.map(|i| take.manifest.markers[i].label()),
            markers: take
                .manifest
                .markers
                .iter()
                .enumerate()
                .map(|(i, m)| MarkerStatus { index: i, label: m.label(), seconds: m.frame as f64 / sr, end_seconds: take.marker_end(i) as f64 / sr })
                .collect(),
            loop_seconds: s.loop_range.map(|(a, b)| (a as f64 / sr, b as f64 / sr)),
            inputs: take.manifest.inputs.iter().take(LANES).map(|i| i.name.clone()).collect(),
            midi_events: take.manifest.midi_events,
            underruns: self.underruns.load(Ordering::Relaxed),
            error: s.error,
        }
    }

    fn current(&self) -> Option<(Take, Option<(u64, u64)>)> {
        let s = self.state.lock().ok()?;
        Some((s.take.clone()?, s.loop_range))
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct MarkerStatus {
    pub index: usize,
    pub label: String,
    pub seconds: f64,
    pub end_seconds: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Status {
    /// Virtual Soundcheck is on: the inputs hear the take, not the stage.
    pub active: bool,
    pub playing: bool,
    /// The take played to its end (and the inputs are silent until a seek).
    pub ended: bool,
    pub take: Option<String>,
    pub seconds: f64,
    pub length_seconds: f64,
    pub marker: Option<usize>,
    pub marker_label: Option<String>,
    pub markers: Vec<MarkerStatus>,
    pub loop_seconds: Option<(f64, f64)>,
    pub inputs: Vec<String>,
    pub midi_events: u64,
    pub underruns: u64,
    pub error: Option<String>,
}

struct Reader {
    gen: u64,
    pos: u64,
    end: u64,
    loop_range: Option<(u64, u64)>,
    streams: Vec<WavStream>,
    events: Vec<Timed>,
    markers: Vec<(u64, Option<usize>)>,
    hw_map: Vec<Option<u32>>,
    ends: ReaderEnds,
}

impl Reader {
    /// The first event at or after `pos`, and the part marker in force there
    /// (applied first, so a jump lands in the right part).
    fn seek_events(&self, pos: u64) -> (usize, Option<Timed>) {
        let at = self.events.partition_point(|t| t.frame < pos);
        let part = self.markers.iter().rev().find(|(f, _)| *f <= pos).map(|(_, p)| Timed { frame: pos, ev: TakeEv::Part { part: *p } });
        (at, part)
    }

    fn map_source(&self, s: SourceId) -> SourceId {
        match s {
            SourceId::Hardware(i) => SourceId::Hardware(self.hw_map.get(i as usize).copied().flatten().unwrap_or(i)),
            other => other,
        }
    }

    fn map_ev(&self, ev: TakeEv) -> TakeEv {
        match ev {
            TakeEv::NoteOn { note, velocity, source, channel } => TakeEv::NoteOn { note, velocity, source: self.map_source(source), channel },
            TakeEv::NoteOff { note, source, channel } => TakeEv::NoteOff { note, source: self.map_source(source), channel },
            TakeEv::Cc { source, channel, cc, value } => TakeEv::Cc { source: self.map_source(source), channel, cc, value },
            TakeEv::PitchBend { source, channel, value } => TakeEv::PitchBend { source: self.map_source(source), channel, value },
            TakeEv::Pressure { source, channel, note, value } => TakeEv::Pressure { source: self.map_source(source), channel, note, value },
            TakeEv::Program { source, channel, program } => TakeEv::Program { source: self.map_source(source), channel, program },
            TakeEv::Part { part } => TakeEv::Part { part },
        }
    }
}

fn reader_loop(mut r: Reader, shared: &SoundcheckShared, stop: &AtomicBool) {
    let (mut ev_at, mut lead) = r.seek_events(r.pos);
    let mut tmp = Vec::new();
    let mut pushed = 0usize;
    let mut failed: Option<String> = None;
    while !stop.load(Ordering::Acquire) {
        let end = r.loop_range.map_or(r.end, |(_, b)| b);
        if r.pos >= end {
            match r.loop_range {
                Some((a, _)) => {
                    r.pos = a;
                    (ev_at, lead) = r.seek_events(a);
                    continue;
                }
                None => {
                    shared.finished.store(true, Ordering::Release);
                    shared.active.store(true, Ordering::Release);
                    break;
                }
            }
        }
        let chunk = r.ends.spares.pop().or_else(|| r.ends.pool_rx.pop().ok());
        let Some(mut c) = chunk else {
            if pushed > 0 {
                shared.active.store(true, Ordering::Release); // read ahead as far as the pool goes
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
            continue;
        };
        let mut to = (r.pos + CHUNK as u64).min(end);
        c.n_events = 0;
        if let Some(p) = lead.take() {
            c.events[0] = Some(p);
            c.n_events = 1;
        }
        while ev_at < r.events.len() && r.events[ev_at].frame < to {
            if c.n_events == CHUNK_EVENTS {
                // Full: the chunk ends at the next event's frame.
                to = r.events[ev_at].frame.max(r.pos);
                break;
            }
            let t = r.events[ev_at];
            c.events[c.n_events] = Some(Timed { frame: t.frame.max(r.pos), ev: r.map_ev(t.ev) });
            c.n_events += 1;
            ev_at += 1;
        }
        for e in c.events[c.n_events..].iter_mut() {
            *e = None;
        }
        let n = (to - r.pos) as usize;
        c.gen = r.gen;
        c.pos = r.pos;
        c.frames = n;
        c.lanes = r.streams.len();
        for (lane, s) in r.streams.iter_mut().enumerate() {
            if let Err(e) = s.read_stereo(r.pos, n, &mut tmp) {
                failed = Some(format!("reading the take failed: {e}"));
                tmp.clear();
                tmp.resize(n * 2, 0.0);
            }
            c.data[lane * CHUNK * 2..lane * CHUNK * 2 + n * 2].copy_from_slice(&tmp[..n * 2]);
        }
        r.pos = to;
        shared.queued.fetch_add(1, Ordering::AcqRel);
        if let Err(rtrb::PushError::Full(c)) = r.ends.chunk_tx.push(c) {
            shared.queued.fetch_sub(1, Ordering::AcqRel);
            r.ends.spares.push(c); // cannot happen: the ring holds the pool
        }
        pushed += 1;
        if pushed >= 4 {
            shared.active.store(true, Ordering::Release);
        }
        if failed.is_some() {
            break;
        }
    }
    if let Some(e) = failed {
        if let Ok(mut s) = shared.state.lock() {
            s.error = Some(e);
        }
        shared.finished.store(true, Ordering::Release);
    }
    if let Ok(mut e) = shared.ends.lock() {
        *e = Some(r.ends);
    }
}

// -- tools (assistant, MCP, UI alike) ------------------------------------

/// Every tool this module answers. They are on the assistant's table, in
/// `exec_tool` and in MCP (rules 6, 23, 66).
pub const TOOLS: [&str; 7] = [
    "list_takes",
    "soundcheck_start",
    "soundcheck_seek",
    "soundcheck_loop",
    "soundcheck_stop",
    "soundcheck_status",
    "export_take",
];

/// Tools that read and change nothing.
pub fn is_read_only(name: &str) -> bool {
    matches!(name, "list_takes" | "soundcheck_status")
}

/// The last export: its folder and files, or why it failed.
static EXPORT: Mutex<Option<Result<crate::take::Exported, String>>> = Mutex::new(None);
static EXPORTING: AtomicBool = AtomicBool::new(false);

fn take_folder(args: &Value, dir: &Path) -> Result<PathBuf, String> {
    match args.get("take").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty()) {
        Some(t) => {
            let p = PathBuf::from(t);
            Ok(if p.is_absolute() { p } else { dir.join(p) })
        }
        None => crate::take::list(dir)
            .into_iter()
            .next()
            .map(|t| PathBuf::from(t.folder))
            .ok_or_else(|| format!("no take under {}: record one first", dir.display())),
    }
}

fn frames_of(args: &Value, key: &str, sr: u32) -> Option<u64> {
    args.get(key).and_then(|v| v.as_f64()).filter(|s| s.is_finite() && *s >= 0.0).map(|s| (s * sr as f64).round() as u64)
}

/// A marker's section by index, or a seconds range.
fn range_of(args: &Value, take: &Take) -> Result<Option<(u64, u64)>, String> {
    let sr = take.manifest.sample_rate.max(1);
    if let Some(i) = args.get("marker").and_then(|v| v.as_u64()) {
        let i = i as usize;
        let m = take.manifest.markers.get(i).ok_or_else(|| {
            format!("the take has {} markers (0-based): {}", take.manifest.markers.len(),
                take.manifest.markers.iter().enumerate().map(|(i, m)| format!("{i} {}", m.label())).collect::<Vec<_>>().join(", "))
        })?;
        return Ok(Some((m.frame, take.marker_end(i))));
    }
    match (frames_of(args, "start_seconds", sr), frames_of(args, "end_seconds", sr)) {
        (Some(a), Some(b)) if b > a => Ok(Some((a, b))),
        (Some(_), Some(_)) => Err("end_seconds must come after start_seconds".into()),
        (None, None) => Ok(None),
        _ => Err("give both start_seconds and end_seconds, or a marker".into()),
    }
}

fn status_json(shared: &SoundcheckShared) -> Value {
    let mut v = serde_json::to_value(shared.status()).unwrap_or(Value::Null);
    if let Some(export) = EXPORT.lock().ok().and_then(|e| e.clone()) {
        v["last_export"] = match export {
            Ok(x) => serde_json::to_value(x).unwrap_or(Value::Null),
            Err(e) => json!({"error": e}),
        };
    }
    v["exporting"] = json!(EXPORTING.load(Ordering::Acquire));
    v
}

/// Answer one of [`TOOLS`]. `shared` is the engine's soundcheck (None: no
/// audio engine), `dir` where takes are recorded.
pub fn tool(name: &str, args: &Value, shared: Option<&Arc<SoundcheckShared>>, dir: &Path) -> Value {
    let r = (|| -> Result<Value, String> {
        match name {
            "list_takes" => Ok(json!({"folder": dir.to_string_lossy(), "takes": crate::take::list(dir)})),
            "export_take" => {
                let folder = take_folder(args, dir)?;
                let take = Take::open(&folder)?;
                let format = match args.get("format").and_then(|v| v.as_str()) {
                    Some(f) => crate::take::ExportFormat::from_id(f).ok_or_else(|| format!("format {f:?}: pcm24, pcm16 or float32"))?,
                    None => crate::take::ExportFormat::default(),
                };
                let stems = match args.get("stems") {
                    None | Some(Value::Null) => None,
                    Some(Value::Bool(true)) => None,
                    Some(Value::Bool(false)) => Some(Vec::new()),
                    Some(Value::Array(a)) => Some(a.iter().filter_map(|v| v.as_u64()).map(|r| r as u8).collect()),
                    Some(Value::String(t)) if t.trim().eq_ignore_ascii_case("all") => None,
                    Some(Value::String(t)) if t.trim().eq_ignore_ascii_case("none") || t.trim().is_empty() => Some(Vec::new()),
                    Some(Value::String(t)) => Some(
                        t.split(',')
                            .map(|r| r.trim().parse::<u8>().map_err(|_| format!("stems: \"all\", \"none\" or 0-based rigs like \"0,2\", not {t:?}")))
                            .collect::<Result<Vec<_>, _>>()?,
                    ),
                    Some(other) => return Err(format!("stems: \"all\", \"none\" or 0-based rigs like \"0,2\", not {other}")),
                };
                let spec = crate::take::ExportSpec {
                    mixdown: args.get("mixdown").and_then(|v| v.as_bool()).unwrap_or(true),
                    stems,
                    inputs: args.get("inputs").and_then(|v| v.as_bool()).unwrap_or(false),
                    range: range_of(args, &take)?,
                    format,
                    out: None,
                };
                if EXPORTING.swap(true, Ordering::AcqRel) {
                    return Err("an export is running: soundcheck_status shows when it is done".into());
                }
                let seconds = spec.range.map_or(take.seconds(), |(a, b)| (b - a) as f64 / take.manifest.sample_rate.max(1) as f64);
                std::thread::Builder::new()
                    .name("kabelsalat-export".into())
                    .spawn(move || {
                        let r = crate::take::export(&take, &spec);
                        if let Ok(mut e) = EXPORT.lock() {
                            *e = Some(r);
                        }
                        EXPORTING.store(false, Ordering::Release);
                    })
                    .map_err(|e| {
                        EXPORTING.store(false, Ordering::Release);
                        e.to_string()
                    })?;
                Ok(json!({"exporting": true, "take": folder.to_string_lossy(), "seconds": seconds,
                    "next": "soundcheck_status shows last_export (its folder and files) once it is written"}))
            }
            _ => {
                let shared = shared.ok_or("Virtual Soundcheck needs the audio engine")?;
                let sr = shared.sample_rate();
                match name {
                    "soundcheck_status" => Ok(status_json(shared)),
                    "soundcheck_stop" => {
                        shared.stop();
                        Ok(json!({"active": false, "note": "the inputs hear the stage again"}))
                    }
                    "soundcheck_start" => {
                        let take = Take::open(&take_folder(args, dir)?)?;
                        let section = range_of(args, &take)?;
                        let from = section.map(|(a, _)| a).or_else(|| frames_of(args, "seconds", sr)).unwrap_or(0);
                        let looped = args.get("loop").and_then(|v| v.as_bool()).unwrap_or(false);
                        let loop_range = if looped { Some(section.unwrap_or((0, take.manifest.frames))) } else { None };
                        shared.start(take, Play { from, loop_range })?;
                        Ok(status_json(shared))
                    }
                    "soundcheck_seek" => {
                        let (take, loop_range) = shared.current().ok_or("Virtual Soundcheck is off: soundcheck_start first")?;
                        let from = match range_of(args, &take)? {
                            Some((a, _)) => a,
                            None => frames_of(args, "seconds", sr).ok_or("give a marker or seconds")?,
                        };
                        shared.start(take, Play { from, loop_range })?;
                        Ok(status_json(shared))
                    }
                    "soundcheck_loop" => {
                        let (take, _) = shared.current().ok_or("Virtual Soundcheck is off: soundcheck_start first")?;
                        let off = args.get("off").and_then(|v| v.as_bool()).unwrap_or(false);
                        let loop_range = if off { None } else { Some(range_of(args, &take)?.ok_or("give a marker, start_seconds and end_seconds, or off=true")?) };
                        let from = shared.pos.load(Ordering::Acquire);
                        shared.start(take, Play { from, loop_range })?;
                        Ok(status_json(shared))
                    }
                    other => Err(format!("unknown soundcheck tool {other}")),
                }
            }
        }
    })();
    r.unwrap_or_else(|e| json!({"error": e}))
}

/// The assistant's tool table entries for [`TOOLS`].
pub fn tool_defs() -> Vec<Value> {
    let take = json!({"type":"string","description":"take folder (a name under ~/Music/kabelsalat or a full path); default the newest take"});
    let range = json!({
        "marker":{"type":"integer","description":"0-based song/part marker of the take (soundcheck_status lists them)"},
        "start_seconds":{"type":"number"},"end_seconds":{"type":"number"}
    });
    let with = |extra: Value| {
        let mut p = range.as_object().cloned().unwrap_or_default();
        if let Value::Object(e) = extra {
            p.extend(e);
        }
        Value::Object(p)
    };
    vec![
        json!({"type":"function","function":{"name":"list_takes","description":"List recorded gig takes, newest first: folder, seconds, stems, raw inputs, song/part markers and MIDI events of each.","parameters":{"type":"object","properties":{}}}}),
        json!({"type":"function","function":{"name":"soundcheck_start","description":"Virtual Soundcheck: play a recorded take through the live rack. Every Audio In hears its recorded raw input instead of the stage, the recorded MIDI plays the instruments and the recorded song/part switches step the setlist, so knob, fader and preset changes are heard at once. Start at a marker (its section) or at seconds; loop=true loops that section (or the whole take).","parameters":{"type":"object","properties":with(json!({"take":take,"seconds":{"type":"number"},"loop":{"type":"boolean"}}))}}}),
        json!({"type":"function","function":{"name":"soundcheck_seek","description":"Jump the playing take to a song/part marker or to seconds.","parameters":{"type":"object","properties":with(json!({"seconds":{"type":"number"}}))}}}),
        json!({"type":"function","function":{"name":"soundcheck_loop","description":"Loop a marker's section or a seconds range of the playing take; off=true plays on.","parameters":{"type":"object","properties":with(json!({"off":{"type":"boolean"}}))}}}),
        json!({"type":"function","function":{"name":"soundcheck_stop","description":"End Virtual Soundcheck: the inputs hear the stage again.","parameters":{"type":"object","properties":{}}}}),
        json!({"type":"function","function":{"name":"soundcheck_status","description":"Virtual Soundcheck state: take, position, current marker, markers with times, loop, inputs, underruns; also the last stem export.","parameters":{"type":"object","properties":{}}}}),
        json!({"type":"function","function":{"name":"export_take","description":"Export a take after the gig: the full show mixdown (mixdown, default true), rig stems (stems: all, none or 0-based rigs like \"0,2\") and raw inputs (inputs), of the whole take or a marker's section / seconds range, as 24-bit (default), 16-bit or float WAV into <take>/export. Runs in the background; soundcheck_status shows last_export.","parameters":{"type":"object","properties":with(json!({"take":take,"mixdown":{"type":"boolean"},"stems":{"type":"string","description":"all (default), none, or 0-based rigs like 0,2"},"inputs":{"type":"boolean"},"format":{"type":"string","enum":["pcm24","pcm16","float32"]}}))}}}),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lanes_go_to_their_own_node_then_to_free_ones_in_rack_order() {
        let mut rec = [NO_INPUT; LANES];
        rec[0] = 5;
        rec[1] = 9;
        rec[2] = 7;
        let d = map_lanes(&rec, &[3, 7, 4]);
        assert_eq!(&d[..4], &[3, 4, 7, NO_INPUT], "7 keeps its node, 5 and 9 take the free ones");
        let d = map_lanes(&rec, &[]);
        assert!(d.iter().all(|x| *x == NO_INPUT));
    }

    #[test]
    fn every_soundcheck_tool_is_on_the_tool_table() {
        let defs = tool_defs();
        for t in TOOLS {
            assert!(defs.iter().any(|d| d["function"]["name"] == t), "{t}");
        }
        assert_eq!(defs.len(), TOOLS.len());
    }

    #[test]
    fn tools_without_an_engine_say_so_and_list_takes_still_works() {
        let dir = std::env::temp_dir().join(format!("ksalat-sc-none-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let v = tool("soundcheck_start", &json!({}), None, &dir);
        assert!(v["error"].as_str().unwrap().contains("needs the audio engine"), "{v}");
        let v = tool("list_takes", &json!({}), None, &dir);
        assert!(v["takes"].as_array().unwrap().is_empty(), "{v}");
        let v = tool("export_take", &json!({}), None, &dir);
        assert!(v["error"].as_str().unwrap().contains("no take"), "{v}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
