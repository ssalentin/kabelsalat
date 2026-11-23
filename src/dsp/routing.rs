//! Routing utility units (issue #308): the A/B switch and crossfader, and
//! the splitter. Both are zero-latency and allocation-free, and both use a
//! second stereo buffer beside the one every processor has
//! (`Processor::process_aux`): the crossfader's second input, the splitter's
//! second output. The engine fills and reads it by the port a cable is
//! plugged into.

use super::{clamp01, flush, frame_count, log_map, log_norm, Processor, Slew};
use crate::arp::step_index;
use crate::engine::gain_ramp::{GainRamp, RAMP_S};

// ---------------------------------------------------------------------------
// Crossfade: IN A, IN B -> OUT
// ---------------------------------------------------------------------------

/// How the crossfader's MIX knob weighs its inputs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FadeMode {
    /// Equal power: two different sources keep their loudness through the
    /// travel (each is −3 dB at the midpoint).
    Power,
    /// Linear: two paths of the same sound (dry and wet) sum to the same
    /// level at every position (each is −6 dB at the midpoint).
    Linear,
    /// A/B switch: A below the midpoint, B from it on. For a foot switch or
    /// an expression pedal that should select, not blend.
    Switch,
}

impl FadeMode {
    pub const ALL: [FadeMode; 3] = [FadeMode::Power, FadeMode::Linear, FadeMode::Switch];
    pub const NAMES: [&'static str; 3] = ["Equal power", "Linear", "A/B switch"];
    /// The readout on the unit's face.
    pub const LABELS: [&'static str; 3] = ["POWER", "LINEAR", "SWITCH"];

    /// The mode a 0..1 knob value means (the one stepped mapping, rule 137).
    pub fn from_norm(v: f32) -> Self {
        Self::ALL[step_index(v as f64, Self::ALL.len())]
    }

    pub fn name(self) -> &'static str {
        Self::NAMES[self as usize]
    }

    /// The gains of A and B at mix position `mix` (0 = A, 1 = B).
    pub fn gains(self, mix: f32) -> (f32, f32) {
        let m = clamp01(mix);
        match self {
            FadeMode::Power => {
                let a = m * std::f32::consts::FRAC_PI_2;
                // The ends are exact, so the other input is really off.
                if m <= 0.0 {
                    (1.0, 0.0)
                } else if m >= 1.0 {
                    (0.0, 1.0)
                } else {
                    (a.cos(), a.sin())
                }
            }
            FadeMode::Linear => (1.0 - m, m),
            FadeMode::Switch => {
                if m >= 0.5 {
                    (0.0, 1.0)
                } else {
                    (1.0, 0.0)
                }
            }
        }
    }
}

pub const P_XFADE_MIX: u32 = 0;
pub const P_XFADE_MODE: u32 = 1;

/// A/B switch and crossfader. Param 0 `mix`: 0 = A, 1 = B. Param 1 `mode`:
/// three positions (`FadeMode`). Every change of either reaches the output
/// over `RAMP_S` (5 ms, rule 202), so a switch under a held note has no
/// click and a knob a controller sweeps has no zipper noise.
pub struct Crossfade {
    mix: f32,
    mode: FadeMode,
    a: GainRamp,
    b: GainRamp,
    ramp: u32,
    /// False until the first block: the gains start where the first
    /// parameters put them, without a fade from the defaults.
    primed: bool,
}

impl Crossfade {
    pub fn new(sample_rate: f64) -> Self {
        Self {
            mix: 0.0,
            mode: FadeMode::Power,
            a: GainRamp::at(1.0),
            b: GainRamp::at(0.0),
            ramp: ((RAMP_S * sample_rate) as u32).max(1),
            primed: false,
        }
    }

    fn retarget(&mut self) {
        let (ga, gb) = self.mode.gains(self.mix);
        if self.primed {
            self.a.set((ga, ga), self.ramp);
            self.b.set((gb, gb), self.ramp);
        } else {
            self.a = GainRamp::at(ga);
            self.b = GainRamp::at(gb);
        }
    }
}

impl Processor for Crossfade {
    /// Without a second input (a master insert, where only one signal
    /// exists) the unit passes its input on.
    fn process(&mut self, _io: &mut [f32], _frames: usize) {}

