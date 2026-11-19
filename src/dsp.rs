//! Built-in DSP node suite (task-7). RT-safe processors: no allocation,
//! locks, or syscalls inside `process`. Owned by the dsp workstream.
//!
//! All nodes are hand-rolled (no DSP dependencies), stereo-interleaved f32,
//! and take normalized 0..1 parameters via `set_param`. Every buffer (delay
//! lines, lookahead rings, reverb tanks) is allocated in the constructor;
//! `process` and `set_param` only touch preallocated memory and do arithmetic.
//! Knob changes are per-sample smoothed (one-pole slews) to avoid zipper
//! noise. See `docs/p1-dsp.md` for the full parameter tables.
//!
//! Integration: `build_processor(&DspKind, sample_rate)` is the factory the
//! engine uses to instantiate graph nodes (task-10).

use crate::control::DspKind;

/// Common interface for built-in nodes; the engine wires these into the graph.
pub trait Processor: Send {
    /// Process `frames` stereo-interleaved frames in place.
    fn process(&mut self, io: &mut [f32], frames: usize);
    fn set_param(&mut self, id: u32, value: f32);
    fn param_count(&self) -> u32;
    fn param_name(&self, id: u32) -> &'static str;
    /// Frames the output lags the input (a lookahead); the engine delays
    /// parallel paths to match (`engine::pdc`).
    fn latency_frames(&self) -> usize {
        0
    }
}

/// Instantiate a built-in node for the control plane's `DspKind`.
/// All allocation happens here (off the audio thread).
pub fn build_processor(kind: &DspKind, sample_rate: f64) -> Box<dyn Processor> {
    match kind {
        DspKind::Gain => Box::new(Gain::new(sample_rate)),
        DspKind::Pan => Box::new(Pan::new(sample_rate)),
        DspKind::Eq3 => Box::new(Eq3::new(sample_rate)),
        DspKind::Filter => Box::new(Filter::new(sample_rate)),
        DspKind::Limiter => Box::new(Limiter::new(sample_rate)),
        DspKind::Saturation => Box::new(Saturation::new(sample_rate)),
        DspKind::Delay => Box::new(Delay::new(sample_rate)),
        DspKind::Reverb => Box::new(Reverb::new(sample_rate)),
        DspKind::Metronome => Box::new(Metronome),
        DspKind::Bus => Box::new(BusFader::new(sample_rate)),
    }
}

/// Unity pass-through placeholder for the metronome node. The engine drives
/// click generation directly into the node's buffer (transport state), so
/// this processor only satisfies the `Processor` contract. No native params.
pub struct Metronome;

impl Processor for Metronome {
    fn process(&mut self, _io: &mut [f32], _frames: usize) {}
    fn set_param(&mut self, _id: u32, _value: f32) {}
    fn param_count(&self) -> u32 {
        0
    }
    fn param_name(&self, _id: u32) -> &'static str {
        "unknown"
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn clamp01(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

/// Exponential (log) mapping of a normalized knob to a physical range.
fn log_map(v: f32, lo: f32, hi: f32) -> f32 {
    lo * (hi / lo).powf(clamp01(v))
}

/// Inverse of `log_map` — used to express defaults in physical units.
fn log_norm(x: f32, lo: f32, hi: f32) -> f32 {
    ((x / lo).ln() / (hi / lo).ln()).clamp(0.0, 1.0)
}

/// Flush denormals to zero (keeps tails from costing CPU on some hardware).
#[inline]
fn flush(x: f32) -> f32 {
    if x.abs() < 1e-18 {
        0.0
    } else {
        x
    }
}

/// One-pole parameter smoother. `next()` is a few flops, no allocation.
/// State is f64: an f32 slew stalls a fraction of a unit away from large
/// targets (e.g. delay times in frames) once the increment rounds to zero.
struct Slew {
    value: f64,
    target: f64,
    coeff: f64,
}

impl Slew {
    fn new(sample_rate: f64, tau_ms: f64, initial: f32) -> Self {
        let coeff = (-1.0 / (sample_rate * tau_ms * 0.001)).exp();
        Self { value: initial as f64, target: initial as f64, coeff }
    }

    #[inline]
    fn set(&mut self, target: f32) {
        self.target = target as f64;
    }

    #[inline]
    fn next(&mut self) -> f32 {
        self.value += (1.0 - self.coeff) * (self.target - self.value);
        self.value as f32
    }
}

/// Frames to process, defensively bounded by the buffer length.
#[inline]
fn frame_count(io: &[f32], frames: usize) -> usize {
    frames.min(io.len() / 2)
}

// ---------------------------------------------------------------------------
// Gain
// ---------------------------------------------------------------------------

/// Stereo gain. Param 0 `gain`: amplitude = (2v)² — 0 = mute, 0.5 = unity,
/// 1 = +12 dB. Quadratic taper gives fine control around unity.
pub struct Gain {
    amp: Slew,
}

impl Gain {
    pub fn new(sample_rate: f64) -> Self {
        Self { amp: Slew::new(sample_rate, 10.0, 1.0) }
    }
}

impl Processor for Gain {
    fn process(&mut self, io: &mut [f32], frames: usize) {
        let n = frame_count(io, frames);
        for f in 0..n {
            let g = self.amp.next();
            io[2 * f] *= g;
            io[2 * f + 1] *= g;
        }
    }

    fn set_param(&mut self, id: u32, value: f32) {
        if id == 0 {
            let a = 2.0 * clamp01(value);
            self.amp.set(a * a);
        }
    }

    fn param_count(&self) -> u32 {
        1
    }

    fn param_name(&self, id: u32) -> &'static str {
        match id {
            0 => "gain",
            _ => "unknown",
        }
    }
}

// ---------------------------------------------------------------------------
// Bus fader
// ---------------------------------------------------------------------------

/// An aux bus's fader (issue #5). Param 0 is the linear gain itself (0..=1,
/// 1 = unity), the same scale the mixer's dB faders ride, slewed like Gain.
pub struct BusFader {
    amp: Slew,
}

impl BusFader {
    pub fn new(sample_rate: f64) -> Self {
        Self { amp: Slew::new(sample_rate, 10.0, 1.0) }
    }
}

impl Processor for BusFader {
    fn process(&mut self, io: &mut [f32], frames: usize) {
        let n = frame_count(io, frames);
        for f in 0..n {
            let g = self.amp.next();
            io[2 * f] *= g;
            io[2 * f + 1] *= g;
        }
    }

    fn set_param(&mut self, id: u32, value: f32) {
        if id == 0 {
            self.amp.set(clamp01(value));
        }
    }

    fn param_count(&self) -> u32 {
        1
    }

    fn param_name(&self, id: u32) -> &'static str {
        match id {
            0 => "level",
            _ => "unknown",
        }
    }
}

// ---------------------------------------------------------------------------
// Pan
// ---------------------------------------------------------------------------

/// Constant-power stereo pan. Param 0 `pan`: 0 = hard left, 0.5 = center
/// (-3 dB per side), 1 = hard right. L = cos(v·π/2), R = sin(v·π/2).
pub struct Pan {
    l: Slew,
    r: Slew,
}

