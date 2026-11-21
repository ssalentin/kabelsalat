//! Retrospective recording (#215): the last minutes of the master and of each
//! armed rig's stem stay in a ring allocated once, so a take can be saved
//! after it was played.
//!
//! The audio thread writes each block straight into the ring ([`Feed`]):
//! relaxed atomic stores into lanes allocated elsewhere, no allocation, no
//! lock, and no ring in between that could overflow. One position counter
//! spans every lane, so the master and the stems of a save line up by
//! construction. The recorder's writer thread owns the memory ([`Keeper`]):
//! it allocates the lanes, hands the audio thread each new set through a ring,
//! frees the set that comes back, and copies the last seconds out on a save.
//!
//! A save is read oldest frames first, one chunk at a time, and can race the
//! audio thread overwriting the oldest frames. The feed announces how far it
//! is about to write (`reserved`) before it writes, and each chunk's read
//! checks that announcement after it finished (a seqlock's check): a chunk
//! the feed reached is refused, never saved with a seam. Each chunk has the
//! lane's spare length plus the audio played since the save began, so only a
//! save stalled for that long fails.

use std::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use rtrb::{Consumer, Producer, RingBuffer};

use crate::control::MAX_RIGS;

/// Lane 0 holds the master; lane `1 + rig` holds that rig's post-fader stem.
pub const LANES: usize = 1 + MAX_RIGS;
const _: () = assert!(LANES <= 64, "one bit per lane in a block's written mask");
const MASTER: usize = 0;
const NOT_YET: u64 = u64::MAX;

/// Lengths the Settings page offers, in minutes (0 = off).
pub const MINUTES_CHOICES: &[u32] = &[0, 1, 2, 5, 10];

/// Every lane holds this much more than a save can take, so a copy of the
/// whole length has this long before the feed reaches its first frame.
const MARGIN_SECS: u64 = 2;

/// One target's history: interleaved stereo f32, as bits.
pub struct Lane {
    samples: Box<[AtomicU32]>,
    /// The first position that belongs to this lane's target: where the feed
    /// began writing it, or where the rack's rigs last changed.
    since: AtomicU64,
}

impl Lane {
    fn new(frames: u64) -> Arc<Lane> {
        let samples: Box<[AtomicU32]> = (0..frames as usize * 2).map(|_| AtomicU32::new(0)).collect();
        // The allocator may hand back pages the kernel has not mapped yet, and
        // the audio thread's first store to each would fault one in.
        for s in samples.iter().step_by(1024).chain(samples.last()) {
            s.store(0, Ordering::Relaxed);
        }
        Arc::new(Lane { samples, since: AtomicU64::new(NOT_YET) })
    }

    fn frames(&self) -> u64 {
        self.samples.len() as u64 / 2
    }

    fn write(&self, pos: u64, frames: impl Iterator<Item = (f32, f32)>) {
        let cap = self.samples.len() / 2;
        let mut slot = (pos % cap as u64) as usize;
        for (l, r) in frames {
            self.samples[slot * 2].store(l.to_bits(), Ordering::Relaxed);
            self.samples[slot * 2 + 1].store(r.to_bits(), Ordering::Relaxed);
            slot += 1;
            if slot == cap {
                slot = 0;
            }
        }
    }

    /// Frames `from..to` as interleaved stereo into `out`; frames before the
    /// lane began are silence.
    fn read_into(&self, from: u64, to: u64, out: &mut Vec<f32>) {
        out.clear();
        let cap = self.samples.len() / 2;
        let since = self.since.load(Ordering::Relaxed);
        let mut slot = (from % cap as u64) as usize;
        for p in from..to {
            if p < since {
                out.extend_from_slice(&[0.0, 0.0]);
            } else {
                out.push(f32::from_bits(self.samples[slot * 2].load(Ordering::Relaxed)));
                out.push(f32::from_bits(self.samples[slot * 2 + 1].load(Ordering::Relaxed)));
            }
            slot += 1;
            if slot == cap {
                slot = 0;
            }
        }
    }
}

