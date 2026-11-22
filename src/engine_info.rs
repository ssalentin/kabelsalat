//! Engine load: what the DSP readout, the Engine Info panel and
//! the `get_engine_info` tool show. All three render one [`EngineStats`]
//! snapshot, built by [`EngineStats::build`] from the callback counters
//! ([`EngineCounters`], the bridge atomics) and the per-node render times
//! ([`NodeTimes`]).

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use crate::bridge;
use crate::control::NodeId;

/// Rows of the per-node table. A rack with more timed units keeps playing;
/// the surplus is counted in `EngineStats::nodes_not_timed`.
pub const NODE_TIME_SLOTS: usize = 64;

/// Render time per node, audio thread → frontends. Lock-free and fixed
/// size: the render path writes row `i` for the i-th unit it runs in a
/// block, so a row needs no search and no allocation. The order is stable
/// while the graph is; when it changes, the rows restart.
pub struct NodeTimes {
    /// `(node id << 32) | smoothed render time in ns` (EMA, 1/64).
    avg: [AtomicU64; NODE_TIME_SLOTS],
    max_ns: [AtomicU32; NODE_TIME_SLOTS],
    /// Units timed in the last block (can exceed the rows).
    count: AtomicU32,
    /// Bumped by a frontend; the audio thread then restarts every maximum.
    reset: AtomicU32,
}

/// One row as a frontend reads it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NodeTime {
    pub node: NodeId,
    pub avg_ns: u32,
    pub max_ns: u32,
}

impl NodeTimes {
    pub fn new() -> Self {
        Self {
            avg: std::array::from_fn(|_| AtomicU64::new(0)),
            max_ns: std::array::from_fn(|_| AtomicU32::new(0)),
            count: AtomicU32::new(0),
            reset: AtomicU32::new(0),
        }
    }

    /// The rows of the last block and how many units had no row.
    pub fn read(&self) -> (Vec<NodeTime>, u32) {
        let count = self.count.load(Ordering::Relaxed) as usize;
        let rows = (0..count.min(NODE_TIME_SLOTS))
            .map(|i| {
                let packed = self.avg[i].load(Ordering::Relaxed);
                NodeTime { node: (packed >> 32) as NodeId, avg_ns: packed as u32, max_ns: self.max_ns[i].load(Ordering::Relaxed) }
            })
            .collect();
        (rows, count.saturating_sub(NODE_TIME_SLOTS) as u32)
    }

    pub fn reset_peaks(&self) {
        self.reset.fetch_add(1, Ordering::Relaxed);
    }
}

impl Default for NodeTimes {
    fn default() -> Self {
        Self::new()
    }
}

/// The audio thread's writer for a [`NodeTimes`] table (one per engine).
pub struct NodeTimer {
    table: Arc<NodeTimes>,
    next: usize,
    seen_reset: u32,
    restart_max: bool,
}

impl NodeTimer {
    pub fn new() -> Self {
        Self { table: Arc::new(NodeTimes::new()), next: 0, seen_reset: 0, restart_max: false }
    }

    pub fn handle(&self) -> Arc<NodeTimes> {
        self.table.clone()
    }

    pub fn begin_block(&mut self) {
        self.next = 0;
        let reset = self.table.reset.load(Ordering::Relaxed);
        self.restart_max = reset != self.seen_reset;
        self.seen_reset = reset;
    }

    /// The next unit of this block took `elapsed`.
    pub fn record(&mut self, node: NodeId, elapsed: Duration) {
        let i = self.next;
        self.next += 1;
        if i >= NODE_TIME_SLOTS {
            return;
        }
        let ns = elapsed.as_nanos().min(u32::MAX as u128) as u32;
        let packed = self.table.avg[i].load(Ordering::Relaxed);
        let same_node = (packed >> 32) as NodeId == node && packed != 0;
        let avg = if same_node { ema_ns(packed as u32, ns) } else { ns };
        let max = if same_node && !self.restart_max { self.table.max_ns[i].load(Ordering::Relaxed).max(ns) } else { ns };
        self.table.max_ns[i].store(max, Ordering::Relaxed);
        self.table.avg[i].store(((node as u64) << 32) | avg as u64, Ordering::Relaxed);
    }

