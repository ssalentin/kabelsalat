//! Graph-driven multi-voice audio engine: the control-plane graph IS the mix.
//!
//! Each Clap node owns a voice; each Delay node owns a delay line. `render`
//! walks the graph edges: every voice renders, feeds the delays it is wired
//! to, and everything wired to the master is summed. Notes fan out to every
//! instrument that has a Keys→instrument MIDI cable. Unplug a cable and that
//! path goes silent — what you patch is what you hear.
//!
//! RT discipline: commands are fixed-size, graph storage is reserved, buffers
//! are preallocated, edge lookups are linear scans — no allocation in render.
//! Voices are created at startup (plugin loading is main-thread work); dynamic
//! AddNode(Clap) with off-thread loading is the follow-up.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crate::control::{Command, CommandDrain, DspKind, Graph, NodeId, NodeKind, ParamId, PortKind};
use crate::host::ClapVoice;

// Param ids (per node).
pub const P_LEVEL: ParamId = 0; // instrument output level
pub const P_TIME: ParamId = 1;  // delay time
pub const P_MIX: ParamId = 2;   // delay mix
pub const P_VOL: ParamId = 3;   // master volume
pub const P_WARM: ParamId = 4;  // master saturation

/// Preallocated stereo delay line. `process` is allocation-free.
pub struct DelayLine {
    buf: Vec<f32>,
    frames: usize,
    write: usize,
}

impl DelayLine {
    pub fn new(sample_rate: f64, max_secs: f64) -> Self {
        let frames = (sample_rate * max_secs) as usize;
        Self { buf: vec![0.0; frames * 2], frames, write: 0 }
    }

    /// out = dry*(1-mix) + delayed*mix. Feeding silence rings the tail out.
    pub fn process(&mut self, data: &mut [f32], time: f32, mix: f32) {
        let delay_frames =
            ((time.clamp(0.0, 1.0) as f64) * (self.frames as f64 - 2.0)) as usize;
        let mix = mix.clamp(0.0, 1.0);
        let n = data.len() / 2;
        for i in 0..n {
            let w = self.write;
            let r = (w + self.frames - delay_frames) % self.frames;
            let l_in = data[2 * i];
            let r_in = data[2 * i + 1];
            let l_d = self.buf[2 * r];
            let r_d = self.buf[2 * r + 1];
            self.buf[2 * w] = l_in;
            self.buf[2 * w + 1] = r_in;
            data[2 * i] = l_in * (1.0 - mix) + l_d * mix;
            data[2 * i + 1] = r_in * (1.0 - mix) + r_d * mix;
            self.write = (w + 1) % self.frames;
        }
    }
}

struct VoiceSlot {
    node: NodeId,
    voice: ClapVoice,
    level: f32,
    scratch: Vec<f32>,
}

struct DelaySlot {
    node: NodeId,
    delay: DelayLine,
    time: f32,
    mix: f32,
    buf: Vec<f32>,
}

pub struct GraphEngine {
    graph: Graph,
    drain: CommandDrain,
    voices: Vec<VoiceSlot>,
    delays: Vec<DelaySlot>,
    keys_node: Option<NodeId>,
    master_vol: f32,
    master_warm: f32,
    out_peak: Arc<AtomicU32>,
}

impl GraphEngine {
    /// `voices` pairs each Clap node id with a loaded voice (created on the
    /// main thread). Delay lines are built for every Dsp(Delay) node.
    pub fn new(
        graph: Graph,
        drain: CommandDrain,
        sample_rate: f64,
        voices: Vec<(NodeId, ClapVoice)>,
    ) -> Self {
        let keys_node = graph.nodes.iter().find(|n| matches!(n.kind, NodeKind::Keys)).map(|n| n.id);
        let voices = voices
            .into_iter()
            .map(|(node, voice)| VoiceSlot { node, voice, level: 0.85, scratch: vec![0.0; 8192 * 2] })
            .collect();
        let delays = graph
            .nodes
            .iter()
            .filter(|n| matches!(n.kind, NodeKind::Dsp(DspKind::Delay)))
            .map(|n| DelaySlot {
                node: n.id,
                delay: DelayLine::new(sample_rate, 2.0),
                time: 0.4,
                mix: 0.5,
                buf: vec![0.0; 8192 * 2],
            })
            .collect();
        Self {
            graph,
            drain,
            voices,
            delays,
            keys_node,
            master_vol: 0.8,
            master_warm: 0.15,
            out_peak: Arc::new(AtomicU32::new(0)),
        }
    }