type LaneSet = [Option<Arc<Lane>>; LANES];

fn no_lanes() -> LaneSet {
    std::array::from_fn(|_| None)
}

/// Where the feed stands, readable from any thread.
pub struct Clock {
    /// Frames written completely.
    written: AtomicU64,
    /// Frames the feed may be writing right now (the block in progress).
    reserved: AtomicU64,
    /// The position the master lane began at (`NOT_YET` while there is none).
    master_since: AtomicU64,
    /// Frames a save can hold (0 = off).
    keep: AtomicU64,
}

impl Clock {
    pub fn new() -> Self {
        Self {
            written: AtomicU64::new(0),
            reserved: AtomicU64::new(0),
            master_since: AtomicU64::new(NOT_YET),
            keep: AtomicU64::new(0),
        }
    }

    /// Frames a save would hold now.
    pub fn kept_frames(&self) -> u64 {
        let since = self.master_since.load(Ordering::Relaxed);
        let written = self.written.load(Ordering::Relaxed);
        if since == NOT_YET {
            return 0;
        }
        written.saturating_sub(since).min(self.keep.load(Ordering::Relaxed))
    }

    pub fn keep_frames(&self) -> u64 {
        self.keep.load(Ordering::Relaxed)
    }
}

/// Audio-thread side: writes every block into the current lanes.
pub struct Feed {
    lanes: LaneSet,
    rx: Consumer<LaneSet>,
    back: Producer<LaneSet>,
    clock: Arc<Clock>,
    pos: u64,
    open: bool,
    rigs_written: u64,
    swapped: bool,
}

/// Writer-thread side: owns the lanes' memory and copies saves out.
pub struct Keeper {
    set: LaneSet,
    tx: Producer<LaneSet>,
    back: Consumer<LaneSet>,
    clock: Arc<Clock>,
    sample_rate: u64,
    keep: u64,
}

/// What a save holds: its frames, and the lanes it reads them from (the
/// master first when asked for, then each armed rig with a lane, `Some(rig)`).
pub struct Window {
    /// The position of the first frame.
    pub start: u64,
    pub frames: u64,
    lanes: Vec<(Option<u8>, Arc<Lane>)>,
    clock: Arc<Clock>,
}

impl Window {
    /// The target of each lane, in reading order (None = the master).
    pub fn targets(&self) -> Vec<Option<u8>> {
        self.lanes.iter().map(|(t, _)| *t).collect()
    }

    /// Frames `from..to` of lane `i` as interleaved stereo into `out`; Err
    /// when the feed overwrote any of them while they were read.
    pub fn read(&self, i: usize, from: u64, to: u64, out: &mut Vec<f32>) -> Result<(), String> {
        let lane = &self.lanes[i].1;
        lane.read_into(from, to, out);
        fence(Ordering::Acquire);
        if self.clock.reserved.load(Ordering::Relaxed) > from + lane.frames() {
            return Err("the history moved on while it was being saved; save again".into());
        }
        Ok(())
    }
}

pub fn channel(sample_rate: u32) -> (Feed, Keeper, Arc<Clock>) {
    let clock = Arc::new(Clock::new());
    let (tx, rx) = RingBuffer::new(8);
    let (back_tx, back) = RingBuffer::new(8);
    let feed = Feed {
        lanes: no_lanes(),
        rx,
        back: back_tx,
        clock: clock.clone(),
        pos: 0,
        open: false,
        rigs_written: 0,
        swapped: false,
    };
    let keeper = Keeper {
        set: no_lanes(),
        tx,
        back,
        clock: clock.clone(),
        sample_rate: sample_rate.max(1) as u64,
        keep: 0,
    };
    (feed, keeper, clock)
}