impl Pan {
    pub fn new(sample_rate: f64) -> Self {
        let c = std::f32::consts::FRAC_1_SQRT_2;
        Self { l: Slew::new(sample_rate, 10.0, c), r: Slew::new(sample_rate, 10.0, c) }
    }
}

impl Processor for Pan {
    fn process(&mut self, io: &mut [f32], frames: usize) {
        let n = frame_count(io, frames);
        for f in 0..n {
            let l = self.l.next();
            let r = self.r.next();
            io[2 * f] *= l;
            io[2 * f + 1] *= r;
        }
    }

    fn set_param(&mut self, id: u32, value: f32) {
        if id == 0 {
            let a = clamp01(value) * std::f32::consts::FRAC_PI_2;
            self.l.set(a.cos());
            self.r.set(a.sin());
        }
    }

    fn param_count(&self) -> u32 {
        1
    }

    fn param_name(&self, id: u32) -> &'static str {
        match id {
            0 => "pan",
            _ => "unknown",
        }
    }
}

// ---------------------------------------------------------------------------
// Eq3 — three-band parametric EQ (RBJ biquads, transposed direct form II)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

#[derive(Clone, Copy, Default)]
struct BiquadState {
    z1: f32,
    z2: f32,
}

impl BiquadState {
    #[inline]
    fn process(&mut self, c: &Biquad, x: f32) -> f32 {
        let y = c.b0 * x + self.z1;
        self.z1 = flush(c.b1 * x - c.a1 * y + self.z2);
        self.z2 = flush(c.b2 * x - c.a2 * y);
        y
    }
}

/// RBJ audio-EQ-cookbook coefficient builders (normalized by a0).
fn peaking(sr: f64, freq: f32, q: f32, gain_db: f32) -> Biquad {
    let a = 10.0f32.powf(gain_db / 40.0);
    let w0 = 2.0 * std::f32::consts::PI * (freq / sr as f32);
    let (s, c) = w0.sin_cos();
    let alpha = s / (2.0 * q);
    let a0 = 1.0 + alpha / a;
    Biquad {
        b0: (1.0 + alpha * a) / a0,
        b1: (-2.0 * c) / a0,
        b2: (1.0 - alpha * a) / a0,
        a1: (-2.0 * c) / a0,
        a2: (1.0 - alpha / a) / a0,
    }
}

fn shelf(sr: f64, freq: f32, gain_db: f32, high: bool) -> Biquad {
    let a = 10.0f32.powf(gain_db / 40.0);
    let w0 = 2.0 * std::f32::consts::PI * (freq / sr as f32);
    let (s, c) = w0.sin_cos();
    // Shelf slope S = 1.
    let alpha = s / 2.0 * 2.0f32.sqrt(); // shelf slope S = 1
    let sa = 2.0 * a.sqrt() * alpha;
    let (a0, a1, a2, b0, b1, b2) = if high {
        (
            (a + 1.0) - (a - 1.0) * c + sa,
            2.0 * ((a - 1.0) - (a + 1.0) * c),
            (a + 1.0) - (a - 1.0) * c - sa,
            a * ((a + 1.0) + (a - 1.0) * c + sa),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * c),
            a * ((a + 1.0) + (a - 1.0) * c - sa),
        )
    } else {
        (
            (a + 1.0) + (a - 1.0) * c + sa,
            -2.0 * ((a - 1.0) + (a + 1.0) * c),
            (a + 1.0) + (a - 1.0) * c - sa,
            a * ((a + 1.0) - (a - 1.0) * c + sa),
            2.0 * a * ((a - 1.0) - (a + 1.0) * c),
            a * ((a + 1.0) - (a - 1.0) * c - sa),
        )
    };
    Biquad { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
}

/// Three-band parametric EQ: low shelf, peaking mid, high shelf, cascaded.
///
/// Params (all 0..1):
/// - 0 `low_gain`, 1 `mid_gain`, 2 `high_gain`: -18..+18 dB, 0.5 = 0 dB.
/// - 3 `low_freq`: 30..300 Hz (log), default 100 Hz.
/// - 4 `mid_freq`: 200..8000 Hz (log), default 1000 Hz.
/// - 5 `high_freq`: 2000..16000 Hz (log), default 8000 Hz.
/// - 6 `mid_q`: 0.3..5 (log), default 0.707.
pub struct Eq3 {
    sr: f32,
    norms: [f32; 7],
    bands: [Biquad; 3],
    state: [[BiquadState; 3]; 2],
}

impl Eq3 {
    pub fn new(sample_rate: f64) -> Self {
        let mut eq = Self {
            sr: sample_rate as f32,
            norms: [
                0.5,
                0.5,
                0.5,
                log_norm(100.0, 30.0, 300.0),
                log_norm(1000.0, 200.0, 8000.0),
                log_norm(8000.0, 2000.0, 16000.0),
                log_norm(0.707, 0.3, 5.0),
            ],
            bands: [Biquad::default(); 3],
            state: [[BiquadState::default(); 3]; 2],
        };
        eq.recompute();
        eq
    }

    /// Coefficients are rebuilt on parameter changes only (block boundary),
    /// never per sample.
    fn recompute(&mut self) {
        let n = &self.norms;
        let db = |v: f32| -18.0 + 36.0 * v;
        let clamp_f = |f: f32| f.clamp(10.0, self.sr * 0.45);
        let lf = clamp_f(log_map(n[3], 30.0, 300.0));
        let mf = clamp_f(log_map(n[4], 200.0, 8000.0));
        let hf = clamp_f(log_map(n[5], 2000.0, 16000.0));
        let q = log_map(n[6], 0.3, 5.0);
        let sr = self.sr as f64;
        self.bands[0] = shelf(sr, lf, db(n[0]), false);
        self.bands[1] = peaking(sr, mf, q, db(n[1]));
        self.bands[2] = shelf(sr, hf, db(n[2]), true);
    }
}

impl Processor for Eq3 {
    fn process(&mut self, io: &mut [f32], frames: usize) {
        let n = frame_count(io, frames);
        for f in 0..n {
            for ch in 0..2 {
                let mut x = io[2 * f + ch];
                for b in 0..3 {
                    x = self.state[ch][b].process(&self.bands[b], x);
                }
                io[2 * f + ch] = x;
            }
        }
    }

    fn set_param(&mut self, id: u32, value: f32) {
        if (id as usize) < self.norms.len() {
            self.norms[id as usize] = clamp01(value);
            self.recompute();
        }
    }

    fn param_count(&self) -> u32 {
        7
    }

    fn param_name(&self, id: u32) -> &'static str {
        match id {
            0 => "low_gain",
            1 => "mid_gain",
            2 => "high_gain",
            3 => "low_freq",
            4 => "mid_freq",
            5 => "high_freq",
            6 => "mid_q",
            _ => "unknown",
        }
    }
}

// ---------------------------------------------------------------------------
// Filter — resonant LP/HP + bipolar DJ combo (TPT state-variable, 12 dB/oct)
// ---------------------------------------------------------------------------