    /// `io` holds IN A and becomes the output; `aux` holds IN B.
    fn process_aux(&mut self, io: &mut [f32], aux: &mut [f32], frames: usize) {
        let n = frame_count(io, frames).min(aux.len() / 2) * 2;
        self.primed = true;
        self.a.apply(&mut io[..n]);
        self.b.apply(&mut aux[..n]);
        for (o, b) in io[..n].iter_mut().zip(&aux[..n]) {
            *o += *b;
        }
    }

    fn set_param(&mut self, id: u32, value: f32) {
        match id {
            P_XFADE_MIX => self.mix = clamp01(value),
            P_XFADE_MODE => self.mode = FadeMode::from_norm(value),
            _ => return,
        }
        self.retarget();
    }

    fn param_count(&self) -> u32 {
        2
    }

    fn param_name(&self, id: u32) -> &'static str {
        match id {
            P_XFADE_MIX => "mix",
            P_XFADE_MODE => "mode",
            _ => "unknown",
        }
    }
}

// ---------------------------------------------------------------------------
// Split: IN -> OUT 1, OUT 2
// ---------------------------------------------------------------------------

/// What the splitter's two outputs carry. In every mode the two outputs sum
/// back to the input: exactly for mid/side and left/right, as an all-pass
/// (flat level, shifted phase) for the band split.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitMode {
    /// Two bands around FREQ, a 4th-order Linkwitz-Riley crossover
    /// (24 dB/octave): OUT 1 = low, OUT 2 = high.
    Band,
    /// OUT 1 = mid ((L+R)/2 on both sides), OUT 2 = side ((L−R)/2 left and
    /// its inverse right).
    MidSide,
    /// OUT 1 = the left channel alone, OUT 2 = the right channel alone.
    LeftRight,
}

impl SplitMode {
    pub const ALL: [SplitMode; 3] = [SplitMode::Band, SplitMode::MidSide, SplitMode::LeftRight];
    pub const NAMES: [&'static str; 3] = ["Low / High", "Mid / Side", "Left / Right"];
    /// The readout on the unit's face.
    pub const LABELS: [&'static str; 3] = ["BAND", "M/S", "L/R"];

    pub fn from_norm(v: f32) -> Self {
        Self::ALL[step_index(v as f64, Self::ALL.len())]
    }

    pub fn name(self) -> &'static str {
        Self::NAMES[self as usize]
    }
}

pub const P_SPLIT_MODE: u32 = 0;
pub const P_SPLIT_FREQ: u32 = 1;

/// The crossover's range: from under a bass guitar's low E to the top of
/// the presence range, so every position splits something audible.
pub const SPLIT_MIN_HZ: f32 = 60.0;
pub const SPLIT_MAX_HZ: f32 = 8000.0;
/// Where a new splitter crosses over (bass against the rest).
pub const SPLIT_DEFAULT_HZ: f32 = 250.0;

/// The crossover frequency a 0..1 knob value means.
pub fn split_hz(norm: f32) -> f32 {
    log_map(norm, SPLIT_MIN_HZ, SPLIT_MAX_HZ)
}

/// The knob value of a crossover frequency.
pub fn split_norm(hz: f32) -> f32 {
    log_norm(hz, SPLIT_MIN_HZ, SPLIT_MAX_HZ)
}

/// A 2nd-order state-variable filter (trapezoidal integrators), Butterworth
/// damping: stable while its frequency moves, and it gives the low-pass and
/// the high-pass of one input at once.
#[derive(Clone, Copy, Default)]
struct Svf {
    ic1: f32,
    ic2: f32,
}

/// The coefficients of every `Svf` of a crossover at one frequency.
#[derive(Clone, Copy)]
struct SvfCoef {
    a1: f32,
    a2: f32,
    a3: f32,
}

const SVF_K: f32 = std::f32::consts::SQRT_2;

impl SvfCoef {
    fn at(hz: f32, sample_rate: f32) -> Self {
        let g = (std::f32::consts::PI * (hz / sample_rate).min(0.45)).tan();
        let a1 = 1.0 / (1.0 + g * (g + SVF_K));
        let a2 = g * a1;
        SvfCoef { a1, a2, a3: g * a2 }
    }
}

impl Svf {
    /// (low-pass, high-pass) of `x`.
    #[inline]
    fn tick(&mut self, c: &SvfCoef, x: f32) -> (f32, f32) {
        let v3 = x - self.ic2;
        let v1 = c.a1 * self.ic1 + c.a2 * v3;
        let v2 = self.ic2 + c.a2 * self.ic1 + c.a3 * v3;
        self.ic1 = flush(2.0 * v1 - self.ic1);
        self.ic2 = flush(2.0 * v2 - self.ic2);
        (v2, x - SVF_K * v1 - v2)
    }
}

/// One channel of the Linkwitz-Riley crossover: the low band is two
/// Butterworth low-passes in series, the high band two high-passes.
#[derive(Clone, Copy, Default)]
struct Crossover {
    first: Svf,
    low: Svf,
    high: Svf,
}

impl Crossover {
    #[inline]
    fn tick(&mut self, c: &SvfCoef, x: f32) -> (f32, f32) {
        let (lp, hp) = self.first.tick(c, x);
        (self.low.tick(c, lp).0, self.high.tick(c, hp).1)
    }
}

/// Splitter. Param 0 `mode`: three positions (`SplitMode`). Param 1 `freq`:
/// the crossover of the band split, `SPLIT_MIN_HZ`..`SPLIT_MAX_HZ`
/// (logarithmic); the other modes do not use it.
pub struct Split {
    sample_rate: f32,
    mode: SplitMode,
    /// The mode asked for; the outputs dip to silence for it (`duck`).
    want: SplitMode,
    hz: Slew,
    coef: SvfCoef,
    coef_hz: f32,
    bands: [Crossover; 2],
    /// A mode change swaps what both outputs carry: it happens at silence,
    /// 5 ms down and 5 ms up, never in one sample.
    duck: GainRamp,
    ramp: u32,
    primed: bool,
}

impl Split {
    pub fn new(sample_rate: f64) -> Self {
        let sr = sample_rate as f32;
        Self {
            sample_rate: sr,
            mode: SplitMode::Band,
            want: SplitMode::Band,
            hz: Slew::new(sample_rate, 20.0, SPLIT_DEFAULT_HZ),
            coef: SvfCoef::at(SPLIT_DEFAULT_HZ, sr),
            coef_hz: SPLIT_DEFAULT_HZ,
            bands: [Crossover::default(); 2],
            duck: GainRamp::UNITY,
            ramp: ((RAMP_S * sample_rate) as u32).max(1),
            primed: false,
        }
    }
}

impl Processor for Split {
    /// Without a second output (a master insert) the unit passes its input
    /// on: the two outputs summed.
    fn process(&mut self, _io: &mut [f32], _frames: usize) {}

