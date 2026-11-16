//! Master-bus loudness / peak / band metering (Bravo 1 "measure loop", PLAN §13).
//!
//! Fed with the final stereo master output every render block (the same place
//! as the recorder tap), it computes, allocation-free on the render path:
//!
//! * momentary (400 ms) and short-term (3 s) loudness in LUFS (ITU-R BS.1770-4,
//!   K-weighting: the two standard biquads),
//! * integrated LUFS since the last reset (gated: absolute -70 LU, relative
//!   -10 LU, via a fixed-size block ring — no allocation in render),
//! * true peak (dBTP) via 4x oversampling (a short polyphase FIR),
//! * a low-end (< 120 Hz) mono check: side/mid correlation,
//! * a 4-band energy summary (< 120, 120-500, 500-4k, > 4k Hz) in dB.
//!
//! RT discipline (rule 49 class): every buffer is preallocated in `new()`; the
//! per-block `process()` never allocates, locks, or logs. The single `Copy`
//! snapshot is published into `GraphPub` the same way `mix` is (rule 33).

use serde::{Deserialize, Serialize};

use std::f64::consts::PI;

/// Loudness-stable window sizes (seconds).
const MOMENTARY_SECS: f64 = 0.4;
const SHORT_TERM_SECS: f64 = 3.0;
/// Gating block length (BS.1770 uses 400 ms blocks).
const GATE_SECS: f64 = 0.4;
/// Absolute and relative gates (LUFS).
const ABS_GATE_LU: f64 = -70.0;
const REL_GATE_LU: f64 = -10.0;
/// LUFS offset in the BS.1770 loudness sum.
const LUFS_OFFSET: f64 = -0.691;

/// Floor for energies/peak reported in dB (a "silent" floor, never `-inf`).
const DB_FLOOR: f64 = -120.0;

/// Energy/history ring capacity: enough blocks for > 3 s even at tiny blocks.
const HIST_CAP: usize = 2048;
/// Gating-block ring capacity (2048 * 400 ms = ~13 minutes of integration).
const GATE_CAP: usize = 2048;

/// True-peak polyphase interpolator: 4 phases × 8 taps (32-tap prototype).
const TP_TAPS: usize = 8;

/// The numbers the agent reads for `measure_mix` / `get_status`. `Copy` so it
/// moves into `GraphPub` without clone machinery. LUFS/band values are `f32`
/// enough for ±0.1 LU resolution.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct MeterSnapshot {
    /// 400 ms loudness (LUFS).
    pub momentary_lufs: f32,
    /// 3 s loudness (LUFS).
    pub short_term_lufs: f32,
    /// Gated integrated loudness since the last reset (LUFS). `NaN` until at
    /// least one gating block clears the absolute gate.
    pub integrated_lufs: f32,
    /// True peak (dBTP, relative to full scale).
    pub true_peak_db: f32,
    /// Stereo correlation of the < 120 Hz band, -1..1.
    pub low_end_corr: f32,
    /// `true` = low-end is phase-compatible (mono-safe); `false` = the bass is
    /// out of phase (a mono-collapse risk).
    pub low_end_mono: bool,
    /// Mean-square energy per band (< 120, 120-500, 500-4k, > 4k Hz) in dB.
    pub band_db: [f32; 4],
}

impl Default for MeterSnapshot {
    fn default() -> Self {
        Self {
            momentary_lufs: f32::NAN,
            short_term_lufs: f32::NAN,
            integrated_lufs: f32::NAN,
            true_peak_db: DB_FLOOR as f32,
            low_end_corr: 1.0,
            low_end_mono: true,
            band_db: [DB_FLOOR as f32; 4],
        }
    }
}

/// A normalized biquad (a0 = 1), coefficients `[b0, b1, b2, a1, a2]`.
#[derive(Clone, Copy)]
struct Coef([f64; 5]);

/// Direct-form-2-transposed biquad state + fixed coefficients.
struct Biquad {
    c: Coef,
    z1: f64,
    z2: f64,
}

impl Biquad {
    fn new(c: Coef) -> Self {
        Self { c, z1: 0.0, z2: 0.0 }
    }

    #[inline]
    fn process(&mut self, x: f32) -> f64 {
        let x = x as f64;
        let c = &self.c.0;
        let y = c[0] * x + self.z1;
        self.z1 = c[1] * x - c[3] * y + self.z2;
        self.z2 = c[2] * x - c[4] * y;
        y
    }