/// Resonant filter. Params:
/// - 0 `cutoff`: 20..20000 Hz (log). In DJ mode this knob is bipolar:
///   0.5 = open, <0.5 = highpass (rising toward the left extreme),
///   >0.5 = lowpass (closing toward the right extreme).
/// - 1 `resonance`: Q = 0.5·32^v (0.5 = damped, 1 = Q 16, near self-oscillation).
/// - 2 `mode`: [0, 1/3) = lowpass, [1/3, 2/3) = highpass, [2/3, 1] = DJ combo.
///
/// Topology: Cytomic/Andy Simper TPT state-variable filter — stable under
/// per-sample coefficient modulation, LP and HP taps from the same state.
pub struct Filter {
    sr: f32,
    cutoff_norm: f32,
    reso_norm: f32,
    mode: u8, // 0 lp, 1 hp, 2 dj
    g: Slew,
    k: Slew,
    ic: [[f32; 2]; 2], // per-channel integrator state
}

impl Filter {
    pub fn new(sample_rate: f64) -> Self {
        let mut f = Self {
            sr: sample_rate as f32,
            cutoff_norm: 1.0,
            reso_norm: 0.0,
            mode: 0,
            g: Slew::new(sample_rate, 10.0, 1.0),
            k: Slew::new(sample_rate, 10.0, 2.0),
            ic: [[0.0; 2]; 2],
        };
        f.retarget();
        // Start settled at the default (open) coefficients.
        f.g.value = f.g.target;
        f.k.value = f.k.target;
        f
    }

    fn cutoff_hz(&self) -> f32 {
        let v = self.cutoff_norm;
        match self.mode {
            2 => {
                // Bipolar DJ sweep: center open, extremes fully closed.
                if v < 0.5 {
                    log_map(1.0 - 2.0 * v, 20.0, 20000.0) // HP side
                } else {
                    log_map(2.0 * v - 1.0, 20000.0, 20.0) // LP side
                }
            }
            _ => log_map(v, 20.0, 20000.0),
        }
    }

    fn retarget(&mut self) {
        let fc = self.cutoff_hz().clamp(10.0, self.sr * 0.45);
        let g = (std::f32::consts::PI * fc / self.sr).tan();
        let q = 0.5 * 32.0f32.powf(self.reso_norm);
        self.g.set(g);
        self.k.set(1.0 / q);
    }
}

impl Processor for Filter {
    fn process(&mut self, io: &mut [f32], frames: usize) {
        let n = frame_count(io, frames);
        for f in 0..n {
            let g = self.g.next();
            let k = self.k.next();
            let a1 = 1.0 / (1.0 + g * (g + k));
            let a2 = g * a1;
            let a3 = g * a2;
            for ch in 0..2 {
                let x = io[2 * f + ch];
                let [ic1, ic2] = self.ic[ch];
                let v3 = x - ic2;
                let v1 = a1 * ic1 + a2 * v3;
                let v2 = ic2 + a2 * ic1 + a3 * v3;
                self.ic[ch] = [flush(2.0 * v1 - ic1), flush(2.0 * v2 - ic2)];
                let lp = v2;
                let hp = x - k * v1 - v2;
                io[2 * f + ch] = match self.mode {
                    1 => hp,
                    2 => {
                        if self.cutoff_norm < 0.5 {
                            hp
                        } else {
                            lp
                        }
                    }
                    _ => lp,
                };
            }
        }
    }

    fn set_param(&mut self, id: u32, value: f32) {
        let v = clamp01(value);
        match id {
            0 => self.cutoff_norm = v,
            1 => self.reso_norm = v,
            2 => {
                self.mode = if v < 1.0 / 3.0 {
                    0
                } else if v < 2.0 / 3.0 {
                    1
                } else {
                    2
                };
            }
            _ => return,
        }
        self.retarget();
    }

    fn param_count(&self) -> u32 {
        3
    }

    fn param_name(&self, id: u32) -> &'static str {
        match id {
            0 => "cutoff",
            1 => "resonance",
            2 => "mode",
            _ => "unknown",
        }
    }
}

// ---------------------------------------------------------------------------
// Limiter — lookahead peak limiter
// ---------------------------------------------------------------------------

/// Lookahead limiter (fixed 5 ms lookahead). The input is delayed through a
/// preallocated ring while a peak envelope computes gain reduction ahead of
/// time, so transients are caught without overshoot.
///
/// Params:
/// - 0 `ceiling`: output ceiling -12..0 dB (1 = 0 dBFS).
/// - 1 `drive`: input gain 0..+24 dB (pushes material into the limiter).
/// - 2 `release`: 10..500 ms (log).
///
/// Reports its lookahead as `latency_frames`, which the engine compensates.
pub struct Limiter {
    sr: f64,
    ring: Vec<f32>, // stereo interleaved lookahead ring
    ring_frames: usize,
    write: usize,
    lookahead: usize,
    env: f32,
    gain: f32,
    ceiling: Slew,
    drive: Slew,
    rel_coef: f32,
    atk_coef: f32,
    up_coef: f32,
}

impl Limiter {
    pub fn new(sample_rate: f64) -> Self {
        let lookahead = (sample_rate * 0.005).round().max(1.0) as usize;
        let ring_frames = lookahead + 1;
        let tau_rel = 0.040; // ~40 ms default (release knob 0.35)
        Self {
            sr: sample_rate,
            ring: vec![0.0; ring_frames * 2],
            ring_frames,
            write: 0,
            lookahead,
            env: 0.0,
            gain: 1.0,
            ceiling: Slew::new(sample_rate, 5.0, 1.0),
            drive: Slew::new(sample_rate, 5.0, 1.0),
            rel_coef: (-1.0 / (sample_rate * tau_rel)).exp() as f32,
            atk_coef: (-1.0 / (sample_rate * 0.0005)).exp() as f32,
            up_coef: (-1.0 / (sample_rate * tau_rel * 0.5)).exp() as f32,
        }
    }
}

impl Processor for Limiter {
    fn process(&mut self, io: &mut [f32], frames: usize) {
        let n = frame_count(io, frames);
        for f in 0..n {
            let l = io[2 * f];
            let r = io[2 * f + 1];
            let drive = self.drive.next();
            let ceiling = self.ceiling.next();

            // Peak envelope over the driven input: instant attack, exp release.
            let peak = l.abs().max(r.abs()) * drive;
            self.env = (self.env * self.rel_coef).max(peak);
            let target = if self.env > ceiling {
                ceiling / self.env.max(1e-9)
            } else {
                1.0
            };
            // Fast attack (lookahead covers the rest), knob-controlled release.
            let coef = if target < self.gain { self.atk_coef } else { self.up_coef };
            self.gain = coef * self.gain + (1.0 - coef) * target;

            // Output the sample from `lookahead` frames ago, gained down.
            let rd = (self.write + self.ring_frames - self.lookahead) % self.ring_frames;
            io[2 * f] = self.ring[2 * rd] * self.gain;
            io[2 * f + 1] = self.ring[2 * rd + 1] * self.gain;
            self.ring[2 * self.write] = l;
            self.ring[2 * self.write + 1] = r;
            self.write = (self.write + 1) % self.ring_frames;
        }
    }