impl Feed {
    /// Adopt a new lane set (only at a block boundary) and announce the block.
    fn open(&mut self, frames: u64) {
        if self.open {
            return;
        }
        if self.back.slots() > 0 {
            if let Ok(set) = self.rx.pop() {
                let old = std::mem::replace(&mut self.lanes, set);
                let _ = self.back.push(old);
                for lane in self.lanes.iter().flatten() {
                    let _ = lane.since.compare_exchange(NOT_YET, self.pos, Ordering::Relaxed, Ordering::Relaxed);
                }
                let since = self.lanes[MASTER].as_ref().map_or(NOT_YET, |l| l.since.load(Ordering::Relaxed));
                self.clock.master_since.store(since, Ordering::Relaxed);
                self.swapped = true;
            }
        }
        self.clock.reserved.store(self.pos + frames, Ordering::Relaxed);
        fence(Ordering::Release);
        self.rigs_written = 0;
        self.open = true;
    }

    /// Rig `rig`'s post-fader sum for this block (interleaved stereo).
    pub fn rig(&mut self, rig: u8, stereo: &[f32]) {
        self.open((stereo.len() / 2) as u64);
        let i = 1 + (rig as usize).min(MAX_RIGS - 1);
        if let Some(lane) = &self.lanes[i] {
            lane.write(self.pos, stereo.chunks_exact(2).map(|f| (f[0], f[1])));
            self.rigs_written |= 1 << i;
        }
    }

    /// The block's final output; rigs that sent nothing this block get
    /// silence, so no lane keeps a frame from its previous lap.
    pub fn end_block(&mut self, data: &[f32], ch: usize) {
        let ch = ch.max(1);
        let frames = data.len() / ch;
        self.open(frames as u64);
        if let Some(lane) = &self.lanes[MASTER] {
            lane.write(self.pos, data.chunks_exact(ch).map(|f| (f[0], if ch > 1 { f[1] } else { f[0] })));
        }
        for i in 1..LANES {
            if self.rigs_written & (1 << i) == 0 {
                if let Some(lane) = &self.lanes[i] {
                    lane.write(self.pos, std::iter::repeat_n((0.0, 0.0), frames));
                }
            }
        }
        self.pos += frames as u64;
        self.clock.written.store(self.pos, Ordering::Release);
        self.open = false;
    }

    /// Did the feed give a lane set back since the last call (the keeper
    /// should wake and free it)?
    pub fn take_swapped(&mut self) -> bool {
        std::mem::take(&mut self.swapped)
    }
}

impl Keeper {
    /// Free the lane sets the feed gave back.
    pub fn collect(&mut self) {
        while self.back.pop().is_ok() {}
    }

    /// Make the lanes keep `keep_secs` (0 = off) of the master and of each rig
    /// in `rigs` (bit r = rig r). A rig lane exists only while the rig is
    /// armed; a length change starts every lane over. The feed adopts the new
    /// set at its next block; a full ring leaves it for the next call.
    pub fn reconcile(&mut self, keep_secs: u32, rigs: u64) {
        let keep = keep_secs as u64 * self.sample_rate;
        let rigs = if keep == 0 { 0 } else { rigs };
        let have_rigs = (0..MAX_RIGS).filter(|&r| self.set[1 + r].is_some()).fold(0u64, |m, r| m | 1 << r);
        if keep == self.keep && rigs == have_rigs {
            return;
        }
        if self.tx.slots() == 0 {
            return;
        }
        let same_length = keep == self.keep && self.set[MASTER].is_some();
        let frames = keep + MARGIN_SECS * self.sample_rate;
        let mut next = no_lanes();
        if keep > 0 {
            let lane = |old: &Option<Arc<Lane>>| match old {
                Some(l) if same_length => l.clone(),
                _ => Lane::new(frames),
            };
            next[MASTER] = Some(lane(&self.set[MASTER]));
            for r in 0..MAX_RIGS {
                if rigs & (1 << r) != 0 {
                    next[1 + r] = Some(lane(&self.set[1 + r]));
                }
            }
        }
        if !same_length {
            // Nothing is kept until the feed adopts the new master lane.
            self.clock.master_since.store(NOT_YET, Ordering::Relaxed);
        }
        self.clock.keep.store(keep, Ordering::Relaxed);
        let _ = self.tx.push(next.clone());
        self.set = next;
        self.keep = keep;
    }