    /// `io` holds the input and becomes OUT 1; `aux` becomes OUT 2.
    fn process_aux(&mut self, io: &mut [f32], aux: &mut [f32], frames: usize) {
        let n = frame_count(io, frames).min(aux.len() / 2);
        if !self.primed {
            self.primed = true;
            self.mode = self.want;
            self.hz.value = self.hz.target;
        }
        if self.want != self.mode {
            if self.duck.is_silent() {
                self.mode = self.want;
                self.bands = [Crossover::default(); 2];
                self.duck.set((1.0, 1.0), self.ramp);
            } else {
                self.duck.set((0.0, 0.0), self.ramp);
            }
        }
        match self.mode {
            SplitMode::Band => {
                for f in 0..n {
                    let hz = self.hz.next();
                    // The coefficients follow the knob while it moves and
                    // rest once it has arrived (a `tan` per sample otherwise).
                    if (hz - self.coef_hz).abs() > 0.01 {
                        self.coef = SvfCoef::at(hz, self.sample_rate);
                        self.coef_hz = hz;
                    }
                    for ch in 0..2 {
                        let (lo, hi) = self.bands[ch].tick(&self.coef, io[2 * f + ch]);
                        io[2 * f + ch] = lo;
                        aux[2 * f + ch] = hi;
                    }
                }
            }
            SplitMode::MidSide => {
                for f in 0..n {
                    let (l, r) = (io[2 * f], io[2 * f + 1]);
                    let (m, s) = (0.5 * (l + r), 0.5 * (l - r));
                    io[2 * f] = m;
                    io[2 * f + 1] = m;
                    aux[2 * f] = s;
                    aux[2 * f + 1] = -s;
                }
            }
            SplitMode::LeftRight => {
                for f in 0..n {
                    aux[2 * f] = 0.0;
                    aux[2 * f + 1] = io[2 * f + 1];
                    io[2 * f + 1] = 0.0;
                }
            }
        }
        if !self.duck.is_unity() {
            let mut same = self.duck;
            self.duck.apply(&mut io[..n * 2]);
            same.apply(&mut aux[..n * 2]);
        }
    }