    fn set_param(&mut self, id: u32, value: f32) {
        let v = clamp01(value);
        match id {
            0 => self.ceiling.set(10.0f32.powf(-12.0 * (1.0 - v) / 20.0)),
            1 => self.drive.set(10.0f32.powf(24.0 * v / 20.0)),
            2 => {
                let tau = log_map(v, 0.010, 0.500) as f64;
                self.rel_coef = (-1.0 / (self.sr * tau)).exp() as f32;
                self.up_coef = (-1.0 / (self.sr * tau * 0.5)).exp() as f32;
            }
            _ => {}
        }
    }

    fn param_count(&self) -> u32 {
        3
    }

    fn param_name(&self, id: u32) -> &'static str {
        match id {
            0 => "ceiling",
            1 => "drive",
            2 => "release",
            _ => "unknown",
        }
    }

    fn latency_frames(&self) -> usize {
        self.lookahead
    }
}

// ---------------------------------------------------------------------------
// Saturation — tanh waveshaper with drive, mix, output trim
// ---------------------------------------------------------------------------

/// Soft saturation. Params:
/// - 0 `drive`: waveshaper input gain 1..40 (exponential; 0 = gentlest).
/// - 1 `mix`: dry/wet blend (0 = dry, 1 = fully saturated).
/// - 2 `output`: linear output trim 0..2 (0.5 = unity).
///
/// Transfer: wet = tanh(x·drive). Odd-symmetric, bounded to ±1 pre-trim.
pub struct Saturation {
    drive: Slew,
    mix: Slew,
    trim: Slew,
}

impl Saturation {
    pub fn new(sample_rate: f64) -> Self {
        Self {
            drive: Slew::new(sample_rate, 10.0, 1.0),
            mix: Slew::new(sample_rate, 10.0, 1.0),
            trim: Slew::new(sample_rate, 10.0, 1.0),
        }
    }
}

impl Processor for Saturation {
    fn process(&mut self, io: &mut [f32], frames: usize) {
        let n = frame_count(io, frames);
        for f in 0..n {
            let d = self.drive.next();
            let m = self.mix.next();
            let t = self.trim.next();
            for ch in 0..2 {
                let x = io[2 * f + ch];
                let wet = (x * d).tanh();
                io[2 * f + ch] = (x * (1.0 - m) + wet * m) * t;
            }
        }
    }

    fn set_param(&mut self, id: u32, value: f32) {
        let v = clamp01(value);
        match id {
            0 => self.drive.set(40.0f32.powf(v)),
            1 => self.mix.set(v),
            2 => self.trim.set(2.0 * v),
            _ => {}
        }
    }

    fn param_count(&self) -> u32 {
        3
    }

    fn param_name(&self, id: u32) -> &'static str {
        match id {
            0 => "drive",
            1 => "mix",
            2 => "output",
            _ => "unknown",
        }
    }
}

// ---------------------------------------------------------------------------
// Delay — stereo / ping-pong with feedback, damping, fractional interpolation
// ---------------------------------------------------------------------------

/// Stereo delay (max 2 s, preallocated). Params:
/// - 0 `time`: `DELAY_MIN_S`..`DELAY_MAX_S` (log).
/// - 1 `feedback`: 0..0.95 (never reaches unity — always decays).
/// - 2 `mix`: dry/wet (0 = dry passthrough, 1 = echoes only).
/// - 3 `pingpong`: <0.5 = parallel stereo, ≥0.5 = cross-channel ping-pong.
/// - 4 `damping`: feedback lowpass 20 kHz..500 Hz (0 = bright, 1 = dark).
///
/// Delay time is slewed and read with linear interpolation, so live sweeps
/// glide instead of clicking.
/// Shortest echo the TIME knob reaches. Below ~50 ms a fed-back delay is a
/// resonant comb filter, not an echo: the old 1 ms floor put the face's
/// default (0.4) at 21 ms, and every new delay sounded distorted.
pub const DELAY_MIN_S: f32 = 0.05;
pub const DELAY_MAX_S: f32 = 2.0;

pub struct Delay {
    sr: f32,
    buf: Vec<f32>, // stereo interleaved
    frames: usize,
    write: usize,
    delay: Slew, // in frames
    fb: Slew,
    mix: Slew,
    damp: Slew, // one-pole coefficient
    damp_state: [f32; 2],
    pingpong: bool,
}

impl Delay {
    pub fn new(sample_rate: f64) -> Self {
        let frames = (sample_rate * 2.0).round().max(4.0) as usize;
        let sr = sample_rate as f32;
        // Default damping knob 0 = bright (20 kHz feedback lowpass).
        let dc0 = 1.0 - (-2.0 * std::f32::consts::PI * 20000.0 / sr).exp();
        Self {
            sr,
            buf: vec![0.0; frames * 2],
            frames,
            write: 0,
            delay: Slew::new(sample_rate, 30.0, sr * 0.300),
            fb: Slew::new(sample_rate, 10.0, 0.35 * 0.95),
            mix: Slew::new(sample_rate, 10.0, 0.5),
            damp: Slew::new(sample_rate, 10.0, dc0),
            damp_state: [0.0; 2],
            pingpong: false,
        }
    }

    fn damp_coef(&self, v: f32) -> f32 {
        let fc = log_map(v, 20000.0, 500.0);
        1.0 - (-2.0 * std::f32::consts::PI * fc / self.sr).exp()
    }

    /// Read the delayed stereo pair at fractional distance `d` frames back.
    #[inline]
    fn read(&self, d: f32) -> (f32, f32) {
        let pos = (self.write as f32 - d).rem_euclid(self.frames as f32);
        let i0 = pos.floor() as usize % self.frames;
        let i1 = (i0 + 1) % self.frames;
        let fr = pos - pos.floor();
        let l = self.buf[2 * i0] * (1.0 - fr) + self.buf[2 * i1] * fr;
        let r = self.buf[2 * i0 + 1] * (1.0 - fr) + self.buf[2 * i1 + 1] * fr;
        (l, r)
    }
}

impl Processor for Delay {
    fn process(&mut self, io: &mut [f32], frames: usize) {
        let n = frame_count(io, frames);
        let max_d = (self.frames - 2) as f32;
        for f in 0..n {
            let d = self.delay.next().clamp(1.0, max_d);
            let fb = self.fb.next();
            let mix = self.mix.next();
            let dc = self.damp.next();

            let (dl, dr) = self.read(d);
            // Damped feedback (one-pole lowpass per channel).
            self.damp_state[0] = flush(self.damp_state[0] + dc * (dl - self.damp_state[0]));
            self.damp_state[1] = flush(self.damp_state[1] + dc * (dr - self.damp_state[1]));
            let (fl, fr) = (self.damp_state[0] * fb, self.damp_state[1] * fb);

            let li = io[2 * f];
            let ri = io[2 * f + 1];
            let (wl, wr) = if self.pingpong { (li + fr, ri + fl) } else { (li + fl, ri + fr) };
            self.buf[2 * self.write] = wl;
            self.buf[2 * self.write + 1] = wr;
            self.write = (self.write + 1) % self.frames;

            io[2 * f] = li * (1.0 - mix) + dl * mix;
            io[2 * f + 1] = ri * (1.0 - mix) + dr * mix;
        }
    }