    pub fn graph(&self) -> &Graph {
        &self.graph
    }

    pub fn level_handle(&self) -> Arc<AtomicU32> {
        self.out_peak.clone()
    }

    fn apply(&mut self, cmd: Command) {
        match cmd {
            Command::AddNode { kind, name } => {
                self.graph.add_node(kind, name);
            }
            Command::RemoveNode(id) => {
                self.graph.remove_node(id);
            }
            Command::Connect { id, from, to, kind } => {
                self.graph.connect_with_id(id, from, to, kind);
            }
            Command::Disconnect(id) => {
                self.graph.disconnect(id);
            }
            Command::SetParam { node, param, value } => {
                let v = value as f32;
                match param {
                    P_LEVEL => {
                        if let Some(s) = self.voices.iter_mut().find(|s| s.node == node) {
                            s.level = v;
                        }
                    }
                    P_TIME => {
                        if let Some(s) = self.delays.iter_mut().find(|s| s.node == node) {
                            s.time = v;
                        }
                    }
                    P_MIX => {
                        if let Some(s) = self.delays.iter_mut().find(|s| s.node == node) {
                            s.mix = v;
                        }
                    }
                    P_VOL => self.master_vol = v,
                    P_WARM => self.master_warm = v,
                    _ => {}
                }
            }
            Command::NoteOn { note, velocity } => {
                if let Some(keys) = self.keys_node {
                    for s in &mut self.voices {
                        if self.graph.has_route(keys, s.node, PortKind::Midi) {
                            s.voice.note_on(note as u16, velocity as f64);
                        }
                    }
                }
            }
            Command::NoteOff { note } => {
                if let Some(keys) = self.keys_node {
                    for s in &mut self.voices {
                        if self.graph.has_route(keys, s.node, PortKind::Midi) {
                            s.voice.note_off(note as u16);
                        }
                    }
                }
            }
            Command::TransportPlay | Command::TransportStop => {} // P2
        }
    }

    /// Render one device block, allocation-free.
    pub fn render(&mut self, data: &mut [f32], ch: usize) {
        while let Some(cmd) = self.drain.pop() {
            self.apply(cmd);
        }

        let frames = data.len() / ch;
        let cap = 8192;
        if frames > cap {
            for s in data.iter_mut() {
                *s = 0.0;
            }
            return;
        }
        let n2 = frames * 2;

        // 1. Every instrument renders and applies its level.
        for s in &mut self.voices {
            s.voice.render(&mut s.scratch[..n2]);
            for x in s.scratch[..n2].iter_mut() {
                *x *= s.level;
            }
        }

        // 2. Each delay sums the instruments wired into it, then processes.
        for d in &mut self.delays {
            for x in d.buf[..n2].iter_mut() {
                *x = 0.0;
            }
            for v in &self.voices {
                if self.graph.has_route(v.node, d.node, PortKind::Audio) {
                    for i in 0..n2 {
                        d.buf[i] += v.scratch[i];
                    }
                }
            }
            d.delay.process(&mut d.buf[..n2], d.time, d.mix);
        }

        // 3. Master sums everything wired to it.
        let mut peak = 0.0f32;
        let master = self.graph.nodes.iter().find(|n| matches!(n.kind, NodeKind::MasterOut)).map(|n| n.id);
        if let Some(master) = master {
            for (i, frame) in data.chunks_mut(ch).enumerate() {
                let mut l = 0.0f32;
                let mut r = 0.0f32;
                for v in &self.voices {
                    if self.graph.has_route(v.node, master, PortKind::Audio) {
                        l += v.scratch[i * 2];
                        r += v.scratch[i * 2 + 1];
                    }
                }
                for d in &self.delays {
                    if self.graph.has_route(d.node, master, PortKind::Audio) {
                        l += d.buf[i * 2];
                        r += d.buf[i * 2 + 1];
                    }
                }
                l = (l * (1.0 + self.master_warm * 3.0)).tanh() * self.master_vol;
                r = (r * (1.0 + self.master_warm * 3.0)).tanh() * self.master_vol;
                peak = peak.max(l.abs()).max(r.abs());
                frame[0] = l;
                if ch > 1 {
                    frame[1] = r;
                }
                for s in frame.iter_mut().skip(2) {
                    *s = 0.0;
                }
            }
        }
        self.out_peak.store(peak.to_bits(), Ordering::Relaxed);
    }
}