    /// The last `want` frames at most, of the master (when `master`) and of
    /// each rig in `rigs` that has a lane the feed has begun.
    pub fn window(&self, want: u64, master: bool, rigs: u64) -> Result<Window, String> {
        let lane0 = self.set[MASTER].as_ref().ok_or("retrospective recording is off")?;
        let end = self.clock.written.load(Ordering::Acquire);
        let since = lane0.since.load(Ordering::Relaxed);
        if since == NOT_YET || since >= end {
            return Err("nothing has been played since retrospective recording started".into());
        }
        let frames = want.min(self.keep).min(end - since);
        let mut lanes = Vec::new();
        if master {
            lanes.push((None, lane0.clone()));
        }
        for r in (0..MAX_RIGS).filter(|&r| rigs & (1 << r) != 0) {
            if let Some(l) = self.set[1 + r].as_ref().filter(|l| l.since.load(Ordering::Relaxed) < end) {
                lanes.push((Some(r as u8), l.clone()));
            }
        }
        if lanes.is_empty() {
            return Err(if rigs == 0 {
                "nothing is armed: arm the master or a rig".into()
            } else {
                "the armed rigs were armed a moment ago and have nothing kept yet".into()
            });
        }
        Ok(Window { start: end - frames, frames, lanes, clock: self.clock.clone() })
    }
}