    pub fn end_block(&mut self) {
        self.table.count.store(self.next as u32, Ordering::Relaxed);
    }
}

impl Default for NodeTimer {
    fn default() -> Self {
        Self::new()
    }
}

/// One step of the 1/64 moving average, in nanoseconds. Signed and rounded
/// towards the sample, so it converges on a constant input from both sides.
pub fn ema_ns(avg: u32, sample: u32) -> u32 {
    let diff = sample as i64 - avg as i64;
    let step = if diff >= 0 { (diff + 63) / 64 } else { -((-diff + 63) / 64) };
    (avg as i64 + step) as u32
}

/// The audio callback's counters, read once.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EngineCounters {
    /// 0 = no output stream.
    pub sample_rate: u32,
    /// Frames of the last callback; 0 before the first one.
    pub callback_frames: u32,
    /// Configured target buffer size; 0 falls back to `bridge::configured_buffer_frames()`.
    pub configured_frames: u32,
    pub render_avg_ns: u32,
    pub render_max_us: u32,
    pub render_late: u32,
    pub xruns: u32,
    /// 0 = no hardware MIDI event measured yet.
    pub midi_latency_us: u32,
}

impl EngineCounters {
    pub fn load() -> Self {
        Self {
            sample_rate: bridge::SAMPLE_RATE.load(Ordering::Relaxed),
            callback_frames: bridge::RENDER_LAST_FRAMES.load(Ordering::Relaxed),
            configured_frames: bridge::configured_buffer_frames(),
            render_avg_ns: bridge::RENDER_AVG_NS.load(Ordering::Relaxed),
            render_max_us: bridge::RENDER_MAX_US.load(Ordering::Relaxed),
            render_late: bridge::RENDER_LATE.load(Ordering::Relaxed),
            xruns: bridge::XRUNS.load(Ordering::Relaxed),
            midi_latency_us: bridge::MIDI_LATENCY_US.load(Ordering::Relaxed),
        }
    }

    /// The buffer the device asks for per callback; the configured size
    /// until the first callback reports the real one.
    pub fn buffer_frames(&self) -> u32 {
        if self.callback_frames > 0 {
            self.callback_frames
        } else if self.configured_frames > 0 {
            self.configured_frames
        } else {
            bridge::configured_buffer_frames()
        }
    }

    /// Duration of one buffer in ms (0 without an output stream).
    pub fn buffer_ms(&self) -> f64 {
        if self.sample_rate == 0 { 0.0 } else { self.buffer_frames() as f64 / self.sample_rate as f64 * 1000.0 }
    }
}

/// Zero the worst render time, the late-block count and the xrun count.
pub fn reset_counter_peaks() {
    bridge::RENDER_MAX_US.store(0, Ordering::Relaxed);
    bridge::RENDER_LATE.store(0, Ordering::Relaxed);
    bridge::XRUNS.store(0, Ordering::Relaxed);
}

/// Reset timing and dropout counters after reopening the stream at a new buffer size.
pub fn reset_for_buffer_change() {
    bridge::RENDER_LAST_FRAMES.store(0, Ordering::Relaxed);
    bridge::RENDER_AVG_NS.store(0, Ordering::Relaxed);
    bridge::AUDIO_THREAD_RT.store(0, Ordering::Relaxed);
    reset_counter_peaks();
}

/// One unit's share of the render time.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NodeStat {
    pub node: NodeId,
    pub name: String,
    pub avg_us: f64,
    pub max_us: f64,
    /// `avg_us` as a share of one buffer.
    pub percent: f64,
}

/// The engine's load and audio session, as plain values.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct EngineStats {
    /// An output stream runs and has rendered at least one buffer.
    pub audio_live: bool,
    pub sample_rate: u32,
    pub buffer_frames: u32,
    pub buffer_ms: f64,
    /// Channels of the rack's Audio In units.
    pub inputs: u32,
    pub outputs: u32,
    /// Average render time as a share of one buffer.
    pub dsp_percent: f64,
    pub render_avg_us: f64,
    pub render_max_us: u32,
    /// Buffers that took more than 80 % of their time.
    pub late_blocks: u32,
    pub xruns: u32,
    /// Queue wait of hardware MIDI before the engine sees it; None until an
    /// event was measured.
    pub midi_input_latency_ms: Option<f64>,
    /// Units by render time, most expensive first.
    pub nodes: Vec<NodeStat>,
    pub nodes_not_timed: u32,
}