    fn clear(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }
}

fn norm(b: [f64; 6]) -> Coef {
    let a0 = b[3];
    Coef([b[0] / a0, b[1] / a0, b[2] / a0, b[4] / a0, b[5] / a0])
}

// --- RBJ-cookbook bilinear designs (used for the band filters and the
// non-48 kHz K-weighting fallback; 48 kHz uses the canonical Table 4 values).

fn rbj_highpass(f0: f64, q: f64, fs: f64) -> Coef {
    let w0 = 2.0 * PI * f0 / fs;
    let alpha = w0.sin() / (2.0 * q);
    let k = (w0.cos() + 1.0) / 2.0;
    norm([k, -(w0.cos() + 1.0), k, 1.0 + alpha, -2.0 * w0.cos(), 1.0 - alpha])
}

fn rbj_lowpass(f0: f64, q: f64, fs: f64) -> Coef {
    let w0 = 2.0 * PI * f0 / fs;
    let alpha = w0.sin() / (2.0 * q);
    let k = (1.0 - w0.cos()) / 2.0;
    norm([k, 1.0 - w0.cos(), k, 1.0 + alpha, -2.0 * w0.cos(), 1.0 - alpha])
}

/// The BS.1770 K-weighting shelf stage (the "pre-filter", a +4 dB high-shelf).
/// `q` sets the transition shape.
fn k_shelf(f0: f64, db_gain: f64, q: f64, fs: f64) -> Coef {
    let a = 10f64.powf(db_gain / 40.0);
    let w0 = 2.0 * PI * f0 / fs;
    let cos = w0.cos();
    let alpha = w0.sin() / (2.0 * q);
    let sq = 2.0 * a.sqrt() * alpha;
    let b0 = a * ((a + 1.0) + (a - 1.0) * cos + sq);
    let b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cos);
    let b2 = a * ((a + 1.0) + (a - 1.0) * cos - sq);
    let a0 = (a + 1.0) - (a - 1.0) * cos + sq;
    let a1 = 2.0 * ((a - 1.0) - (a + 1.0) * cos);
    let a2 = (a + 1.0) - (a - 1.0) * cos - sq;
    norm([b0, b1, b2, a0, a1, a2])
}

/// The two K-weighting biquad coefficients for a sample rate. 48 kHz uses the
/// exact BS.1770-4 Table 4 coefficients; other rates re-derive the shelf +
/// Butterworth high-pass from the analog prototypes (bilinear transform).
fn k_weight_coeffs(fs: f64) -> (Coef, Coef) {
    if (fs - 48000.0).abs() < 1.0 {
        // BS.1770-4 Table 4 (48 kHz): pre-filter shelf, then the RLB high-pass.
        const SHELF: [f64; 5] = [
            1.535_124_859_586_97,
            -2.691_696_189_406_38,
            1.198_392_810_852_85,
            -1.690_659_293_182_41,
            0.732_480_774_215_85,
        ];
        const HP: [f64; 5] = [1.0, -2.0, 1.0, -1.990_047_454_833_98, 0.990_072_250_366_21];
        (Coef(SHELF), Coef(HP))
    } else {
        // Analog prototypes (libebur128 constants): high-shelf +4 dB above
        // 1681.97 Hz, Butterworth high-pass at 38.14 Hz.
        (
            k_shelf(1681.974_450_955_533, 4.0, 0.707_175_236_955_42, fs),
            rbj_highpass(38.135_470_876_024_44, 0.500_327_037_323_88, fs),
        )
    }
}

/// Per-block accumulators pushed into the history ring (all `f64` sums).
#[derive(Clone, Copy, Default)]
struct BlockStat {
    k_l: f64,   // Σ K-weighted L²
    k_r: f64,   // Σ K-weighted R²
    lo_ll: f64, // Σ lowpass(L)²
    lo_rr: f64, // Σ lowpass(R)²
    lo_lr: f64, // Σ lowpass(L)·lowpass(R)
    b1: f64,    // Σ band120-500² (mono)
    b2: f64,    // Σ band500-4k²  (mono)
    b3: f64,    // Σ band>4k²     (mono)
    n: u32,
}