    fn set_param(&mut self, id: u32, value: f32) {
        match id {
            P_SPLIT_MODE => self.want = SplitMode::from_norm(value),
            P_SPLIT_FREQ => self.hz.set(split_hz(value)),
            _ => {}
        }
    }

    fn param_count(&self) -> u32 {
        2
    }

    fn param_name(&self, id: u32) -> &'static str {
        match id {
            P_SPLIT_MODE => "mode",
            P_SPLIT_FREQ => "freq",
            _ => "unknown",
        }
    }

    fn reset(&mut self) {
        self.bands = [Crossover::default(); 2];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arp::step_value;

    const SR: f64 = 48000.0;

    fn sine(freq: f32, frames: usize, amp: f32) -> Vec<f32> {
        (0..frames)
            .flat_map(|f| {
                let x = amp * (2.0 * std::f32::consts::PI * freq * f as f32 / SR as f32).sin();
                [x, x]
            })
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>() / x.len().max(1) as f64).sqrt() as f32
    }

    fn db(a: f32, b: f32) -> f32 {
        20.0 * (a / b).log10()
    }

    /// A at 0.5 and B at 0.25, both constant, through a crossfader at `mix`
    /// in `mode`: the settled output level.
    fn fade(mode: FadeMode, mix: f32) -> f32 {
        let mut x = Crossfade::new(SR);
        x.set_param(P_XFADE_MODE, step_value(mode as usize, 3));
        x.set_param(P_XFADE_MIX, mix);
        let mut a = vec![0.5f32; 512];
        let mut b = vec![0.25f32; 512];
        x.process_aux(&mut a, &mut b, 256);
        a[511]
    }