impl EngineStats {
    pub fn build(
        c: EngineCounters,
        times: Option<&NodeTimes>,
        inputs: u32,
        name_of: impl Fn(NodeId) -> Option<String>,
    ) -> Self {
        let buffer_ms = c.buffer_ms();
        let share = |us: f64| if buffer_ms > 0.0 { us / (buffer_ms * 1000.0) * 100.0 } else { 0.0 };
        let (rows, nodes_not_timed) = times.map(NodeTimes::read).unwrap_or_default();
        let mut nodes: Vec<NodeStat> = rows
            .into_iter()
            .map(|t| {
                let avg_us = t.avg_ns as f64 / 1000.0;
                NodeStat {
                    node: t.node,
                    name: name_of(t.node).unwrap_or_else(|| format!("Node {}", t.node)),
                    avg_us,
                    max_us: t.max_ns as f64 / 1000.0,
                    percent: share(avg_us),
                }
            })
            .collect();
        nodes.sort_by(|a, b| b.avg_us.total_cmp(&a.avg_us));
        let render_avg_us = c.render_avg_ns as f64 / 1000.0;
        Self {
            audio_live: c.sample_rate > 0 && c.callback_frames > 0,
            sample_rate: c.sample_rate,
            buffer_frames: c.buffer_frames(),
            buffer_ms,
            inputs,
            outputs: 2,
            dsp_percent: share(render_avg_us),
            render_avg_us,
            render_max_us: c.render_max_us,
            late_blocks: c.render_late,
            xruns: c.xruns,
            midi_input_latency_ms: (c.midi_latency_us > 0).then(|| c.midi_latency_us as f64 / 1000.0),
            nodes,
            nodes_not_timed,
        }
    }
}

/// How loaded the engine is, for the readout's colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadLevel {
    Normal,
    High,
    Critical,
}

impl LoadLevel {
    pub fn of(dsp_percent: f64) -> Self {
        if dsp_percent > 90.0 {
            LoadLevel::Critical
        } else if dsp_percent >= 70.0 {
            LoadLevel::High
        } else {
            LoadLevel::Normal
        }
    }
}

/// Per-unit CPU load band for the rack overlay (`F3`):
/// normal `<= 15 %`, warm/amber `> 15 %`, hot/red `> 30 %` of one buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnitLoadLevel {
    Normal,
    Warm,
    Hot,
}