impl BlockStat {
    const ZERO: Self = Self {
        k_l: 0.0,
        k_r: 0.0,
        lo_ll: 0.0,
        lo_rr: 0.0,
        lo_lr: 0.0,
        b1: 0.0,
        b2: 0.0,
        b3: 0.0,
        n: 0,
    };
}

/// 4x-oversampling true-peak detector: a Hann-windowed-sinc polyphase FIR.
struct TruePeak {
    phase: [[f64; TP_TAPS]; 4],
    hl: [f32; TP_TAPS],
    hr: [f32; TP_TAPS],
    /// Running max absolute interpolated value over the current block.
    peak: f64,
}

impl TruePeak {
    fn new() -> Self {
        // 32-tap prototype, low-pass at 0.25 of the 4x Nyquist, Hann window.
        // Interpolation gain ×4 keeps full-scale amplitude.
        let total = TP_TAPS * 4;
        let center = (total as f64 - 1.0) / 2.0;
        let fc = 0.25;
        let mut h = [0.0f64; 32];
        for (k, slot) in h.iter_mut().enumerate() {
            let d = k as f64 - center;
            let sinc = if d == 0.0 { 1.0 } else { (PI * d).sin() / (PI * d) };
            let win = 0.5 - 0.5 * (2.0 * PI * k as f64 / total as f64).cos();
            *slot = 4.0 * 2.0 * fc * sinc * win;
        }
        let mut phase = [[0.0f64; TP_TAPS]; 4];
        for p in 0..4 {
            for t in 0..TP_TAPS {
                phase[p][t] = h[p + 4 * t];
            }
        }
        Self {
            phase,
            hl: [0.0; TP_TAPS],
            hr: [0.0; TP_TAPS],
            peak: 0.0,
        }
    }

    /// Feed one mono-pair sample; track the block's oversampled peak.
    #[inline]
    fn push(&mut self, l: f32, r: f32) {
        for i in (1..TP_TAPS).rev() {
            self.hl[i] = self.hl[i - 1];
            self.hr[i] = self.hr[i - 1];
        }
        self.hl[0] = l;
        self.hr[0] = r;
        for p in 0..4 {
            let mut sl = 0.0f64;
            let mut sr = 0.0f64;
            for t in 0..TP_TAPS {
                sl += self.phase[p][t] * self.hl[t] as f64;
                sr += self.phase[p][t] * self.hr[t] as f64;
            }
            self.peak = self.peak.max(sl.abs()).max(sr.abs());
        }
    }

    fn take(&mut self) -> f64 {
        let v = self.peak;
        self.peak = 0.0;
        v
    }

    fn clear(&mut self) {
        self.hl = [0.0; TP_TAPS];
        self.hr = [0.0; TP_TAPS];
        self.peak = 0.0;
    }
}

/// The allocation-free master meter. Preallocates every buffer in `new()`.
pub struct MasterMeter {
    sample_rate: f64,
    // K-weighting (2 stages) per channel.
    k_l_shelf: Biquad,
    k_l_hp: Biquad,
    k_r_shelf: Biquad,
    k_r_hp: Biquad,
    // Stereo low-pass at 120 Hz (low-end mono check).
    lp_l: Biquad,
    lp_r: Biquad,
    // Mono band filters (band summary): cascaded 2nd-order crossovers so a tone
    // lands in exactly one band. band1 = HP120·LP500, band2 = HP500·LP4000,
    // band3 = HP4000.
    band1_hp: Biquad,
    band1_lp: Biquad,
    band2_hp: Biquad,
    band2_lp: Biquad,
    band3_hp: Biquad,
    // Energy/history ring.
    hist: Box<[BlockStat; HIST_CAP]>,
    hist_head: usize,
    hist_len: usize,
    // Gating-block ring (integrated loudness).
    gate: Box<[f64; GATE_CAP]>,
    gate_head: usize,
    gate_len: usize,
    gate_acc_l: f64,
    gate_acc_r: f64,
    gate_acc_n: u32,
    // True peak.
    true_peak: TruePeak,
    /// Most recent block's oversampled (true) peak, held for `snapshot()`.
    true_peak_block: f64,
}