    fn set_param(&mut self, id: u32, value: f32) {
        let v = clamp01(value);
        match id {
            0 => {
                let secs = log_map(v, DELAY_MIN_S, DELAY_MAX_S);
                self.delay.set(self.sr * secs);
            }
            1 => self.fb.set(0.95 * v),
            2 => self.mix.set(v),
            3 => self.pingpong = v >= 0.5,
            4 => {
                let c = self.damp_coef(v);
                self.damp.set(c);
            }
            _ => {}
        }
    }

    fn param_count(&self) -> u32 {
        5
    }

    fn param_name(&self, id: u32) -> &'static str {
        match id {
            0 => "time",
            1 => "feedback",
            2 => "mix",
            3 => "pingpong",
            4 => "damping",
            _ => "unknown",
        }
    }
}

// ---------------------------------------------------------------------------
// Reverb — Freeverb-style Schroeder tank (8 damped combs + 4 allpasses / ch)
// ---------------------------------------------------------------------------

const COMB_TUNING: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
const ALLPASS_TUNING: [usize; 4] = [556, 441, 341, 225];
const STEREO_SPREAD: usize = 23;

struct Comb {
    buf: Vec<f32>,
    idx: usize,
    store: f32, // damping lowpass state
}

impl Comb {
    fn new(len: usize) -> Self {
        Self { buf: vec![0.0; len.max(4)], idx: 0, store: 0.0 }
    }

    #[inline]
    fn process(&mut self, input: f32, feedback: f32, damp: f32) -> f32 {
        let out = self.buf[self.idx];
        self.store = flush(out * (1.0 - damp) + self.store * damp);
        self.buf[self.idx] = input + self.store * feedback;
        self.idx += 1;
        if self.idx == self.buf.len() {
            self.idx = 0;
        }
        out
    }
}

struct Allpass {
    buf: Vec<f32>,
    idx: usize,
}

impl Allpass {
    fn new(len: usize) -> Self {
        Self { buf: vec![0.0; len.max(4)], idx: 0 }
    }

    #[inline]
    fn process(&mut self, input: f32) -> f32 {
        let bufout = self.buf[self.idx];
        let out = bufout - input;
        self.buf[self.idx] = input + bufout * 0.5;
        self.idx += 1;
        if self.idx == self.buf.len() {
            self.idx = 0;
        }
        out
    }
}

/// Freeverb-style Schroeder reverb. The classic comb/allpass tunings are
/// scaled from 44.1 kHz to the running sample rate; the right channel is
/// offset by the stereo spread. Params:
/// - 0 `size`: room size → comb feedback 0.70..0.98 (larger = longer tail).
/// - 1 `damping`: high-frequency absorption in the comb loop (0 = bright).
/// - 2 `mix`: dry/wet (0 = dry passthrough).
/// - 3 `width`: stereo width of the wet field (0 = mono, 1 = full).
pub struct Reverb {
    combs_l: [Comb; 8],
    combs_r: [Comb; 8],
    aps_l: [Allpass; 4],
    aps_r: [Allpass; 4],
    fb: Slew,
    damp: Slew,
    mix: Slew,
    width: Slew,
}

impl Reverb {
    pub fn new(sample_rate: f64) -> Self {
        let scale = sample_rate / 44100.0;
        let len = |t: usize| ((t as f64) * scale).round() as usize;
        Self {
            combs_l: std::array::from_fn(|i| Comb::new(len(COMB_TUNING[i]))),
            combs_r: std::array::from_fn(|i| Comb::new(len(COMB_TUNING[i]) + STEREO_SPREAD)),
            aps_l: std::array::from_fn(|i| Allpass::new(len(ALLPASS_TUNING[i]))),
            aps_r: std::array::from_fn(|i| Allpass::new(len(ALLPASS_TUNING[i]) + STEREO_SPREAD)),
            fb: Slew::new(sample_rate, 20.0, 0.70 + 0.28 * 0.5),
            damp: Slew::new(sample_rate, 20.0, 0.5 * 0.4),
            mix: Slew::new(sample_rate, 10.0, 0.3),
            width: Slew::new(sample_rate, 10.0, 1.0),
        }
    }
}

impl Processor for Reverb {
    fn process(&mut self, io: &mut [f32], frames: usize) {
        let n = frame_count(io, frames);
        for f in 0..n {
            let fb = self.fb.next();
            let damp = self.damp.next();
            let mix = self.mix.next();
            let width = self.width.next();

            let l = io[2 * f];
            let r = io[2 * f + 1];
            let input = (l + r) * 0.01; // fixed input gain, keeps the tank tame

            let mut wl = 0.0;
            let mut wr = 0.0;
            for i in 0..8 {
                wl += self.combs_l[i].process(input, fb, damp);
                wr += self.combs_r[i].process(input, fb, damp);
            }
            for ap in self.aps_l.iter_mut() {
                wl = ap.process(wl);
            }
            for ap in self.aps_r.iter_mut() {
                wr = ap.process(wr);
            }

            let w1 = width * 0.5 + 0.5;
            let w2 = (1.0 - width) * 0.5;
            let wet_l = (wl * w1 + wr * w2) * 2.0;
            let wet_r = (wr * w1 + wl * w2) * 2.0;
            io[2 * f] = l * (1.0 - mix) + wet_l * mix;
            io[2 * f + 1] = r * (1.0 - mix) + wet_r * mix;
        }
    }

    fn set_param(&mut self, id: u32, value: f32) {
        let v = clamp01(value);
        match id {
            0 => self.fb.set(0.70 + 0.28 * v),
            1 => self.damp.set(0.4 * v),
            2 => self.mix.set(v),
            3 => self.width.set(v),
            _ => {}
        }
    }

    fn param_count(&self) -> u32 {
        4
    }

