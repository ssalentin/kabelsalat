//! Graph-driven audio: the rack's cables and knobs ARE the audio path.
//!
//! The UI publishes route flags and knob values as atomics (lock-free, no
//! allocation); the audio callback consumes them per block. Unplug a cable
//! and the signal stops — what you patch is what you hear.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

// Route bits (fixed P1 topology: Keys=0, Surge=1, Delay=2, Master=3).
pub const R_KEYS_SURGE: u32 = 1 << 0; // MIDI Keys -> Surge
pub const R_SURGE_DELAY: u32 = 1 << 1; // Audio Surge -> Delay
pub const R_DELAY_MASTER: u32 = 1 << 2; // Audio Delay -> Master
pub const R_SURGE_MASTER: u32 = 1 << 3; // Audio Surge -> Master (dry)

pub struct AudioShared {
    pub routes: AtomicU32,
    pub surge_level: AtomicU32, // f32 bits
    pub delay_time: AtomicU32,  // 0..1
    pub delay_mix: AtomicU32,   // 0..1
    pub master_vol: AtomicU32,  // 0..1
    pub master_warm: AtomicU32, // 0..1
}

impl AudioShared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            routes: AtomicU32::new(
                R_KEYS_SURGE | R_SURGE_DELAY | R_DELAY_MASTER | R_SURGE_MASTER,
            ),
            surge_level: AtomicU32::new(0.85f32.to_bits()),
            delay_time: AtomicU32::new(0.4f32.to_bits()),
            delay_mix: AtomicU32::new(0.5f32.to_bits()),
            master_vol: AtomicU32::new(0.8f32.to_bits()),
            master_warm: AtomicU32::new(0.15f32.to_bits()),
        })
    }

    #[inline]
    pub fn routes(&self) -> u32 {
        self.routes.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn f(&self, a: &AtomicU32) -> f32 {
        f32::from_bits(a.load(Ordering::Relaxed))
    }

    #[inline]
    pub fn set_f(a: &AtomicU32, v: f32) {
        a.store(v.to_bits(), Ordering::Relaxed);
    }
}

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