impl UnitLoadLevel {
    pub fn of(percent: f64) -> Self {
        // Tiny epsilon absorbs IEEE-754 noise on exact rational buffer shares
        // (e.g. 800 µs of 256/48000 s = 15.000000000000002 %).
        if percent > 30.0 + 1e-9 {
            UnitLoadLevel::Hot
        } else if percent > 15.0 + 1e-9 {
            UnitLoadLevel::Warm
        } else {
            UnitLoadLevel::Normal
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counters(avg_us: u32) -> EngineCounters {
        EngineCounters { sample_rate: 48_000, callback_frames: 256, render_avg_ns: avg_us * 1000, ..Default::default() }
    }

    #[test]
    fn dsp_percent_is_the_average_render_time_over_one_buffer() {
        // 256 frames at 48 kHz = 5333.3 µs; 1173 µs of it is 22 %.
        let s = EngineStats::build(counters(1173), None, 0, |_| None);
        assert!((s.buffer_ms - 5.3333).abs() < 1e-3, "{}", s.buffer_ms);
        assert!((s.dsp_percent - 21.99).abs() < 0.01, "{}", s.dsp_percent);
        assert!(s.audio_live && s.outputs == 2 && s.inputs == 0);
        assert_eq!(s.midi_input_latency_ms, None, "no MIDI event measured yet");

        // The callback's real size sets the budget, not the configured one.
        let c = EngineCounters { callback_frames: 1024, ..counters(1173) };
        let s = EngineStats::build(c, None, 0, |_| None);
        assert_eq!(s.buffer_frames, 1024);
        assert!((s.dsp_percent - 5.498).abs() < 0.01, "{}", s.dsp_percent);
    }

    #[test]
    fn without_an_output_stream_nothing_is_reported_as_load() {
        let s = EngineStats::build(EngineCounters::default(), None, 0, |_| None);
        assert!(!s.audio_live);
        assert_eq!((s.dsp_percent, s.buffer_ms, s.buffer_frames), (0.0, 0.0, bridge::BUFFER_FRAMES));

        // Before the first callback reports its size, configured_frames is used.
        let s = EngineStats::build(EngineCounters { configured_frames: 512, ..Default::default() }, None, 0, |_| None);
        assert_eq!(s.buffer_frames, 512);
    }

    #[test]
    fn load_levels_split_at_70_and_90_percent() {
        assert_eq!(LoadLevel::of(69.9), LoadLevel::Normal);
        assert_eq!(LoadLevel::of(70.0), LoadLevel::High);
        assert_eq!(LoadLevel::of(90.0), LoadLevel::High);
        assert_eq!(LoadLevel::of(90.1), LoadLevel::Critical);
    }

    #[test]
    fn unit_load_levels_split_at_15_and_30_percent() {
        assert_eq!(UnitLoadLevel::of(0.0), UnitLoadLevel::Normal);
        assert_eq!(UnitLoadLevel::of(15.0), UnitLoadLevel::Normal);
        assert_eq!(UnitLoadLevel::of(15.01), UnitLoadLevel::Warm);
        assert_eq!(UnitLoadLevel::of(30.0), UnitLoadLevel::Warm);
        assert_eq!(UnitLoadLevel::of(30.01), UnitLoadLevel::Hot);
    }

    #[test]
    fn the_average_converges_on_small_and_large_times() {
        for target in [0u32, 7, 50, 100, 300, 5_000, 2_000_000] {
            for start in [0u32, 1_000_000] {
                let mut avg = start;
                for _ in 0..4000 { avg = ema_ns(avg, target); }
                assert_eq!(avg, target, "from {start}");
            }
        }
    }

    #[test]
    fn node_rows_are_named_sorted_and_shared_per_buffer() {
        let mut timer = NodeTimer::new();
        timer.begin_block();
        timer.record(4, Duration::from_micros(40));
        timer.record(9, Duration::from_micros(800));
        timer.end_block();
        let table = timer.handle();
        let s = EngineStats::build(counters(900), Some(&table), 2, |n| (n == 9).then(|| "Surge A".to_string()));
        let rows: Vec<(u32, &str, f64)> = s.nodes.iter().map(|n| (n.node, n.name.as_str(), n.avg_us)).collect();
        assert_eq!(rows, vec![(9, "Surge A", 800.0), (4, "Node 4", 40.0)], "most expensive first");
        assert!((s.nodes[0].percent - 15.0).abs() < 1e-6, "800 of 5333 µs: {}", s.nodes[0].percent);
        assert_eq!(s.inputs, 2);
    }

    #[test]
    fn node_times_follow_the_block_and_restart_when_the_order_changes() {
        let mut timer = NodeTimer::new();
        let table = timer.handle();
        for us in [100u64, 300, 100] {
            timer.begin_block();
            timer.record(1, Duration::from_micros(us));
            timer.end_block();
        }
        let (rows, extra) = table.read();
        assert_eq!((rows.len(), extra), (1, 0));
        assert_eq!(rows[0].max_ns, 300_000, "the worst block is kept");
        assert!(rows[0].avg_ns > 100_000 && rows[0].avg_ns < 110_000, "smoothed: {}", rows[0].avg_ns);

        table.reset_peaks();
        timer.begin_block();
        timer.record(1, Duration::from_micros(120));
        timer.end_block();
        assert_eq!(table.read().0[0].max_ns, 120_000, "a reset restarts the maximum");

        // Another node takes the row: its numbers start from its own sample.
        timer.begin_block();
        timer.record(2, Duration::from_micros(10));
        timer.end_block();
        assert_eq!(table.read().0, vec![NodeTime { node: 2, avg_ns: 10_000, max_ns: 10_000 }]);

        // More units than rows: the surplus is counted, never written.
        timer.begin_block();
        for n in 0..NODE_TIME_SLOTS as u32 + 3 { timer.record(n, Duration::from_micros(1)); }
        timer.end_block();
        let (rows, extra) = table.read();
        assert_eq!((rows.len(), extra), (NODE_TIME_SLOTS, 3));
    }
}