impl Feed {
    /// The rack's rigs changed (a load, a rig added or deleted): from here on
    /// a rig slot may hold another instrument, so each stem starts over.
    pub fn restart_stems(&self) {
        for lane in self.lanes[1..].iter().flatten() {
            lane.since.store(self.pos, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::tests::rt_alloc;

    const SR: u32 = 1000;

    /// A block whose every sample names its frame and channel.
    fn block(pos: u64, frames: usize, tag: f32) -> Vec<f32> {
        (0..frames).flat_map(|i| {
            let f = (pos + i as u64) as f32;
            [tag + f, -(tag + f)]
        }).collect()
    }

    fn render(feed: &mut Feed, blocks: usize, frames: usize, rig: Option<u8>) -> Vec<f32> {
        let mut heard = Vec::new();
        for _ in 0..blocks {
            let pos = feed.pos;
            if let Some(r) = rig {
                feed.rig(r, &block(pos, frames, 0.5));
            }
            let out = block(pos, frames, 0.0);
            feed.end_block(&out, 2);
            heard.extend_from_slice(&out);
        }
        heard
    }

    /// Every lane of a save, read whole, in the window's order.
    fn save(keeper: &Keeper, want: u64, master: bool, rigs: u64) -> Result<(Window, Vec<Vec<f32>>), String> {
        let w = keeper.window(want, master, rigs)?;
        let mut lanes = Vec::new();
        for i in 0..w.targets().len() {
            let mut out = Vec::new();
            w.read(i, w.start, w.start + w.frames, &mut out)?;
            lanes.push(out);
        }
        Ok((w, lanes))
    }

    #[test]
    fn a_save_holds_the_last_frames_the_feed_wrote_across_the_wrap() {
        let (mut feed, mut keeper, clock) = channel(SR);
        keeper.reconcile(3, 0);
        let heard = render(&mut feed, 100, 64, None);
        assert_eq!(clock.kept_frames(), 3 * SR as u64, "a lane full of history keeps its length");
        let (w, lanes) = save(&keeper, 2 * SR as u64, true, 0).unwrap();
        assert_eq!((w.frames, w.targets()), (2 * SR as u64, vec![None]));
        assert_eq!(lanes[0], heard[heard.len() - 2 * SR as usize * 2..], "the save is the last 2 s, sample for sample");
    }

    #[test]
    fn a_save_never_reaches_back_before_the_history_began() {
        let (mut feed, mut keeper, clock) = channel(SR);
        render(&mut feed, 10, 64, None);
        assert!(keeper.window(100, true, 0).is_err(), "off saves nothing");
        keeper.reconcile(3, 0);
        let heard = render(&mut feed, 5, 64, None);
        assert_eq!(clock.kept_frames(), 5 * 64);
        let (_, lanes) = save(&keeper, 3 * SR as u64, true, 0).unwrap();
        assert_eq!(lanes[0], heard, "only what was played since it started");
    }

    #[test]
    fn a_new_length_keeps_nothing_until_the_feed_has_begun_it() {
        let (mut feed, mut keeper, clock) = channel(SR);
        keeper.reconcile(1, 0);
        render(&mut feed, 40, 64, None);
        assert_eq!(clock.kept_frames(), SR as u64);
        keeper.reconcile(2, 0);
        assert_eq!(clock.kept_frames(), 0, "the old lane's history is gone, the new one has none yet");
        render(&mut feed, 1, 64, None);
        assert_eq!(clock.kept_frames(), 64);
    }

    #[test]
    fn a_stem_lines_up_with_the_master_and_is_silent_before_its_rig_was_armed() {
        let (mut feed, mut keeper, _) = channel(SR);
        keeper.reconcile(3, 0);
        render(&mut feed, 4, 64, Some(2));
        keeper.reconcile(3, 1 << 2);
        render(&mut feed, 4, 64, Some(2));
        let (w, lanes) = save(&keeper, 8 * 64, true, 1 << 2).unwrap();
        assert_eq!(w.targets(), vec![None, Some(2)]);
        assert_eq!(lanes[1].len(), lanes[0].len());
        assert!(lanes[1][..4 * 64 * 2].iter().all(|&s| s == 0.0), "before the arm the stem is silence");
        assert_eq!(lanes[1][4 * 64 * 2..], block(w.start + 4 * 64, 4 * 64, 0.5)[..], "after it, the rig's own frames at the master's positions");
    }

    #[test]
    fn the_last_rig_has_a_lane_of_its_own() {
        let last = (MAX_RIGS - 1) as u8;
        let (mut feed, mut keeper, _) = channel(SR);
        keeper.reconcile(3, 1 << last | 1 << 7);
        render(&mut feed, 4, 64, Some(last));
        let (w, lanes) = save(&keeper, 4 * 64, false, 1 << last | 1 << 7).unwrap();
        assert_eq!(w.targets(), vec![Some(7), Some(last)]);
        assert!(lanes[0].iter().all(|&s| s == 0.0), "rig 8 is not the last rig");
        assert_eq!(lanes[1], block(w.start, 4 * 64, 0.5));
    }

    #[test]
    fn a_stem_starts_over_when_the_rack_changes() {
        let (mut feed, mut keeper, _) = channel(SR);
        keeper.reconcile(3, 1);
        render(&mut feed, 4, 64, Some(0));
        feed.restart_stems();
        render(&mut feed, 2, 64, Some(0));
        let (w, lanes) = save(&keeper, 6 * 64, true, 1).unwrap();
        assert!(lanes[1][..4 * 64 * 2].iter().all(|&s| s == 0.0), "what rig slot 1 held before the load is not its stem");
        assert_eq!(lanes[1][4 * 64 * 2..], block(w.start + 4 * 64, 2 * 64, 0.5)[..]);
        assert_eq!(lanes[0].len(), 6 * 64 * 2, "the master keeps everything");
    }

    #[test]
    fn a_rig_armed_a_moment_ago_is_named_not_called_unarmed() {
        let (mut feed, mut keeper, _) = channel(SR);
        keeper.reconcile(3, 0);
        render(&mut feed, 4, 64, None);
        keeper.reconcile(3, 1);
        let e = keeper.window(SR as u64, false, 1).err().unwrap();
        assert!(e.contains("armed a moment ago"), "{e}");
        assert!(keeper.window(SR as u64, false, 0).err().unwrap().contains("nothing is armed"));
    }

    #[test]
    fn a_rig_that_sends_nothing_writes_silence_not_its_previous_lap() {
        let (mut feed, mut keeper, _) = channel(SR);
        keeper.reconcile(1, 1);
        render(&mut feed, 60, 64, Some(0));
        render(&mut feed, 60, 64, None);
        let (_, lanes) = save(&keeper, SR as u64, false, 1).unwrap();
        assert!(lanes[0].iter().all(|&s| s == 0.0));
    }

    #[test]
    fn a_chunk_the_feed_reached_is_refused_and_one_frame_short_of_it_is_saved() {
        let (mut feed, mut keeper, _) = channel(SR);
        keeper.reconcile(1, 0);
        render(&mut feed, 100, 64, None);
        let w = keeper.window(SR as u64, true, 0).unwrap();
        let cap = (1 + MARGIN_SECS) * SR as u64;
        let spare = w.start + cap - feed.pos;
        let mut out = Vec::new();
        render(&mut feed, 1, spare as usize, None);
        assert!(w.read(0, w.start, w.start + 1, &mut out).is_ok(), "the feed wrote up to the frame before the oldest");
        assert_eq!(out, block(w.start, 1, 0.0));
        feed.open(1);
        assert!(w.read(0, w.start, w.start + 1, &mut out).is_err(), "the next block reaches it");
        assert!(w.read(0, w.start + 1, w.start + SR as u64, &mut out).is_ok(), "later chunks are still ahead of the feed");
    }

    #[test]
    fn the_feed_neither_allocates_nor_frees_even_when_it_swaps_lanes() {
        let (mut feed, mut keeper, _) = channel(SR);
        keeper.reconcile(2, 0b101);
        let out = block(0, 256, 0.0);
        let stem = block(0, 256, 0.5);
        let (allocs, frees) = (rt_alloc::count(), rt_alloc::frees());
        for _ in 0..50 {
            feed.rig(0, &stem);
            feed.rig(2, &stem);
            feed.end_block(&out, 2);
        }
        assert_eq!((rt_alloc::count() - allocs, rt_alloc::frees() - frees), (0, 0));
        assert!(feed.take_swapped(), "the feed adopted the set while it was measured");
        keeper.reconcile(1, 0b1);
        let (allocs, frees) = (rt_alloc::count(), rt_alloc::frees());
        feed.rig(0, &stem);
        feed.end_block(&out, 2);
        assert_eq!((rt_alloc::count() - allocs, rt_alloc::frees() - frees), (0, 0), "a swap hands the old lanes back");
        let before = rt_alloc::frees();
        keeper.collect();
        assert!(rt_alloc::frees() > before, "the keeper frees them");
    }

    #[test]
    fn a_full_return_ring_leaves_the_new_set_waiting_instead_of_freeing_on_the_feed() {
        let (mut feed, mut keeper, _) = channel(SR);
        let out = block(0, 64, 0.0);
        for i in 0..9u32 {
            keeper.reconcile(1 + i % 2, 0);
            feed.end_block(&out, 2);
        }
        assert_eq!(feed.back.slots(), 0, "eight sets went back and none was collected");
        keeper.reconcile(1, 1);
        let held = feed.lanes[1].is_none();
        let frees = rt_alloc::frees();
        feed.end_block(&out, 2);
        assert_eq!(rt_alloc::frees(), frees);
        assert_eq!(feed.lanes[1].is_none(), held, "the feed kept its set");
        keeper.collect();
        feed.end_block(&out, 2);
        assert!(feed.lanes[MASTER].is_some(), "it adopts once the keeper made room");
    }
}
