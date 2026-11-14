//! Graph-driven audio engine: the control-plane graph IS the audio path.
//!
//! `GraphEngine` owns the Surge voice, the delay line, and the patch. It
//! drains control-plane commands at block boundaries (fixed-size commands,
//! reserved graph storage — no allocation in the callback) and renders
//! strictly according to what is plugged. Unplug a cable and the signal stops.

use crate::control::{Command, CommandDrain, DspKind, Graph, NodeKind, ParamId, PortKind};
use crate::host::ClapVoice;

// Param ids for the fixed rack (task-10 generalizes to per-node param tables).
pub const P_LEVEL: ParamId = 0; // Surge output level
pub const P_TIME: ParamId = 1;  // Delay time
pub const P_MIX: ParamId = 2;   // Delay mix
pub const P_VOL: ParamId = 3;   // Master volume
pub const P_WARM: ParamId = 4;  // Master saturation

/// Preallocated stereo delay line. `process` is allocation-free.
pub struct DelayLine {
    buf: Vec<f32>, // stereo interleaved
    frames: usize,
    write: usize,
}

impl DelayLine {
    pub fn new(sample_rate: f64, max_secs: f64) -> Self {
        let frames = (sample_rate * max_secs) as usize;
        Self {
            buf: vec![0.0; frames * 2],
            frames,
            write: 0,
        }
    }

    /// Process a stereo block in place: out = dry*(1-mix) + delayed*mix.
    /// `time` 0..1 maps to 0..max delay. Feeding silence rings the tail out.
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

pub struct GraphEngine {
    graph: Graph,
    drain: CommandDrain,
    voice: ClapVoice,
    delay: DelayLine,
    scratch: Vec<f32>,
    delay_in: Vec<f32>,
    // Route cache, refreshed on structural commands.
    keys_surge: bool,
    surge_delay: bool,
    delay_master: bool,
    surge_master: bool,
    // Knob cache.
    level: f32,
    time: f32,
    mix: f32,
    vol: f32,
    warm: f32,
}

impl GraphEngine {
    pub fn new(voice: ClapVoice, drain: CommandDrain, sample_rate: f64) -> Self {
        let mut e = Self {
            graph: Graph::demo_patch(),
            drain,
            voice,
            delay: DelayLine::new(sample_rate, 2.0),
            scratch: vec![0.0; 8192 * 2],
            delay_in: vec![0.0; 8192 * 2],
            keys_surge: false,
            surge_delay: false,
            delay_master: false,
            surge_master: false,
            level: 0.85,
            time: 0.4,
            mix: 0.5,
            vol: 0.8,
            warm: 0.15,
        };
        e.refresh_routes();
        e
    }

    pub fn graph(&self) -> &Graph {
        &self.graph
    }

    fn refresh_routes(&mut self) {
        let find = |pred: fn(&NodeKind) -> bool| -> Option<u32> {
            self.graph.nodes.iter().find(|n| pred(&n.kind)).map(|n| n.id)
        };
        let keys = find(|k| matches!(k, NodeKind::Keys));
        let surge = find(|k| matches!(k, NodeKind::Clap { .. }));
        let delay = find(|k| matches!(k, NodeKind::Dsp(DspKind::Delay)));
        let master = find(|k| matches!(k, NodeKind::MasterOut));
        let route = |a: Option<u32>, b: Option<u32>, kind: PortKind| -> bool {
            match (a, b) {
                (Some(a), Some(b)) => self.graph.has_route(a, b, kind),
                _ => false,
            }
        };
        self.keys_surge = route(keys, surge, PortKind::Midi);
        self.surge_delay = route(surge, delay, PortKind::Audio);
        self.delay_master = route(delay, master, PortKind::Audio);
        self.surge_master = route(surge, master, PortKind::Audio);
    }

    fn apply(&mut self, cmd: Command) {
        match cmd {
            Command::AddNode { kind, name } => {
                self.graph.add_node(kind, name);
                self.refresh_routes();
            }
            Command::RemoveNode(id) => {
                self.graph.remove_node(id);
                self.refresh_routes();
            }
            Command::Connect { id, from, to, kind } => {
                if self.graph.connect_with_id(id, from, to, kind) {
                    self.refresh_routes();
                }
            }
            Command::Disconnect(id) => {
                self.graph.disconnect(id);
                self.refresh_routes();
            }
            Command::SetParam { node, param, value } => {
                let v = value as f32;
                // Fixed rack: node 1 = Surge, 2 = Delay, 3 = Master.
                match (node, param) {
                    (1, P_LEVEL) => self.level = v,
                    (2, P_TIME) => self.time = v,
                    (2, P_MIX) => self.mix = v,
                    (3, P_VOL) => self.vol = v,
                    (3, P_WARM) => self.warm = v,
                    _ => {}
                }
            }
            Command::NoteOn { note, velocity } => {
                if self.keys_surge {
                    self.voice.note_on(note as u16, velocity as f64);
                }
            }
            Command::NoteOff { note } => {
                if self.keys_surge {
                    self.voice.note_off(note as u16);
                }
            }
            Command::TransportPlay | Command::TransportStop => {} // P2
        }
    }

    /// Render one device block. Allocation-free: commands are fixed-size,
    /// graph storage is reserved, buffers are preallocated.
    pub fn render(&mut self, data: &mut [f32], ch: usize) {
        while let Some(cmd) = self.drain.pop() {
            self.apply(cmd);
        }

        let frames = data.len() / ch;
        if frames * 2 > self.scratch.len() {
            for s in data.iter_mut() {
                *s = 0.0;
            }
            return;
        }

        self.voice.render(&mut self.scratch[..frames * 2]);
        for s in self.scratch[..frames * 2].iter_mut() {
            *s *= self.level;
        }

        // Delay path: feed Surge when wired, silence otherwise (tail rings out).
        if self.surge_delay {
            self.delay_in[..frames * 2].copy_from_slice(&self.scratch[..frames * 2]);
        } else {
            for s in self.delay_in[..frames * 2].iter_mut() {
                *s = 0.0;
            }
        }
        self.delay
            .process(&mut self.delay_in[..frames * 2], self.time, self.mix);

        for (i, frame) in data.chunks_mut(ch).enumerate() {
            let mut l = 0.0f32;
            let mut r = 0.0f32;
            if self.surge_master {
                l += self.scratch[i * 2];
                r += self.scratch[i * 2 + 1];
            }
            if self.delay_master {
                l += self.delay_in[i * 2];
                r += self.delay_in[i * 2 + 1];
            }
            l = (l * (1.0 + self.warm * 3.0)).tanh() * self.vol;
            r = (r * (1.0 + self.warm * 3.0)).tanh() * self.vol;
            frame[0] = l;
            if ch > 1 {
                frame[1] = r;
            }
            for s in frame.iter_mut().skip(2) {
                *s = 0.0;
            }
        }
    }
}
