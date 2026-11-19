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

/// True-peak polyphase interpolator: 4 phases × 12 taps (the 48-tap size
/// BS.1770 Annex 2 uses).
const TP_TAPS: usize = 12;

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
    /// True peak of the most recent block (dBTP, relative to full scale).
    pub true_peak_db: f32,
    /// Highest true peak since the last reset (dBTP).
    #[serde(default = "db_floor_f32")]
    pub true_peak_max_db: f32,
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
            true_peak_max_db: DB_FLOOR as f32,
            low_end_corr: 1.0,
            low_end_mono: true,
            band_db: [DB_FLOOR as f32; 4],
        }
    }
}

fn db_floor_f32() -> f32 {
    DB_FLOOR as f32
}

/// Named loudness targets (integrated LUFS): the one table the assistant
/// tool, MCP and the Master's menu read. Streaming services normalize to
/// these; "club" is a loud master for a PA.
pub const TARGET_PRESETS: &[(&str, &str, f32)] = &[
    ("spotify", "Spotify", -14.0),
    ("youtube", "YouTube", -14.0),
    ("tidal", "Tidal", -14.0),
    ("apple", "Apple Music", -16.0),
    ("club", "Club", -8.0),
];
/// Targets outside this range are clamped: below it is not a master level,
/// above it the −1 dBTP ceiling leaves no room.
pub const TARGET_RANGE_LUFS: (f32, f32) = (-36.0, -6.0);
/// The pre-limiter trim a target may apply, either way (dB).
pub const TARGET_GAIN_RANGE_DB: f32 = 24.0;
/// The least loudness per dB of trim (LU/dB) a match still follows: a step
/// is at most three times what is missing, and a raise that yielded less
/// than this means the limiter has nothing more to give.
const MIN_YIELD: f32 = 1.0 / 3.0;
/// The true-peak ceiling a target holds (dBTP).
pub const TRUE_PEAK_CEILING_DB: f32 = -1.0;
/// The true-peak guard never turns the output down further than this (dB).
pub const GUARD_FLOOR_DB: f32 = -12.0;

/// The master's loudness target and what it did (#12). `Copy`, published in
/// `GraphPub` like the meter and saved with a session.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct MasterTarget {
    /// Target integrated loudness; `None` = no target, the chain is as before.
    pub lufs: Option<f32>,
    /// Trim in front of the master limiter (dB).
    pub gain_db: f32,
    /// Integrated loudness of the mix without the trim; `None` until sound
    /// was measured.
    pub before_lufs: Option<f32>,
    /// How far the true-peak guard turned the output down (dB, ≤ 0).
    #[serde(default)]
    pub guard_db: f32,
    /// The trim and the loudness measured under it at the previous match:
    /// with the current pair it tells how much of an added dB the limiter
    /// lets through, which sizes the next step.
    #[serde(default)]
    pub last_point: Option<(f32, f32)>,
    /// The last match found the limiter giving back almost nothing for more
    /// trim: this mix is as loud as the chain lets it get, the target is out
    /// of reach at this master VOLUME.
    #[serde(default)]
    pub limited: bool,
}

impl Default for MasterTarget {
    fn default() -> Self {
        Self::OFF
    }
}

impl MasterTarget {
    pub const OFF: Self = Self { lufs: None, gain_db: 0.0, before_lufs: None, guard_db: 0.0, last_point: None, limited: false };

    /// The state after aiming at `lufs`, given the integrated loudness
    /// `measured` under the current trim (`None` = no sound measured since
    /// the trim last changed). The trim moves by what is missing, so a second
    /// call corrects what the limiter took off the first one; "before" stays
    /// the loudness of the untrimmed mix.
    ///
    /// Into the limiter a dB of trim is less than a LU of loudness (a loud
    /// target measured 0.4 LU per dB), so a step is divided by the yield the
    /// previous step had. Without that, "club" was still 1.3 LU short after
    /// four matches.
    pub fn matched(self, lufs: f32, measured: Option<f32>) -> Self {
        let lufs = lufs.clamp(TARGET_RANGE_LUFS.0, TARGET_RANGE_LUFS.1);
        let (gain_db, before_lufs, last_point, limited) = match (measured, self.before_lufs) {
            (Some(m), before) => {
                let before = before.or(Some(m - self.gain_db));
                let here = Some((self.gain_db, m));
                match self.step_yield(m) {
                    // More trim would only be more limiting: stay, and keep
                    // the earlier point so a louder mix (VOLUME up) is seen.
                    Some(y) if y < MIN_YIELD && lufs > m + 0.5 => (self.gain_db, before, self.last_point, true),
                    y => {
                        let lu_per_db = y.unwrap_or(1.0).clamp(MIN_YIELD, 1.0);
                        (self.gain_db + (lufs - m) / lu_per_db, before, here, false)
                    }
                }
            }
            (None, Some(before)) => (lufs - before, Some(before), None, false),
            (None, None) => (0.0, None, None, false),
        };
        Self {
            lufs: Some(lufs),
            gain_db: gain_db.clamp(-TARGET_GAIN_RANGE_DB, TARGET_GAIN_RANGE_DB),
            before_lufs,
            // The guard stays: it is part of what was measured, so the trim
            // makes up for it and repeated matches converge.
            guard_db: self.guard_db,
            last_point,
            limited,
        }
    }

