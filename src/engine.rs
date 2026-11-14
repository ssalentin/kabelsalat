//! Graph-driven multi-voice audio engine: the control-plane graph IS the mix.
//!
//! Each Clap node owns a voice; each Dsp node owns a real built-in processor
//! (see `dsp.rs`). `render` walks the graph edges: every voice renders, feeds
//! the DSP nodes it is wired to, and everything wired to the master is summed.
//! Notes fan out to every instrument that has a Keys→instrument MIDI cable.
//! Unplug a cable and that path goes silent — what you patch is what you hear.
//!
//! RT discipline: commands are fixed-size, graph storage is reserved, buffers
//! are preallocated, edge lookups are linear scans, and every processor is
//! proven allocation-free — no allocation in render. Voices/processors are
//! created at startup (loading is main-thread work); dynamic AddNode with
//! off-thread loading is the follow-up.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crate::control::{Command, CommandDrain, DspKind, Graph, NodeId, NodeKind, ParamId, PortKind};
use crate::dsp::{build_processor, Processor};
use crate::host::ClapVoice;

// Engine-level param ids (what the UI/assistant send). Translated to each
// processor's native param ids in `dsp_native_param`.
pub const P_LEVEL: ParamId = 0; // instrument output level
pub const P_TIME: ParamId = 1;  // delay time
pub const P_MIX: ParamId = 2;   // delay / effect mix
pub const P_VOL: ParamId = 3;   // master volume
pub const P_WARM: ParamId = 4;  // master saturation

/// Translate an engine param id to a processor's native param id for a kind.
/// Returns None if the knob doesn't apply to this kind.
fn dsp_native_param(kind: &DspKind, p: ParamId) -> Option<u32> {
    match kind {
        // dsp::Delay native params: 0=time, 1=feedback, 2=mix, 4=damping.
        DspKind::Delay => match p {
            P_TIME => Some(0),
            P_MIX => Some(2),
            _ => None,
        },
        // Other kinds get their knob maps when the mixer UI exposes them.
        _ => None,
    }
}

struct VoiceSlot {
    node: NodeId,
    voice: ClapVoice,
    level: f32,
    scratch: Vec<f32>,
}

struct DspSlot {
    node: NodeId,
    kind: DspKind,
    proc: Box<dyn Processor>,
    buf: Vec<f32>,
}

pub struct GraphEngine {
    graph: Graph,
    drain: CommandDrain,
    voices: Vec<VoiceSlot>,
    dsp: Vec<DspSlot>,
    keys_node: Option<NodeId>,
    master_vol: f32,
    master_warm: f32,
    out_peak: Arc<AtomicU32>,
}

impl GraphEngine {
    /// `voices` pairs each Clap node id with a loaded voice (created on the
    /// main thread). A processor is built for every Dsp node in the graph.
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
        let dsp = graph
            .nodes
            .iter()
            .filter_map(|n| match &n.kind {
                NodeKind::Dsp(kind) => Some(DspSlot {
                    node: n.id,
                    kind: kind.clone(),
                    proc: build_processor(kind, sample_rate),
                    buf: vec![0.0; 8192 * 2],
                }),
                _ => None,
            })
            .collect();
        Self {
            graph,
            drain,
            voices,
            dsp,
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
                    P_VOL => self.master_vol = v,
                    P_WARM => self.master_warm = v,
                    _ => {
                        if let Some(s) = self.dsp.iter_mut().find(|s| s.node == node) {
                            if let Some(native) = dsp_native_param(&s.kind, param) {
                                s.proc.set_param(native, v);
                            }
                        }
                    }
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

        // 2. Each DSP node sums the instruments wired into it, then processes.
        for d in &mut self.dsp {
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
            d.proc.process(&mut d.buf[..n2], frames);
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
                for d in &self.dsp {
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