    fn param_name(&self, id: u32) -> &'static str {
        match id {
            0 => "size",
            1 => "damping",
            2 => "mix",
            3 => "width",
            _ => "unknown",
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f64 = 48000.0;

    fn run(p: &mut dyn Processor, io: &mut [f32]) {
        let n = io.len() / 2;
        p.process(io, n);
    }

    fn silence(p: &mut dyn Processor, frames: usize) {
        let mut io = vec![0.0; frames * 2];
        run(p, &mut io);
    }

    fn sine(freq: f32, frames: usize, amp: f32) -> Vec<f32> {
        let mut io = vec![0.0; frames * 2];
        for f in 0..frames {
            let x = amp * (2.0 * std::f32::consts::PI * freq * f as f32 / SR as f32).sin();
            io[2 * f] = x;
            io[2 * f + 1] = x;
        }
        io
    }

    fn rms(io: &[f32]) -> f32 {
        (io.iter().map(|x| x * x).sum::<f32>() / io.len() as f32).sqrt()
    }

    fn peak(io: &[f32]) -> f32 {
        io.iter().fold(0.0f32, |m, x| m.max(x.abs()))
    }

    /// Deterministic noise (no rand dependency).
    fn noise(frames: usize, amp: f32) -> Vec<f32> {
        let mut io = vec![0.0; frames * 2];
        let mut s: u32 = 0x1234_5678;
        for x in io.iter_mut() {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            *x = amp * (s as f32 / u32::MAX as f32 * 2.0 - 1.0);
        }
        io
    }

    fn all_finite(io: &[f32]) -> bool {
        io.iter().all(|x| x.is_finite())
    }

    // -- Gain ---------------------------------------------------------------

    #[test]
    fn gain_unity_and_extremes() {
        let mut g = Gain::new(SR);
        // Default 0.5 = unity (already settled at construction).
        let mut io = sine(1000.0, 480, 0.5);
        let before = io.clone();
        run(&mut g, &mut io);
        for (a, b) in io.iter().zip(before.iter()) {
            assert!((a - b).abs() < 1e-4, "unity gain must pass through");
        }
        // Mute.
        g.set_param(0, 0.0);
        silence(&mut g, 4800);
        let mut io = sine(1000.0, 480, 0.5);
        run(&mut g, &mut io);
        assert!(peak(&io) < 1e-4);
        // Max = +12 dB = 4x.
        g.set_param(0, 1.0);
        silence(&mut g, 4800);
        let mut io = sine(1000.0, 480, 0.1);
        run(&mut g, &mut io);
        assert!((peak(&io) - 0.4).abs() < 1e-3);
    }

    // -- Pan ----------------------------------------------------------------

    #[test]
    fn pan_center_and_extremes() {
        let mut p = Pan::new(SR);
        let mut io = sine(1000.0, 480, 0.5);
        run(&mut p, &mut io);
        for f in 0..480 {
            assert!((io[2 * f] - io[2 * f + 1]).abs() < 1e-5, "center: L == R");
            // Constant power: L² + R² equals the mono source power x².
            let power = io[2 * f] * io[2 * f] + io[2 * f + 1] * io[2 * f + 1];
            let x = 0.5f32 * (2.0 * std::f32::consts::PI * 1000.0 * f as f32 / SR as f32).sin();
            assert!((power - x * x).abs() < 1e-4, "constant power");
        }
        // Hard left.
        p.set_param(0, 0.0);
        silence(&mut p, 4800);
        let mut io = sine(1000.0, 480, 0.5);
        run(&mut p, &mut io);
        let l_peak = io.iter().step_by(2).fold(0.0f32, |m, x| m.max(x.abs()));
        let r_peak = io.iter().skip(1).step_by(2).fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(l_peak > 0.49 && r_peak < 1e-4);
        // Hard right.
        p.set_param(0, 1.0);
        silence(&mut p, 4800);
        let mut io = sine(1000.0, 480, 0.5);
        run(&mut p, &mut io);
        let l_peak = io.iter().step_by(2).fold(0.0f32, |m, x| m.max(x.abs()));
        let r_peak = io.iter().skip(1).step_by(2).fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(r_peak > 0.49 && l_peak < 1e-4);
    }

    // -- Eq3 ------------------------------------------------------------------

    #[test]
    fn eq_flat_is_identity() {
        let mut eq = Eq3::new(SR);
        let mut io = noise(4800, 0.5);
        let before = io.clone();
        run(&mut eq, &mut io);
        for (a, b) in io.iter().zip(before.iter()) {
            assert!((a - b).abs() < 1e-4, "0 dB EQ must be transparent");
        }
    }

    #[test]
    fn eq_low_boost_shapes_spectrum() {
        let measure = |boost: f32, freq: f32| -> f32 {
            let mut eq = Eq3::new(SR);
            eq.set_param(0, boost);
            silence(&mut eq, 9600);
            let mut io = sine(freq, 24000, 0.1);
            run(&mut eq, &mut io);
            rms(&io[9600..])
        };
        let low_boosted = measure(1.0, 50.0);
        let low_flat = measure(0.5, 50.0);
        let high_boosted = measure(1.0, 6000.0);
        let high_flat = measure(0.5, 6000.0);
        assert!(low_boosted > low_flat * 4.0, "low shelf must boost 50 Hz");
        assert!((high_boosted - high_flat).abs() < high_flat * 0.15, "low shelf leaves 6 kHz alone");
    }

    #[test]
    fn eq_extremes_stable() {
        for v in [0.0f32, 1.0] {
            let mut eq = Eq3::new(SR);
            for id in 0..7 {
                eq.set_param(id, v);
            }
            let mut io = noise(48000, 0.5);
            run(&mut eq, &mut io);
            assert!(all_finite(&io));
            assert!(peak(&io) < 1000.0, "EQ must not explode at extremes");
        }
    }

    // -- Filter ---------------------------------------------------------------

    #[test]
    fn filter_lp_step_passes_dc() {
        let mut f = Filter::new(SR);
        f.set_param(2, 0.0); // LP
        f.set_param(0, log_norm(1000.0, 20.0, 20000.0));
        silence(&mut f, 4800);
        let mut io = vec![1.0; 4800 * 2];
        run(&mut f, &mut io);
        let tail = &io[io.len() - 200..];
        assert!(tail.iter().all(|&x| (x - 1.0).abs() < 0.02), "LP passes DC");
    }

    #[test]
    fn filter_hp_step_blocks_dc() {
        let mut f = Filter::new(SR);
        f.set_param(2, 0.5); // HP
        f.set_param(0, log_norm(1000.0, 20.0, 20000.0));
        silence(&mut f, 4800);
        let mut io = vec![1.0; 4800 * 2];
        run(&mut f, &mut io);
        let tail = &io[io.len() - 200..];
        assert!(tail.iter().all(|&x| x.abs() < 0.02), "HP blocks DC");
    }

    #[test]
    fn filter_resonance_peak() {
        let mut f = Filter::new(SR);
        f.set_param(2, 0.0);
        f.set_param(0, log_norm(1000.0, 20.0, 20000.0));
        f.set_param(1, 1.0); // Q 16
        silence(&mut f, 9600);
        let mut io = sine(1000.0, 24000, 0.05);
        run(&mut f, &mut io);
        let r = rms(&io[9600..]);
        assert!(r > 0.05 * 3.0 / 2.0f32.sqrt(), "resonance must amplify at cutoff, got rms {r}");
        assert!(all_finite(&io));
    }

    #[test]
    fn filter_dj_center_is_open() {
        let mut f = Filter::new(SR);
        f.set_param(2, 1.0); // DJ combo
        f.set_param(0, 0.5); // center = open
        silence(&mut f, 9600);
        let mut io = sine(1000.0, 4800, 0.4);
        let before = io.clone();
        run(&mut f, &mut io);
        // Compare RMS: an open filter may still shift phase slightly.
        let r_in = rms(&before[960..]);
        let r_out = rms(&io[960..]);
        assert!((r_out - r_in).abs() < 0.03 * r_in, "DJ center must be ~transparent");
        // Left extreme = HP closing in: a 100 Hz tone gets killed.
        f.set_param(0, 0.0);
        silence(&mut f, 9600);
        let mut io = sine(100.0, 24000, 0.4);
        run(&mut f, &mut io);
        assert!(rms(&io[9600..]) < 0.02, "DJ hard left must highpass 100 Hz away");
    }

    #[test]
    fn filter_silence_in_silence_out() {
        let mut f = Filter::new(SR);
        let mut io = vec![0.0; 4800 * 2];
        run(&mut f, &mut io);
        assert!(io.iter().all(|&x| x == 0.0));
    }

    // -- Limiter --------------------------------------------------------------

    #[test]
    fn limiter_ceiling_holds() {
        let mut lim = Limiter::new(SR);
        lim.set_param(0, 0.0); // -12 dB ceiling
        lim.set_param(1, 1.0); // +24 dB drive
        silence(&mut lim, 4800);
        let mut io = sine(1000.0, 48000, 0.5);
        run(&mut lim, &mut io);
        assert!(all_finite(&io));
        let ceiling = 10.0f32.powf(-12.0 / 20.0);
        assert!(peak(&io[4800..]) <= ceiling * 1.02, "peak {} must stay under {ceiling}", peak(&io[4800..]));
    }

    #[test]
    fn limiter_latency_and_transparency() {
        let mut lim = Limiter::new(SR);
        let d = lim.latency_frames();
        assert!(d > 0);
        // Below ceiling: exact passthrough, delayed by the lookahead.
        let mut io = sine(1000.0, 4800, 0.3);
        let before = io.clone();
        run(&mut lim, &mut io);
        for f in d..4800 {
            assert!((io[2 * f] - before[2 * (f - d)]).abs() < 1e-6);
        }
        assert!(io[..2 * d].iter().all(|&x| x == 0.0), "lookahead must delay");
        // Impulse lands exactly d frames later.
        let mut lim = Limiter::new(SR);
        let mut io = vec![0.0; 2048 * 2];
        io[0] = 0.8;
        io[1] = 0.8;
        run(&mut lim, &mut io);
        assert!((io[2 * d] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn limiter_silence_in_silence_out() {
        let mut lim = Limiter::new(SR);
        let mut io = vec![0.0; 4800 * 2];
        run(&mut lim, &mut io);
        assert!(io.iter().all(|&x| x == 0.0));
    }

    // -- Saturation -------------------------------------------------------------

    #[test]
    fn saturation_transfer_and_bounds() {
        let mut s = Saturation::new(SR);
        s.set_param(0, 0.5); // drive = sqrt(40)
        s.set_param(1, 1.0); // full wet
        silence(&mut s, 4800);
        let drive = 40.0f32.powf(0.5);
        let mut io = vec![0.0; 6 * 2];
        for (i, x) in [-2.0f32, -0.5, -0.1, 0.1, 0.5, 2.0].iter().enumerate() {
            io[2 * i] = *x;
            io[2 * i + 1] = *x;
        }
        run(&mut s, &mut io);
        for (i, x) in [-2.0f32, -0.5, -0.1, 0.1, 0.5, 2.0].iter().enumerate() {
            let want = (x * drive).tanh();
            assert!((io[2 * i] - want).abs() < 1e-3, "tanh transfer at {x}");
        }
        // Odd symmetry + bounded output on hot noise.
        let mut io = noise(48000, 4.0);
        s.set_param(0, 1.0);
        silence(&mut s, 4800);
        run(&mut s, &mut io);
        assert!(all_finite(&io));
        assert!(peak(&io) <= 1.0 + 1e-4, "full-wet tanh is bounded to ±1");
    }

    #[test]
    fn saturation_mix_zero_passthrough() {
        let mut s = Saturation::new(SR);
        s.set_param(1, 0.0);
        silence(&mut s, 4800);
        let mut io = noise(4800, 0.5);
        let before = io.clone();
        run(&mut s, &mut io);
        for (a, b) in io.iter().zip(before.iter()) {
            assert!((a - b).abs() < 1e-4);
        }
    }

    // -- Delay ----------------------------------------------------------------

    /// Settle a delay at 100 ms, feedback 0, fully wet.
    fn delay_100ms() -> Delay {
        let mut d = Delay::new(SR);
        d.set_param(0, log_norm(0.100, DELAY_MIN_S, DELAY_MAX_S));
        d.set_param(1, 0.0);
        d.set_param(2, 1.0);
        silence(&mut d, 48000);
        d
    }

    #[test]
    fn delay_echo_timing() {
        let mut d = delay_100ms();
        let mut io = vec![0.0; 12000 * 2];
        io[0] = 1.0;
        io[1] = 1.0;
        run(&mut d, &mut io);
        let want = (SR * 0.100) as usize; // 4800
        let l: Vec<f32> = io.iter().step_by(2).copied().collect();
        let argmax = l.iter().enumerate().max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap()).unwrap().0;
        assert!((argmax as i64 - want as i64).abs() <= 2, "echo at {argmax}, want {want}");
        assert!((l[argmax] - 1.0).abs() < 0.02);
    }

    #[test]
    fn delay_feedback_decay() {
        let mut d = Delay::new(SR);
        d.set_param(0, log_norm(0.100, DELAY_MIN_S, DELAY_MAX_S));
        d.set_param(1, 0.5); // fb = 0.475
        d.set_param(2, 1.0);
        d.set_param(4, 0.0); // bright
        silence(&mut d, 48000);
        let mut io = vec![0.0; 30000 * 2];
        io[0] = 1.0;
        io[1] = 1.0;
        run(&mut d, &mut io);
        let want = (SR * 0.100) as usize;
        let win = |center: usize| -> f32 {
            let mut m = 0.0f32;
            for f in center - 3..center + 4 {
                m = m.max(io[2 * f].abs());
            }
            m
        };
        let e1 = win(want);
        let e2 = win(2 * want);
        let e3 = win(3 * want);
        assert!(e1 > 0.8, "first echo ~unity, got {e1}");
        let ratio = e2 / e1;
        assert!(ratio > 0.25 && ratio < 0.6, "feedback ratio {ratio} should be ~0.475");
        assert!(e3 < e2, "echoes must decay");
    }

    #[test]
    fn delay_mix_zero_passthrough() {
        let mut d = Delay::new(SR);
        d.set_param(2, 0.0);
        silence(&mut d, 9600);
        let mut io = noise(4800, 0.5);
        let before = io.clone();
        run(&mut d, &mut io);
        for (a, b) in io.iter().zip(before.iter()) {
            assert!((a - b).abs() < 1e-5);
        }
    }

    #[test]
    fn delay_pingpong_alternates() {
        let mut d = Delay::new(SR);
        d.set_param(0, log_norm(0.050, DELAY_MIN_S, DELAY_MAX_S));
        d.set_param(1, 0.6);
        d.set_param(2, 1.0);
        d.set_param(3, 1.0); // pingpong
        d.set_param(4, 0.0);
        silence(&mut d, 48000);
        let mut io = vec![0.0; 12000 * 2];
        io[0] = 1.0; // left-only impulse
        run(&mut d, &mut io);
        let step = (SR * 0.050) as usize; // 2400
        let chan_peak = |center: usize, ch: usize| -> f32 {
            let mut m = 0.0f32;
            for f in center - 3..center + 4 {
                m = m.max(io[2 * f + ch].abs());
            }
            m
        };
        assert!(chan_peak(step, 0) > 0.8, "echo 1 on left");
        assert!(chan_peak(step, 1) < 0.05);
        assert!(chan_peak(2 * step, 1) > 0.4, "echo 2 on right");
        assert!(chan_peak(2 * step, 0) < 0.05);
        assert!(chan_peak(3 * step, 0) > 0.2, "echo 3 back on left");
    }

    #[test]
    fn delay_tail_rings_out() {
        let mut d = Delay::new(SR);
        d.set_param(0, log_norm(0.100, DELAY_MIN_S, DELAY_MAX_S));
        d.set_param(1, 0.5);
        d.set_param(2, 1.0);
        silence(&mut d, 48000);
        let mut io = noise(4800, 0.5);
        run(&mut d, &mut io);
        // 4 s of silence: feedback 0.475 decays to nothing.
        let mut tail = vec![0.0; 192000 * 2];
        for chunk in tail.chunks_mut(8192) {
            run(&mut d, chunk);
        }
        assert!(peak(&tail[tail.len() - 4800..]) < 1e-5);
        assert!(all_finite(&tail));
    }

    // -- Reverb -----------------------------------------------------------------

    #[test]
    fn reverb_mix_zero_passthrough() {
        let mut rv = Reverb::new(SR);
        rv.set_param(2, 0.0);
        silence(&mut rv, 4800);
        let mut io = noise(4800, 0.5);
        let before = io.clone();
        run(&mut rv, &mut io);
        for (a, b) in io.iter().zip(before.iter()) {
            assert!((a - b).abs() < 1e-5);
        }
    }

    #[test]
    fn reverb_impulse_tail_decays() {
        let mut rv = Reverb::new(SR);
        rv.set_param(2, 1.0); // fully wet
        rv.set_param(0, 0.5);
        silence(&mut rv, 4800);
        let mut io = vec![0.0; 4800 * 2];
        io[0] = 1.0;
        io[1] = 1.0;
        run(&mut rv, &mut io);
        let early = rms(&io);
        assert!(early > 1e-4, "impulse must excite the tank");
        // 4 s of silence in chunks.
        let mut tail = vec![0.0; 192000 * 2];
        for chunk in tail.chunks_mut(8192) {
            run(&mut rv, chunk);
        }
        assert!(all_finite(&tail));
        let late = rms(&tail[tail.len() - 9600..]);
        assert!(late < early * 1e-3, "tail must decay: early {early}, late {late}");
        assert!(late < 1e-4);
    }

    #[test]
    fn reverb_bounded_on_hot_noise() {
        let mut rv = Reverb::new(SR);
        rv.set_param(0, 1.0); // max size
        rv.set_param(1, 0.0); // no damping (worst case)
        rv.set_param(2, 1.0);
        let mut io = noise(48000, 0.8);
        run(&mut rv, &mut io);
        assert!(all_finite(&io));
        assert!(peak(&io) < 32.0, "tank must stay bounded, got {}", peak(&io));
    }

    #[test]
    fn reverb_silence_in_silence_out() {
        let mut rv = Reverb::new(SR);
        let mut io = vec![0.0; 4800 * 2];
        run(&mut rv, &mut io);
        assert!(io.iter().all(|&x| x == 0.0));
    }

    // -- Factory / trait contract -------------------------------------------------

    #[test]
    fn factory_builds_all_kinds() {
        let kinds = [
            (DspKind::Gain, 1u32),
            (DspKind::Pan, 1),
            (DspKind::Eq3, 7),
            (DspKind::Filter, 3),
            (DspKind::Limiter, 3),
            (DspKind::Saturation, 3),
            (DspKind::Delay, 5),
            (DspKind::Reverb, 4),
            (DspKind::Metronome, 0),
            (DspKind::Bus, 1),
        ];
        for (kind, count) in kinds {
            let mut p = build_processor(&kind, SR);
            assert_eq!(p.param_count(), count, "{kind:?}");
            for id in 0..count {
                assert!(!p.param_name(id).is_empty());
                assert_ne!(p.param_name(id), "unknown");
                // Extremes and out-of-range values must be accepted safely.
                p.set_param(id, 0.0);
                p.set_param(id, 1.0);
                p.set_param(id, -5.0);
                p.set_param(id, 42.0);
            }
            p.set_param(999, 0.5); // unknown id: ignored, no panic
            assert_eq!(p.param_name(999), "unknown");
            let mut io = vec![0.0; 512 * 2];
            run(p.as_mut(), &mut io);
            assert!(io.iter().all(|&x| x == 0.0), "{kind:?}: silence in, silence out");
        }
    }

    #[test]
    fn process_handles_short_buffers_and_zero_frames() {
        let mut p = build_processor(&DspKind::Reverb, SR);
        let mut io = vec![0.0; 8];
        p.process(&mut io, 1000); // frames > buffer: must clamp, not panic
        p.process(&mut io, 0);
        p.process(&mut [], 0);
    }

    // -- RT proof: allocation counter ----------------------------------------------
    //
    // A thread-local counting allocator proves `process` and `set_param` never
    // allocate on the calling (audio) thread. Thread-local so parallel tests on
    // other threads cannot pollute the count; const-initialized so the counter
    // itself never allocates.

    mod rt_alloc {
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::cell::Cell;

        thread_local! {
            static COUNT: Cell<u64> = const { Cell::new(0) };
        }

        pub struct Counting;

        unsafe impl GlobalAlloc for Counting {
            unsafe fn alloc(&self, l: Layout) -> *mut u8 {
                COUNT.with(|c| c.set(c.get() + 1));
                unsafe { System.alloc(l) }
            }
            unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
                COUNT.with(|c| c.set(c.get() + 1));
                unsafe { System.realloc(p, l, n) }
            }
            unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
                unsafe { System.dealloc(p, l) }
            }
        }

        #[global_allocator]
        static A: Counting = Counting;

        pub fn count() -> u64 {
            COUNT.with(|c| c.get())
        }
    }

    #[test]
    fn process_and_set_param_are_allocation_free() {
        let kinds = [
            DspKind::Gain,
            DspKind::Pan,
            DspKind::Eq3,
            DspKind::Filter,
            DspKind::Limiter,
            DspKind::Saturation,
            DspKind::Delay,
            DspKind::Reverb,
            DspKind::Metronome,
            DspKind::Bus,
        ];
        // Everything (nodes + buffers) is built before the measurement window.
        let mut nodes: Vec<Box<dyn Processor>> =
            kinds.iter().map(|k| build_processor(k, SR)).collect();
        let io = noise(1024, 0.5);
        let mut scratch = vec![0.0f32; 1024 * 2];

        let before = rt_alloc::count();
        for block in 0..64 {
            for (i, p) in nodes.iter_mut().enumerate() {
                // Sweep every parameter while processing, like a live knob twist.
                for id in 0..p.param_count() {
                    p.set_param(id, ((block * 7 + i as u32 * 3 + id) % 100) as f32 / 100.0);
                }
                scratch.copy_from_slice(&io);
                p.process(&mut scratch, 1024);
            }
        }
        let after = rt_alloc::count();
        assert_eq!(after, before, "process/set_param allocated {} times on the audio thread", after - before);
        // Keep results alive so nothing is optimized away.
        assert!(scratch.iter().all(|x| x.is_finite()));
    }
}