    /// Loudness gained per dB of trim since the previous match (LU/dB), with
    /// `m` measured under the current trim. `None` when the trim was not
    /// raised by at least 1 dB since, or when the loudness FELL although the
    /// trim rose: then the playing changed, and that says nothing about the
    /// limiter.
    fn step_yield(&self, m: f32) -> Option<f32> {
        let (gain, loudness) = self.last_point?;
        let step = self.gain_db - gain;
        if step < 1.0 || m < loudness - 0.5 {
            return None;
        }
        Some((m - loudness) / step)
    }

    /// The trim cannot go further up and the target is still not reached:
    /// the limiter and the master volume cap this mix below it.
    pub fn at_limit(&self) -> bool {
        self.limited || self.gain_db >= TARGET_GAIN_RANGE_DB
    }

    /// Linear factors of the trim (before the limiter) and the guard (after
    /// the chain).
    pub fn gains(&self) -> (f32, f32) {
        (db_amp(self.gain_db), db_amp(self.guard_db))
    }

    /// The preset names that mean this target ("spotify/youtube/tidal").
    pub fn preset_names(&self) -> Option<String> {
        let lufs = self.lufs?;
        let names: Vec<&str> = TARGET_PRESETS.iter().filter(|p| p.2 == lufs).map(|p| p.0).collect();
        (!names.is_empty()).then(|| names.join("/"))
    }

    /// The before/after readout every frontend shows (tool results,
    /// `get_status`, the Master's menu): the target, the trim, the loudness
    /// before, and what `meter` measured since the trim was set.
    pub fn readout(&self, meter: &MeterSnapshot) -> serde_json::Value {
        // Rounded as f64: an f32 tenth prints as -14.199999809.
        let round = |v: f32| (v as f64 * 10.0).round() / 10.0;
        let Some(lufs) = self.lufs else {
            return serde_json::json!({
                "target_lufs": null,
                "status": "no loudness target: the master chain is unchanged",
                "presets": preset_list(),
            });
        };
        let after = meter.integrated_lufs.is_finite().then_some(meter.integrated_lufs);
        let tp = (meter.true_peak_max_db > DB_FLOOR as f32).then_some(meter.true_peak_max_db);
        let status = match (self.before_lufs, after) {
            (None, _) => format!(
                "target {lufs:.1} LUFS is stored, but no sound was measured yet: play, then set the target again"
            ),
            (Some(b), None) => format!(
                "trim {:+.1} dB set from {b:.1} LUFS; play to measure the result, then read it with get_master_target",
                self.gain_db
            ),
            (Some(b), Some(a)) if (a - lufs).abs() <= 0.5 => format!("on target: {b:.1} LUFS before, {a:.1} LUFS now"),
            (Some(b), Some(a)) if self.at_limit() && a < lufs => format!(
                "{b:.1} LUFS before, {a:.1} LUFS now: this mix does not get louder than that under the master limiter at the current master VOLUME; more trim would only add distortion. Raise the Master's VOLUME, then set the target again"
            ),
            (Some(b), Some(a)) => format!(
                "{b:.1} LUFS before, {a:.1} LUFS now, {:+.1} LU off the target: set the target again to refine",
                a - lufs
            ),
        };
        serde_json::json!({
            "target_lufs": lufs,
            "preset": self.preset_names(),
            "gain_db": round(self.gain_db),
            "before_lufs": self.before_lufs.map(round),
            "after_lufs": after.map(round),
            "true_peak_dbtp": tp.map(round),
            "true_peak_ceiling_dbtp": TRUE_PEAK_CEILING_DB,
            "true_peak_ok": tp.map(|t| t <= TRUE_PEAK_CEILING_DB + 0.05),
            "guard_db": round(self.guard_db),
            "status": status,
        })
    }
}

fn preset_list() -> String {
    let mut out: Vec<String> = TARGET_PRESETS.iter().map(|p| format!("{} {}", p.0, p.2)).collect();
    out.push("or lufs=<number>".into());
    out.join(", ")
}