    #[test]
    fn a_only_b_only_and_the_midpoint_have_the_stated_levels() {
        // Equal power: the ends are one input alone, the middle is both at
        // -3 dB (0.7071).
        assert_eq!(fade(FadeMode::Power, 0.0), 0.5);
        assert_eq!(fade(FadeMode::Power, 1.0), 0.25);
        let mid = fade(FadeMode::Power, 0.5);
        assert!((mid - 0.75 * std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6, "{mid}");
        // Linear: the middle is both at -6 dB (0.5).
        assert_eq!(fade(FadeMode::Linear, 0.0), 0.5);
        assert_eq!(fade(FadeMode::Linear, 1.0), 0.25);
        assert!((fade(FadeMode::Linear, 0.5) - 0.375).abs() < 1e-6);
        assert!((fade(FadeMode::Linear, 0.25) - (0.75 * 0.5 + 0.25 * 0.25)).abs() < 1e-6);
        // Switch: A below the midpoint, B from it on, nothing in between.
        assert_eq!(fade(FadeMode::Switch, 0.0), 0.5);
        assert_eq!(fade(FadeMode::Switch, 0.49), 0.5);
        assert_eq!(fade(FadeMode::Switch, 0.5), 0.25);
        assert_eq!(fade(FadeMode::Switch, 1.0), 0.25);
    }

    #[test]
    fn the_first_block_starts_at_the_set_position_without_a_fade() {
        let mut x = Crossfade::new(SR);
        x.set_param(P_XFADE_MIX, 1.0);
        let mut a = vec![0.5f32; 64];
        let mut b = vec![0.25f32; 64];
        x.process_aux(&mut a, &mut b, 32);
        assert!(a.iter().all(|v| *v == 0.25), "{a:?}");
    }

    /// The largest second difference of the left channel: a click is a jump
    /// in it (rule 202).
    fn second_difference(x: &[f32]) -> f32 {
        let l: Vec<f32> = x.chunks_exact(2).map(|f| f[0]).collect();
        l.windows(3).map(|w| (w[2] - 2.0 * w[1] + w[0]).abs()).fold(0.0, f32::max)
    }

    #[test]
    fn a_switch_under_a_held_sine_has_no_click() {
        // A held 220 Hz sine on A, silence on B, and a foot switch that goes
        // A -> B -> A in switch mode, at the worst moment (a crest).
        let frames = 4800;
        let held = sine(220.0, frames, 0.8);
        let own = second_difference(&held);
        let mut x = Crossfade::new(SR);
        x.set_param(P_XFADE_MODE, 1.0);
        x.set_param(P_XFADE_MIX, 0.0);
        let mut out = Vec::new();
        for (i, block) in held.chunks(64 * 2).enumerate() {
            // Frame 1472 is 5 ms after a crest region; frame 2944 comes back.
            if i == 23 {
                x.set_param(P_XFADE_MIX, 1.0);
            }
            if i == 46 {
                x.set_param(P_XFADE_MIX, 0.0);
            }
            let mut a = block.to_vec();
            let mut b = vec![0.0f32; block.len()];
            x.process_aux(&mut a, &mut b, 64);
            out.extend(a);
        }
        // It did switch: silent on B, back on A.
        assert!(rms(&out[2 * 2000..2 * 2900]) < 1e-6);
        assert!((rms(&out[2 * 3400..2 * 4800]) - rms(&held[2 * 3400..2 * 4800])).abs() < 1e-3);
        // A hard switch would jump by the sample value (up to 0.8); the ramp
        // only bends the sine: 0.8 / 240 frames per corner.
        let d2 = second_difference(&out);
        assert!(d2 < 0.8 / 240.0 + 2.0 * own, "second difference {d2}, the sine's own {own}");
        assert!(d2 < 0.006, "second difference {d2}");
    }

    #[test]
    fn a_swept_mix_knob_has_no_zipper_steps() {
        // A controller sends 128 values across 0.5 s over a held sine on A.
        let frames = 24000;
        let held = sine(220.0, frames, 0.8);
        let mut x = Crossfade::new(SR);
        let mut out = Vec::new();
        for (i, block) in held.chunks(64 * 2).enumerate() {
            x.set_param(P_XFADE_MIX, (i as f32 / 375.0 * 127.0).floor() / 127.0);
            let mut a = block.to_vec();
            let mut b = vec![0.0f32; block.len()];
            x.process_aux(&mut a, &mut b, 64);
            out.extend(a);
        }
        let d2 = second_difference(&out);
        assert!(d2 < 0.002, "second difference {d2}");
    }

    /// Both outputs of a splitter in `mode` at `hz` for `input`.
    fn split(mode: SplitMode, hz: f32, input: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let mut s = Split::new(SR);
        s.set_param(P_SPLIT_MODE, step_value(mode as usize, 3));
        s.set_param(P_SPLIT_FREQ, split_norm(hz));
        let mut a = input.to_vec();
        let mut b = vec![0.0f32; input.len()];
        for (x, y) in a.chunks_mut(128).zip(b.chunks_mut(128)) {
            let n = x.len() / 2;
            s.process_aux(x, y, n);
        }
        (a, b)
    }

    #[test]
    fn the_two_bands_sum_back_flat_within_a_twentieth_of_a_db() {
        // Tolerance: the sum of low and high is within 0.05 dB of the input
        // at every frequency, at three crossover settings (a 4th-order
        // Linkwitz-Riley pair sums to an all-pass).
        for cross in [SPLIT_MIN_HZ, SPLIT_DEFAULT_HZ, 2000.0, SPLIT_MAX_HZ] {
            for hz in [30.0, 60.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0] {
                let input = sine(hz, 24000, 0.5);
                let (lo, hi) = split(SplitMode::Band, cross, &input);
                let sum: Vec<f32> = lo.iter().zip(&hi).map(|(a, b)| a + b).collect();
                // Past the filters' onset.
                let from = 2 * 12000;
                let d = db(rms(&sum[from..]), rms(&input[from..]));
                assert!(d.abs() < 0.05, "crossover {cross} Hz, tone {hz} Hz: sum is {d:+.3} dB");
            }
        }
    }

    #[test]
    fn each_band_is_6_db_down_at_the_crossover_and_24_db_per_octave_beyond() {
        let level = |hz: f32| {
            let input = sine(hz, 24000, 0.5);
            let (lo, hi) = split(SplitMode::Band, 1000.0, &input);
            let from = 2 * 12000;
            (db(rms(&lo[from..]), rms(&input[from..])), db(rms(&hi[from..]), rms(&input[from..])))
        };
        let (lo, hi) = level(1000.0);
        assert!((lo + 6.02).abs() < 0.1 && (hi + 6.02).abs() < 0.1, "at the crossover: low {lo:.2} dB, high {hi:.2} dB");
        // Two octaves out: about 48 dB down on the far side, untouched on the near.
        let (lo, hi) = level(4000.0);
        assert!(lo < -46.0 && hi.abs() < 0.1, "at 4 kHz: low {lo:.1} dB, high {hi:.2} dB");
        let (lo, hi) = level(250.0);
        assert!(hi < -46.0 && lo.abs() < 0.1, "at 250 Hz: low {lo:.2} dB, high {hi:.1} dB");
    }

    /// A stereo signal with different content left and right.
    fn stereo(frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|f| {
                let t = f as f32 / SR as f32;
                [0.5 * (2.0 * std::f32::consts::PI * 220.0 * t).sin(), 0.3 * (2.0 * std::f32::consts::PI * 331.0 * t).sin()]
            })
            .collect()
    }