impl MasterMeter {
    pub fn new(sample_rate: f64) -> Self {
        let (shelf, hp) = k_weight_coeffs(sample_rate);
        let lo_coef = rbj_lowpass(120.0, 0.7071, sample_rate);
        let q = 0.7071;
        Self {
            sample_rate,
            k_l_shelf: Biquad::new(shelf),
            k_l_hp: Biquad::new(hp),
            k_r_shelf: Biquad::new(shelf),
            k_r_hp: Biquad::new(hp),
            lp_l: Biquad::new(lo_coef),
            lp_r: Biquad::new(lo_coef),
            band1_hp: Biquad::new(rbj_highpass(120.0, q, sample_rate)),
            band1_lp: Biquad::new(rbj_lowpass(500.0, q, sample_rate)),
            band2_hp: Biquad::new(rbj_highpass(500.0, q, sample_rate)),
            band2_lp: Biquad::new(rbj_lowpass(4000.0, q, sample_rate)),
            band3_hp: Biquad::new(rbj_highpass(4000.0, q, sample_rate)),
            hist: Box::new([BlockStat::ZERO; HIST_CAP]),
            hist_head: 0,
            hist_len: 0,
            gate: Box::new([-1.0e6; GATE_CAP]),
            gate_head: 0,
            gate_len: 0,
            gate_acc_l: 0.0,
            gate_acc_r: 0.0,
            gate_acc_n: 0,
            true_peak: TruePeak::new(),
            true_peak_block: 0.0,
        }
    }

    /// Feed one block of interleaved stereo output (channel count `ch` ≥ 1).
    pub fn process(&mut self, data: &[f32], ch: usize) {
        let ch = ch.max(1);
        let frames = data.len() / ch;
        let mut acc = BlockStat::ZERO;
        // Mono band filters on a single input (use L; report mono energy).
        for i in 0..frames {
            let l = data[i * ch];
            let r = if ch > 1 { data[i * ch + 1] } else { l };
            let k_l = self.k_l_hp.process(self.k_l_shelf.process(l) as f32);
            let k_r = self.k_r_hp.process(self.k_r_shelf.process(r) as f32);
            let lp_l = self.lp_l.process(l);
            let lp_r = self.lp_r.process(r);
            let m = (l as f64 + r as f64) * 0.5;
            let bb1 = self.band1_lp.process(self.band1_hp.process(m as f32) as f32);
            let bb2 = self.band2_lp.process(self.band2_hp.process(m as f32) as f32);
            let bb3 = self.band3_hp.process(m as f32);
            acc.k_l += k_l * k_l;
            acc.k_r += k_r * k_r;
            acc.lo_ll += lp_l * lp_l;
            acc.lo_rr += lp_r * lp_r;
            acc.lo_lr += lp_l * lp_r;
            acc.b1 += bb1 * bb1;
            acc.b2 += bb2 * bb2;
            acc.b3 += bb3 * bb3;
            acc.n += 1;
            self.true_peak.push(l, r);
        }
        if acc.n == 0 {
            return;
        }
        self.push_block(acc);
        self.true_peak_block = self.true_peak.take();
        // Feed the gating accumulator; close a 400 ms block when full.
        self.gate_acc_l += acc.k_l;
        self.gate_acc_r += acc.k_r;
        self.gate_acc_n += acc.n;
        let gate_samples = (GATE_SECS * self.sample_rate) as u32;
        if self.gate_acc_n >= gate_samples {
            let n = self.gate_acc_n as f64;
            let lufs = loudness_2ch(self.gate_acc_l / n, self.gate_acc_r / n);
            self.push_gate(lufs);
            self.gate_acc_l = 0.0;
            self.gate_acc_r = 0.0;
            self.gate_acc_n = 0;
        }
    }

    /// Advance the meter a block of silence (no master wired, or `ch == 0`).
    pub fn process_silence(&mut self, frames: usize) {
        let mut acc = BlockStat::ZERO; // all-zero energy still occupies the window
        acc.n = frames as u32;
        for _ in 0..frames {
            self.true_peak.push(0.0, 0.0);
        }
        self.true_peak_block = self.true_peak.take();
        // A silent block still occupies the loudness windows, so the meters
        // decay to silence correctly.
        self.gate_acc_n += acc.n;
        let gate_samples = (GATE_SECS * self.sample_rate) as u32;
        if self.gate_acc_n >= gate_samples {
            self.push_gate(loudness_2ch(0.0, 0.0));
            self.gate_acc_l = 0.0;
            self.gate_acc_r = 0.0;
            self.gate_acc_n = 0;
        }
        self.push_block(acc);
    }