/// What a `set_master_target` call asks for: a preset name, "off", or a
/// number. `Ok(None)` = turn the target off.
pub fn parse_target(preset: Option<&str>, lufs: Option<f64>) -> Result<Option<f32>, String> {
    if let Some(l) = lufs {
        if !l.is_finite() {
            return Err("lufs must be a number".into());
        }
        let (lo, hi) = TARGET_RANGE_LUFS;
        if (l as f32) < lo || (l as f32) > hi {
            return Err(format!("lufs {l} is outside {lo}..{hi} LUFS"));
        }
        return Ok(Some(l as f32));
    }
    let Some(name) = preset else {
        return Err(format!("give target (a preset or off) or lufs: {}", preset_list()));
    };
    let name = name.trim().to_lowercase();
    if matches!(name.as_str(), "off" | "none" | "") {
        return Ok(None);
    }
    if let Some(p) = TARGET_PRESETS.iter().find(|p| name == p.0 || name == p.1.to_lowercase()) {
        return Ok(Some(p.2));
    }
    // A model may send the number as text ("-14", "-14 LUFS").
    if let Ok(l) = name.trim_end_matches("lufs").trim().replace('\u{2212}', "-").parse::<f64>() {
        return parse_target(None, Some(l));
    }
    Err(format!("unknown target '{name}': {}", preset_list()))
}