    #[test]
    fn mid_side_and_left_right_sum_back_to_the_input_exactly() {
        let input = stereo(4800);
        let (mid, side) = split(SplitMode::MidSide, 250.0, &input);
        for f in 0..4800 {
            // Mid is the same on both sides, side is opposite.
            assert_eq!(mid[2 * f], mid[2 * f + 1]);
            assert_eq!(side[2 * f], -side[2 * f + 1]);
            assert!((mid[2 * f] + side[2 * f] - input[2 * f]).abs() < 1e-6);
            assert!((mid[2 * f + 1] + side[2 * f + 1] - input[2 * f + 1]).abs() < 1e-6);
        }
        let (left, right) = split(SplitMode::LeftRight, 250.0, &input);
        for f in 0..4800 {
            assert_eq!((left[2 * f], left[2 * f + 1]), (input[2 * f], 0.0));
            assert_eq!((right[2 * f], right[2 * f + 1]), (0.0, input[2 * f + 1]));
        }
    }

    #[test]
    fn a_mode_change_under_a_held_sine_dips_instead_of_jumping() {
        let held = sine(220.0, 4800, 0.8);
        let mut s = Split::new(SR);
        s.set_param(P_SPLIT_MODE, step_value(SplitMode::LeftRight as usize, 3));
        let (mut one, mut two) = (Vec::new(), Vec::new());
        for (i, block) in held.chunks(128).enumerate() {
            if i == 23 {
                s.set_param(P_SPLIT_MODE, step_value(SplitMode::MidSide as usize, 3));
            }
            let mut a = block.to_vec();
            let mut b = vec![0.0f32; block.len()];
            s.process_aux(&mut a, &mut b, 64);
            one.extend(a);
            two.extend(b);
        }
        // Left/right before (the right channel is on OUT 2), mid/side after
        // (a mono signal has no side).
        assert!(rms(&two[..2 * 1400]) > 0.2);
        assert!(rms(&two[2 * 2400..]) < 1e-6);
        assert!((rms(&one[2 * 2400..]) - rms(&held[2 * 2400..])).abs() < 1e-3);
        for out in [&one, &two] {
            let d2 = second_difference(out);
            assert!(d2 < 0.006, "second difference {d2}");
            // The right channel too: OUT 1's right goes from silence to mid.
            let r: Vec<f32> = out.chunks_exact(2).flat_map(|f| [f[1], f[1]]).collect();
            let d2 = second_difference(&r);
            assert!(d2 < 0.006, "second difference, right {d2}");
        }
    }

    #[test]
    fn the_crossover_knob_spans_its_stated_range_and_the_default_is_250_hz() {
        assert!((split_hz(0.0) - SPLIT_MIN_HZ).abs() < 0.01);
        assert!((split_hz(1.0) - SPLIT_MAX_HZ).abs() < 1.0);
        assert!((split_hz(split_norm(SPLIT_DEFAULT_HZ)) - SPLIT_DEFAULT_HZ).abs() < 0.1);
    }

    #[test]
    fn every_mode_position_is_an_exact_knob_step() {
        for (i, m) in FadeMode::ALL.iter().enumerate() {
            assert_eq!(FadeMode::from_norm(step_value(i, 3)), *m);
        }
        for (i, m) in SplitMode::ALL.iter().enumerate() {
            assert_eq!(SplitMode::from_norm(step_value(i, 3)), *m);
        }
    }

    #[test]
    fn a_frequency_sweep_while_audio_runs_stays_bounded() {
        // The crossover moved end to end and back each block: the filters
        // stay stable and the sum stays near the input's level.
        let input = sine(440.0, 48000, 0.5);
        let mut s = Split::new(SR);
        let mut peak = 0.0f32;
        for (i, block) in input.chunks(128).enumerate() {
            s.set_param(P_SPLIT_FREQ, if i % 2 == 0 { 0.0 } else { 1.0 });
            let mut a = block.to_vec();
            let mut b = vec![0.0f32; block.len()];
            s.process_aux(&mut a, &mut b, 64);
            peak = a.iter().zip(&b).map(|(x, y)| (x + y).abs()).fold(peak, f32::max);
        }
        assert!(peak < 0.75, "peak {peak}");
    }
}