    fn push_block(&mut self, s: BlockStat) {
        self.hist[self.hist_head] = s;
        self.hist_head = (self.hist_head + 1) % HIST_CAP;
        if self.hist_len < HIST_CAP {
            self.hist_len += 1;
        }
    }

    fn push_gate(&mut self, lufs: f64) {
        self.gate[self.gate_head] = lufs;
        self.gate_head = (self.gate_head + 1) % GATE_CAP;
        if self.gate_len < GATE_CAP {
            self.gate_len += 1;
        }
    }

    /// Loudness (LUFS) over the trailing `secs` seconds.
    fn window_lufs(&self, secs: f64) -> f64 {
        let target = (secs * self.sample_rate) as f64;
        let mut k_l = 0.0f64;
        let mut k_r = 0.0f64;
        let mut n = 0u32;
        // Walk newest → oldest until we hold `target` samples.
        for step in 0..self.hist_len {
            let idx = (self.hist_head + HIST_CAP - 1 - step) % HIST_CAP;
            let s = &self.hist[idx];
            k_l += s.k_l;
            k_r += s.k_r;
            n += s.n;
            if n as f64 >= target {
                break;
            }
        }
        loudness_2ch(k_l / n.max(1) as f64, k_r / n.max(1) as f64)
    }

    /// Gated integrated loudness (NaN = not enough above-gate data).
    pub fn integrated_lufs(&self) -> f64 {
        let mut sum = 0.0f64;
        let mut n = 0usize;
        for step in 0..self.gate_len {
            let idx = (self.gate_head + GATE_CAP - 1 - step) % GATE_CAP;
            let l = self.gate[idx];
            if l > ABS_GATE_LU {
                sum += l;
                n += 1;
            }
        }
        if n == 0 {
            return f64::NAN;
        }
        let rel = sum / n as f64 + REL_GATE_LU;
        let mut gsum = 0.0f64;
        let mut gn = 0usize;
        for step in 0..self.gate_len {
            let idx = (self.gate_head + GATE_CAP - 1 - step) % GATE_CAP;
            let l = self.gate[idx];
            if l > ABS_GATE_LU && l > rel {
                gsum += l;
                gn += 1;
            }
        }
        if gn == 0 {
            f64::NAN
        } else {
            gsum / gn as f64
        }
    }

    /// Low-end (< 120 Hz) stereo correlation from a short trailing window.
    fn low_end(&self) -> (f64, bool) {
        let target = (MOMENTARY_SECS * self.sample_rate) as f64;
        let (mut ll, mut rr, mut lr) = (0.0f64, 0.0f64, 0.0f64);
        let mut n = 0usize;
        for step in 0..self.hist_len {
            let idx = (self.hist_head + HIST_CAP - 1 - step) % HIST_CAP;
            let s = &self.hist[idx];
            ll += s.lo_ll;
            rr += s.lo_rr;
            lr += s.lo_lr;
            n += s.n as usize;
            if n as f64 >= target {
                break;
            }
        }
        let denom = (ll * rr).sqrt();
        if denom < 1e-12 {
            return (1.0, true); // no low-end energy: trivially mono-safe
        }
        let corr = (lr / denom).clamp(-1.0, 1.0);
        (corr, corr > -0.5)
    }