/// Linear amplitude of a dB value.
#[inline]
pub fn db_amp(db: f32) -> f32 {
    10f32.powf(db / 20.0)
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
        // Windowed-sinc prototype at the 4x rate with its cutoff at the
        // ORIGINAL Nyquist (sinc(d / 4)), split into 4 phases. Each phase is
        // normalized to unity gain, so a tone reads its own amplitude. The
        // first version used sinc(d): no interpolator at all, a half-scale
        // sine read 0.62.
        const TOTAL: usize = TP_TAPS * 4;
        let center = (TOTAL as f64 - 1.0) / 2.0;
        let mut h = [0.0f64; TOTAL];
        for (k, slot) in h.iter_mut().enumerate() {
            let x = PI * (k as f64 - center) / 4.0;
            let sinc = if x == 0.0 { 1.0 } else { x.sin() / x };
            let win = 0.5 - 0.5 * (2.0 * PI * k as f64 / (TOTAL as f64 - 1.0)).cos();
            *slot = sinc * win;
        }
        let mut phase = [[0.0f64; TP_TAPS]; 4];
        for (p, taps) in phase.iter_mut().enumerate() {
            for (t, tap) in taps.iter_mut().enumerate() {
                *tap = h[p + 4 * t];
            }
            let sum: f64 = taps.iter().sum();
            for tap in taps.iter_mut() {
                *tap /= sum;
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
    // Gating-block ring (integrated loudness): each 400 ms block's summed
    // K-weighted mean square. BS.1770 averages energy, never dB values.
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
    /// Highest block peak since the last reset.
    true_peak_max: f64,
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
            gate: Box::new([0.0; GATE_CAP]),
            gate_head: 0,
            gate_len: 0,
            gate_acc_l: 0.0,
            gate_acc_r: 0.0,
            gate_acc_n: 0,
            true_peak: TruePeak::new(),
            true_peak_block: 0.0,
            true_peak_max: 0.0,
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
        self.true_peak_max = self.true_peak_max.max(self.true_peak_block);
        // Feed the gating accumulator; close a 400 ms block when full.
        self.gate_acc_l += acc.k_l;
        self.gate_acc_r += acc.k_r;
        self.gate_acc_n += acc.n;
        let gate_samples = (GATE_SECS * self.sample_rate) as u32;
        if self.gate_acc_n >= gate_samples {
            let n = self.gate_acc_n as f64;
            self.push_gate((self.gate_acc_l + self.gate_acc_r) / n);
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
            self.push_gate((self.gate_acc_l + self.gate_acc_r) / self.gate_acc_n as f64);
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

    fn push_gate(&mut self, energy: f64) {
        self.gate[self.gate_head] = energy;
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

    /// Gated integrated loudness (NaN = not enough above-gate data): the
    /// mean ENERGY of the gating blocks above the absolute gate and above
    /// the relative gate (10 LU under their mean), as BS.1770 defines it.
    /// Averaging the blocks' LUFS values instead read dynamic material low
    /// (a loud and a 10 LU quieter block: −5.0 instead of −2.6 LU re loud).
    pub fn integrated_lufs(&self) -> f64 {
        let abs_gate = 10f64.powf((ABS_GATE_LU - LUFS_OFFSET) / 10.0);
        let blocks = &self.gate[..self.gate_len];
        let mean_above = |floor: f64| {
            let (mut sum, mut n) = (0.0f64, 0usize);
            for &z in blocks {
                if z > abs_gate && z > floor {
                    sum += z;
                    n += 1;
                }
            }
            (n > 0).then(|| sum / n as f64)
        };
        let Some(ungated) = mean_above(0.0) else { return f64::NAN };
        let rel_gate = ungated * 10f64.powf(REL_GATE_LU / 10.0);
        match mean_above(rel_gate) {
            Some(z) => LUFS_OFFSET + 10.0 * z.log10(),
            None => f64::NAN,
        }
    }

    /// Seconds of gating blocks the integrated loudness currently rests on.
    pub fn integrated_secs(&self) -> f64 {
        self.gate_len as f64 * GATE_SECS
    }

    /// The most recent block's true peak as a linear amplitude.
    pub fn block_true_peak(&self) -> f64 {
        self.true_peak_block
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
            true_peak_db: amp_db(self.true_peak_block) as f32,
            true_peak_max_db: amp_db(self.true_peak_max) as f32,
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
    /// Runs on the audio thread: clears in place, never allocates.
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
        self.hist.fill(BlockStat::ZERO);
        self.hist_head = 0;
        self.hist_len = 0;
        self.gate.fill(0.0);
        self.gate_head = 0;
        self.gate_len = 0;
        self.gate_acc_l = 0.0;
        self.gate_acc_r = 0.0;
        self.gate_acc_n = 0;
        self.true_peak.clear();
        self.true_peak_block = 0.0;
        self.true_peak_max = 0.0;
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

/// dB of a linear AMPLITUDE (a peak): 20·log10. `to_db` is for mean squares.
#[inline]
fn amp_db(x: f64) -> f64 {
    if x <= 1e-6 {
        DB_FLOOR
    } else {
        20.0 * x.log10()
    }
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
    fn integrated_loudness_averages_energy_not_db() {
        // 2 s at −20 LUFS, then 2 s at −30: the mean energy is
        // 10·log10((1 + 0.1) / 2) = −2.6 LU under the loud half. Averaging
        // the blocks' dB values read −25.
        let mut met = MasterMeter::new(SR);
        feed_many(&mut met, &sine(997.0, 0.1, (SR * 2.0) as usize, 0.0), 2);
        feed_many(&mut met, &sine(997.0, 0.1 * 10f64.powf(-0.5), (SR * 2.0) as usize, 0.0), 2);
        let i = met.integrated_lufs();
        assert!((i + 22.6).abs() < 0.3, "integrated {i} != -22.6");
    }

    #[test]
    fn true_peak_of_a_half_scale_sine_reads_minus_6_dbtp() {
        // A peak is an amplitude: 20·log10. It was 10·log10 (−3 for 0.5).
        let mut met = MasterMeter::new(SR);
        feed_many(&mut met, &sine(997.0, 0.5, (SR * 0.5) as usize, 0.0), 2);
        let snap = met.snapshot();
        assert!((snap.true_peak_db + 6.02).abs() < 0.2, "block true peak {}", snap.true_peak_db);
        assert!((snap.true_peak_max_db + 6.02).abs() < 0.2, "held true peak {}", snap.true_peak_max_db);
        // The hold keeps the highest peak through a quieter passage.
        feed_many(&mut met, &sine(997.0, 0.05, (SR * 0.5) as usize, 0.0), 2);
        let snap = met.snapshot();
        assert!(snap.true_peak_db < -20.0 && (snap.true_peak_max_db + 6.02).abs() < 0.2, "{snap:?}");
        met.reset();
        assert!(met.snapshot().true_peak_max_db <= -119.0, "reset clears the hold");
    }

    #[test]
    fn a_target_trims_by_what_is_missing_and_keeps_the_before() {
        let near = |a: f32, b: f32| (a - b).abs() < 1e-4;
        // First aim: the whole difference, and "before" is what was measured.
        let t = MasterTarget::OFF.matched(-14.0, Some(-20.0));
        assert_eq!((t.lufs, t.gain_db, t.before_lufs), (Some(-14.0), 6.0, Some(-20.0)));
        // The limiter let 4 of the 6 dB through: the 2 LU still missing take
        // 3 dB at that yield. "Before" stays the untrimmed mix.
        let t2 = t.matched(-14.0, Some(-16.0));
        assert!(near(t2.gain_db, 9.0) && t2.before_lufs == Some(-20.0), "{t2:?}");
        // On target: nothing moves.
        let t3 = t2.matched(-14.0, Some(-14.0));
        assert!(near(t3.gain_db, 9.0), "{t3:?}");
        // 6 dB of trim made it 1 LU louder: the limiter is the ceiling. No
        // more trim, and the readout says why.
        let stuck = t.matched(-14.0, Some(-19.0));
        assert!(near(stuck.gain_db, 6.0) && stuck.limited && stuck.at_limit(), "{stuck:?}");
        let mut m = MeterSnapshot::default();
        m.integrated_lufs = -19.0;
        assert!(stuck.readout(&m)["status"].as_str().unwrap().contains("Raise the Master's VOLUME"), "{}", stuck.readout(&m));
        // The player turned the master up: the same match moves again.
        let freed = stuck.matched(-14.0, Some(-16.0));
        assert!(!freed.limited && near(freed.gain_db, 6.0 + 2.0 / (4.0 / 6.0)), "{freed:?}");
        // Softer playing after a raise is no verdict on the limiter: a plain step.
        let softer = t.matched(-14.0, Some(-24.0));
        assert!(!softer.limited && near(softer.gain_db, 16.0), "{softer:?}");
        // Coming down needs no yield: the limiter lets go at once or not, the
        // step stays bounded.
        let down = t2.matched(-20.0, Some(-14.0));
        assert!(down.gain_db < t2.gain_db && down.gain_db >= t2.gain_db - 18.0, "{down:?}");
        // A new target with nothing played since: from the known "before".
        let t4 = t2.matched(-16.0, None);
        assert_eq!((t4.lufs, t4.gain_db, t4.before_lufs), (Some(-16.0), 4.0, Some(-20.0)));
        // No sound at all yet: the target is stored, the chain untouched.
        let waiting = MasterTarget::OFF.matched(-14.0, None);
        assert_eq!((waiting.gain_db, waiting.before_lufs), (0.0, None));
        assert_eq!(waiting.gains(), (1.0, 1.0), "a trim of 0 dB is exactly unity");
        // The trim and the target are bounded.
        assert_eq!(MasterTarget::OFF.matched(-14.0, Some(-60.0)).gain_db, TARGET_GAIN_RANGE_DB);
        assert_eq!(MasterTarget::OFF.matched(0.0, Some(-20.0)).lufs, Some(TARGET_RANGE_LUFS.1));
    }

    #[test]
    fn targets_parse_from_presets_numbers_and_off() {
        assert_eq!(parse_target(Some("spotify"), None), Ok(Some(-14.0)));
        assert_eq!(parse_target(Some("YouTube"), None), Ok(Some(-14.0)));
        assert_eq!(parse_target(Some("Apple Music"), None), Ok(Some(-16.0)));
        assert_eq!(parse_target(Some("club"), None), Ok(Some(-8.0)));
        assert_eq!(parse_target(Some("off"), None), Ok(None));
        assert_eq!(parse_target(None, Some(-12.5)), Ok(Some(-12.5)));
        assert_eq!(parse_target(Some("-14 LUFS"), None), Ok(Some(-14.0)));
        assert_eq!(parse_target(Some("\u{2212}9"), None), Ok(Some(-9.0)));
        assert!(parse_target(None, Some(3.0)).unwrap_err().contains("outside"));
        assert!(parse_target(Some("loud"), None).unwrap_err().contains("spotify"));
        assert!(parse_target(None, None).is_err());
    }

    #[test]
    fn the_readout_names_before_after_and_the_true_peak() {
        let t = MasterTarget::OFF.matched(-14.0, Some(-20.5));
        let mut m = MeterSnapshot::default();
        let waiting = t.readout(&m);
        assert_eq!(waiting["before_lufs"], -20.5);
        assert!(waiting["after_lufs"].is_null() && waiting["true_peak_ok"].is_null(), "{waiting}");
        assert_eq!(waiting["preset"], "spotify/youtube/tidal");
        m.integrated_lufs = -14.2;
        m.true_peak_max_db = -1.3;
        let done = t.readout(&m);
        assert_eq!((done["after_lufs"].as_f64(), done["true_peak_ok"].as_bool()), (Some(-14.2), Some(true)), "{done}");
        assert!(done["status"].as_str().unwrap().starts_with("on target"), "{done}");
        m.integrated_lufs = -15.6;
        m.true_peak_max_db = -0.2;
        let off = t.readout(&m);
        assert_eq!(off["true_peak_ok"], false);
        assert!(off["status"].as_str().unwrap().contains("set the target again"), "{off}");
        assert!(MasterTarget::OFF.readout(&m)["target_lufs"].is_null());
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