    /// Band energies over the short-term window, in dB (mean-square ref).
    fn bands(&self) -> [f64; 4] {
        let target = (SHORT_TERM_SECS * self.sample_rate) as f64;
        let (mut ll, mut rr, mut lr, mut b1, mut b2, mut b3) =
            (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
        let mut n = 0usize;
        for step in 0..self.hist_len {
            let idx = (self.hist_head + HIST_CAP - 1 - step) % HIST_CAP;
            let s = &self.hist[idx];
            ll += s.lo_ll;
            rr += s.lo_rr;
            lr += s.lo_lr;
            b1 += s.b1;
            b2 += s.b2;
            b3 += s.b3;
            n += s.n as usize;
            if n as f64 >= target {
                break;
            }
        }
        let n = n.max(1) as f64;
        let band0 = 0.25 * (ll + rr + 2.0 * lr) / n; // mono of the stereo low-pass
        [
            to_db(band0),
            to_db(b1 / n),
            to_db(b2 / n),
            to_db(b3 / n),
        ]
    }

    /// The current snapshot (Copy; pushed into `GraphPub` each block).
    pub fn snapshot(&self) -> MeterSnapshot {
        let (corr, mono) = self.low_end();
        let bands = self.bands();
        MeterSnapshot {
            momentary_lufs: self.window_lufs(MOMENTARY_SECS) as f32,
            short_term_lufs: self.window_lufs(SHORT_TERM_SECS) as f32,
            integrated_lufs: self.integrated_lufs() as f32,
            true_peak_db: to_db(self.true_peak_block) as f32,
            low_end_corr: corr as f32,
            low_end_mono: mono,
            band_db: [
                bands[0] as f32,
                bands[1] as f32,
                bands[2] as f32,
                bands[3] as f32,
            ],
        }
    }

    /// Restart integration (clear all windows/gates/state) — `ResetMeter`.
    pub fn reset(&mut self) {
        self.k_l_shelf.clear();
        self.k_l_hp.clear();
        self.k_r_shelf.clear();
        self.k_r_hp.clear();
        self.lp_l.clear();
        self.lp_r.clear();
        self.band1_hp.clear();
        self.band1_lp.clear();
        self.band2_hp.clear();
        self.band2_lp.clear();
        self.band3_hp.clear();
        self.hist = Box::new([BlockStat::ZERO; HIST_CAP]);
        self.hist_head = 0;
        self.hist_len = 0;
        self.gate = Box::new([-1.0e6; GATE_CAP]);
        self.gate_head = 0;
        self.gate_len = 0;
        self.gate_acc_l = 0.0;
        self.gate_acc_r = 0.0;
        self.gate_acc_n = 0;
        self.true_peak.clear();
        self.true_peak_block = 0.0;
    }
}

/// Stereo loudness (LUFS) from per-channel mean squares (already K-weighted).
#[inline]
fn loudness_2ch(ms_l: f64, ms_r: f64) -> f64 {
    let sum = ms_l + ms_r; // G_L = G_R = 1.0
    if sum <= 0.0 {
        return DB_FLOOR;
    }
    LUFS_OFFSET + 10.0 * sum.log10()
}

#[inline]
fn to_db(x: f64) -> f64 {
    if x <= 1e-12 {
        DB_FLOOR
    } else {
        10.0 * x.log10()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    const SR: f64 = 48000.0;

    /// Stereo interleaved sine at `freq` Hz, amplitude `amp` (full-scale = 1.0).
    fn sine(freq: f64, amp: f64, frames: usize, chan_phase: f64) -> Vec<f32> {
        let mut v = vec![0.0f32; frames * 2];
        for i in 0..frames {
            let t = i as f64 / SR;
            v[i * 2] = (amp * (TAU * freq * t).sin()) as f32;
            v[i * 2 + 1] = (amp * (TAU * freq * t + chan_phase).sin()) as f32;
        }
        v
    }

    fn feed_many(met: &mut MasterMeter, data: &[f32], ch: usize) {
        // Feed in typical 256-frame blocks.
        let frames = data.len() / ch;
        let mut start = 0;
        while start < frames {
            let end = (start + 256).min(frames);
            let blk = &data[start * ch..end * ch];
            met.process(blk, ch);
            start = end;
        }
    }

    #[test]
    fn k_weight_reproduces_bs1770_table_at_48k() {
        let (shelf, hp) = k_weight_coeffs(48000.0);
        let s = shelf.0.map(|x| (x * 1e6).round() as i64);
        assert_eq!(s, [1_535_125, -2_691_696, 1_198_393, -1_690_659, 732_481], "shelf coeffs");
        assert_eq!(hp.0, [1.0, -2.0, 1.0, -1.990_047_454_833_98, 0.990_072_250_366_21], "hp coeffs");
    }

    #[test]
    fn momentary_stereo_minus20_dbfs_997hz_reads_minus20_lufs() {
        // BS.1770: 0 dBFS 997 Hz mono = -3.01 LUFS; -20 dBFS both channels = -20.
        let mut met = MasterMeter::new(SR);
        let frames = (SR * 1.5) as usize; // well past 400 ms and a gate block
        let data = sine(997.0, 0.1, frames, 0.0);
        feed_many(&mut met, &data, 2);
        let snap = met.snapshot();
        assert!(
            (snap.momentary_lufs + 20.0).abs() < 0.3,
            "momentary {} != -20 (tolerance 0.3)",
            snap.momentary_lufs
        );
        assert!(
            (snap.short_term_lufs + 20.0).abs() < 0.3,
            "short-term {} != -20",
            snap.short_term_lufs
        );
        assert!(
            (snap.integrated_lufs + 20.0).abs() < 0.3,
            "integrated {} != -20",
            snap.integrated_lufs
        );
    }

    #[test]
    fn true_peak_exceeds_sample_peak_for_inter_sample_peak() {
        // fs/4 sine at 45°: sample peak 0.7071·A, true peak A.
        let mut met = MasterMeter::new(SR);
        let n = 1024;
        let a = 1.0f32;
        let mut data = vec![0.0f32; n * 2];
        for i in 0..n {
            let s = a * ((2.0 * std::f32::consts::PI * 12000.0 * i as f32 / SR as f32)
                + std::f32::consts::FRAC_PI_4)
                .sin();
            data[i * 2] = s;
            data[i * 2 + 1] = s;
        }
        met.process(&data, 2);
        let snap = met.snapshot();
        let sample_peak_db = 20.0 * (a * 0.707_106_78).log10(); // ≈ -3.01 dBFS
        assert!(
            snap.true_peak_db > sample_peak_db + 1.5,
            "true peak {} dBTP should clearly exceed sample peak {} dBFS",
            snap.true_peak_db,
            sample_peak_db
        );
    }

    #[test]
    fn gating_ignores_silence() {
        let mut met = MasterMeter::new(SR);
        // 2.0 s = 5 whole 400 ms gate blocks (no partial block at the boundary).
        let tone = sine(997.0, 0.1, (SR * 2.0) as usize, 0.0);
        feed_many(&mut met, &tone, 2);
        let before = met.integrated_lufs();
        assert!((before + 20.0).abs() < 0.3, "integrated {} != -20", before);
        // 0.4 s of silence = exactly one gate block, below the -70 LU absolute
        // gate: it must be excluded, not drag the integrated reading down.
        let silence = vec![0.0f32; (SR * 0.4) as usize * 2];
        feed_many(&mut met, &silence, 2);
        let after = met.integrated_lufs();
        assert!((before - after).abs() < 0.3, "integrated shifted by silence: {before} -> {after}");
        assert!((after + 20.0).abs() < 0.3, "integrated {} != -20", after);
    }

    #[test]
    fn band_summary_puts_60hz_in_band_zero() {
        let mut met = MasterMeter::new(SR);
        let data = sine(60.0, 0.2, (SR * 1.0) as usize, 0.0);
        feed_many(&mut met, &data, 2);
        let snap = met.snapshot();
        assert!(snap.band_db[0] > snap.band_db[1] + 8.0, "60 Hz must dominate band 0: {:?}", snap.band_db);
        assert!(snap.band_db[0] > snap.band_db[2] + 8.0, "{:?}", snap.band_db);
        assert!(snap.band_db[0] > snap.band_db[3] + 8.0, "{:?}", snap.band_db);
    }

    #[test]
    fn mono_check_flags_out_of_phase_bass() {
        let mut in_phase = MasterMeter::new(SR);
        let data = sine(60.0, 0.2, (SR * 1.0) as usize, 0.0); // L == R
        feed_many(&mut in_phase, &data, 2);
        assert!(in_phase.snapshot().low_end_mono, "in-phase bass must be mono-safe");

        let mut out_phase = MasterMeter::new(SR);
        let data = sine(60.0, 0.2, (SR * 1.0) as usize, std::f64::consts::PI);
        feed_many(&mut out_phase, &data, 2);
        let snap = out_phase.snapshot();
        assert!(!snap.low_end_mono, "out-of-phase bass must be flagged");
        assert!(snap.low_end_corr < -0.5, "corr {:?} should be strongly negative", snap.low_end_corr);
    }

    #[test]
    fn reset_restarts_integration() {
        let mut met = MasterMeter::new(SR);
        let tone = sine(997.0, 0.1, (SR * 1.0) as usize, 0.0);
        feed_many(&mut met, &tone, 2);
        assert!((met.integrated_lufs() + 20.0).abs() < 0.3);
        met.reset();
        assert!(met.integrated_lufs().is_nan(), "reset must clear the integration gate");
    }
}