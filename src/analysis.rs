//! Audio analysis for tests and self-tests (issue #13): what a listener
//! would notice, as numbers. A rendered note's pitch, brightness and
//! aliasing; flat tops of a clipped master; whether a noise signal reaches a
//! tap and how late. Pure functions on sample slices, no engine types and
//! no dependency (own radix-2 FFT, own noise).

use std::f64::consts::PI;

/// In-place radix-2 FFT (`re.len()` a power of two). `inverse` runs the
/// inverse transform without the 1/n scale.
fn fft(re: &mut [f64], im: &mut [f64], inverse: bool) {
    let n = re.len();
    debug_assert!(n.is_power_of_two() && im.len() == n);
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let sign = if inverse { 1.0 } else { -1.0 };
    let mut len = 2;
    while len <= n {
        let ang = sign * 2.0 * PI / len as f64;
        let (wr, wi) = (ang.cos(), ang.sin());
        for start in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1.0, 0.0);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let (tr, ti) = (re[b] * cr - im[b] * ci, re[b] * ci + im[b] * cr);
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                (cr, ci) = (cr * wr - ci * wi, cr * wi + ci * wr);
            }
        }
        len <<= 1;
    }
}

/// Power spectrum of a mono signal (Hann window over the whole signal,
/// zero-padded to a power of two), DC to Nyquist.
pub struct Spectrum {
    power: Vec<f64>,
    /// Width of one (padded) bin.
    bin_hz: f64,
    /// Frequency resolution of the signal itself (sr / its length): the
    /// window's main lobe spans ±2 of these.
    res_hz: f64,
}

impl Spectrum {
    pub fn of(mono: &[f32], sr: f32) -> Self {
        let len = mono.len().max(1);
        let n = len.next_power_of_two().max(2);
        let (mut re, mut im) = (vec![0.0; n], vec![0.0; n]);
        for (i, &x) in mono.iter().enumerate() {
            re[i] = x as f64 * (0.5 - 0.5 * (2.0 * PI * i as f64 / len as f64).cos());
        }
        fft(&mut re, &mut im, false);
        let power = (0..=n / 2).map(|k| re[k] * re[k] + im[k] * im[k]).collect();
        Self { power, bin_hz: sr as f64 / n as f64, res_hz: sr as f64 / len as f64 }
    }

    fn total(&self) -> f64 {
        self.power.iter().sum()
    }

    /// The bin's frequency, refined by a parabola through the log power of
    /// the bin and its neighbours (a Hann peak is close to one).
    fn peak_hz(&self, i: usize) -> f64 {
        if i == 0 || i + 1 >= self.power.len() {
            return i as f64 * self.bin_hz;
        }
        let ln = |k: usize| self.power[k].max(1e-300).ln();
        let (a, b, c) = (ln(i - 1), ln(i), ln(i + 1));
        let den = a - 2.0 * b + c;
        let d = if den.abs() > 1e-12 { (0.5 * (a - c) / den).clamp(-0.5, 0.5) } else { 0.0 };
        (i as f64 + d) * self.bin_hz
    }

    fn argmax(&self, lo: usize, hi: usize) -> usize {
        let hi = hi.min(self.power.len() - 2);
        (lo..=hi).fold(lo, |m, k| if self.power[k] > self.power[m] { k } else { m })
    }

    /// Pitch of a harmonic tone: the strongest partial above 20 Hz, or the
    /// lowest of its subharmonics (down to 1/8) that is a clear peak of its
    /// own (within 20 dB of it), since a tone's strongest partial is not
    /// always its fundamental. None for silence.
    pub fn fundamental_hz(&self) -> Option<f32> {
        let lo = ((20.0 / self.bin_hz).ceil() as usize).max(1);
        if lo + 1 >= self.power.len() {
            return None;
        }
        let top = self.argmax(lo, self.power.len() - 2);
        if self.power[top] <= 0.0 {
            return None;
        }
        let lobe = 2.0 * self.res_hz / self.bin_hz;
        for k in (2..=8).rev() {
            let c = top as f64 / k as f64;
            if c - lobe < lo as f64 {
                continue;
            }
            let i = self.argmax((c - lobe).floor() as usize, (c + lobe).ceil() as usize);
            let local_max = self.power[i] >= self.power[i - 1] && self.power[i] >= self.power[i + 1];
            if local_max && self.power[i] >= self.power[top] * 0.01 {
                return Some(self.peak_hz(i) as f32);
            }
        }
        Some(self.peak_hz(top) as f32)
    }

    /// Brightness: the magnitude-weighted mean frequency.
    pub fn centroid_hz(&self) -> f32 {
        let (mut fw, mut w) = (0.0, 0.0);
        for (k, p) in self.power.iter().enumerate() {
            let m = p.sqrt();
            fw += k as f64 * self.bin_hz * m;
            w += m;
        }
        if w > 0.0 { (fw / w) as f32 } else { 0.0 }
    }

    /// Share of the power above `hz` (0..1).
    pub fn share_above(&self, hz: f32) -> f32 {
        let total = self.total();
        if total <= 0.0 {
            return 0.0;
        }
        let above: f64 = self.power.iter().enumerate().filter(|(k, _)| *k as f64 * self.bin_hz > hz as f64).map(|(_, p)| p).sum();
        (above / total) as f32
    }

    /// Share of the power that is not at a multiple of `f0` (0..1): aliases
    /// of partials folded back from above Nyquist, intermodulation, noise.
    /// A harmonic is the window's main lobe and a little more around k·f0
    /// (±4 resolution bins, at most f0/4), so the signal must be long enough
    /// that 8 / its length in seconds is well below `f0`.
    pub fn inharmonic_share(&self, f0: f32) -> f32 {
        let total = self.total();
        if total <= 0.0 || f0 <= 0.0 {
            return 0.0;
        }
        let f0 = f0 as f64;
        let tol = (4.0 * self.res_hz).min(f0 / 4.0);
        let off: f64 = self
            .power
            .iter()
            .enumerate()
            .filter(|(k, _)| {
                let f = *k as f64 * self.bin_hz;
                let h = (f / f0).round().max(1.0);
                (f - h * f0).abs() > tol
            })
            .map(|(_, p)| p)
            .sum();
        (off / total) as f32
    }
}

pub fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0f32, |m, s| m.max(s.abs()))
}

/// Share of the samples at or above `ceiling` (0..1): how much of the time
/// a signal sits where a limiter or clipper flattens it.
pub fn hard_saturation_share(x: &[f32], ceiling: f32) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    x.iter().filter(|s| s.abs() >= ceiling).count() as f32 / x.len() as f32
}

/// Longest run of consecutive samples at the signal's own peak level
/// (within 0.1 %): a flat top. A clean tone touches its peak for a sample
/// or a few (a 50 Hz sine at 48 kHz: 5); a clipped one stays there.
pub fn flat_top_run(x: &[f32]) -> usize {
    let top = peak(x) * 0.999;
    if top <= 0.0 {
        return 0;
    }
    let (mut best, mut run) = (0, 0);
    for s in x {
        run = if s.abs() >= top { run + 1 } else { 0 };
        best = best.max(run);
    }
    best
}

/// Share of the signal's power that is not the `freq` sine (least-squares
/// fit of one sine+cosine over `x`): ~0 for a clean tone, grows with
/// distortion. Robust to a slowly decaying envelope, unlike a single
/// Goertzel bin.
pub fn non_fundamental_share(x: &[f32], freq: f32, sr: f32) -> f32 {
    let k = 2.0 * std::f32::consts::PI * freq / sr;
    let (mut ss, mut cc, mut sc, mut xs, mut xc, mut xx) = (0f64, 0f64, 0f64, 0f64, 0f64, 0f64);
    for (i, &v) in x.iter().enumerate() {
        let (s, c) = ((k * i as f32).sin() as f64, (k * i as f32).cos() as f64);
        let v = v as f64;
        ss += s * s; cc += c * c; sc += s * c;
        xs += v * s; xc += v * c; xx += v * v;
    }
    let det = ss * cc - sc * sc;
    if det.abs() < 1e-9 || xx < 1e-12 {
        return 0.0;
    }
    let a = (xs * cc - xc * sc) / det;
    let b = (xc * ss - xs * sc) / det;
    let fit = a * xs + b * xc;
    ((xx - fit).max(0.0) / xx) as f32
}

/// SplitMix64: a deterministic stream of 64-bit words from one seed.
fn words(seed: u64) -> impl Iterator<Item = u64> {
    let mut s = seed;
    std::iter::repeat_with(move || {
        s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    })
}

/// White noise, uniform in [-1, 1), the same for the same seed.
pub fn white(seed: u64) -> impl Iterator<Item = f32> {
    words(seed).map(|w| ((w >> 40) as f32 / (1u64 << 23) as f32) - 1.0)
}

/// Pink noise (−3 dB per octave; Paul Kellet's filter on `white(seed)`),
/// peaks around ±0.5.
pub fn pink(seed: u64) -> impl Iterator<Item = f32> {
    let mut b = [0.0f32; 7];
    white(seed).map(move |w| {
        b[0] = 0.99886 * b[0] + w * 0.0555179;
        b[1] = 0.99332 * b[1] + w * 0.0750759;
        b[2] = 0.96900 * b[2] + w * 0.1538520;
        b[3] = 0.86650 * b[3] + w * 0.3104856;
        b[4] = 0.55000 * b[4] + w * 0.5329522;
        b[5] = -0.7616 * b[5] - w * 0.0168980;
        let p = b.iter().sum::<f32>() + w * 0.5362;
        b[6] = w * 0.115926;
        p * 0.11
    })
}

/// Where two signals line up best: `b` is `a` delayed by `lag_frames`
/// (negative: `b` is early), with `r` their normalized correlation there
/// (1 = the same signal, −1 = inverted, ~0 = unrelated).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct XcorrPeak {
    pub lag_frames: isize,
    pub r: f32,
}

/// The lag within ±`max_lag` with the largest |correlation| of `a` and `b`.
pub fn xcorr_peak(a: &[f32], b: &[f32], max_lag: usize) -> XcorrPeak {
    let energy = |x: &[f32]| x.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>();
    let norm = (energy(a) * energy(b)).sqrt();
    if norm <= 0.0 {
        return XcorrPeak { lag_frames: 0, r: 0.0 };
    }
    let n = (a.len() + b.len()).next_power_of_two().max(2);
    let load = |x: &[f32]| {
        let (mut re, mut im) = (vec![0.0; n], vec![0.0; n]);
        for (r, s) in re.iter_mut().zip(x) {
            *r = *s as f64;
        }
        fft(&mut re, &mut im, false);
        (re, im)
    };
    let ((ar, ai), (br, bi)) = (load(a), load(b));
    // conj(A)·B: its inverse is c[ℓ] = Σ a[i]·b[i + ℓ].
    let mut cr: Vec<f64> = (0..n).map(|k| ar[k] * br[k] + ai[k] * bi[k]).collect();
    let mut ci: Vec<f64> = (0..n).map(|k| ar[k] * bi[k] - ai[k] * br[k]).collect();
    fft(&mut cr, &mut ci, true);
    let max_lag = max_lag.min(n / 2 - 1) as isize;
    let at = |lag: isize| cr[lag.rem_euclid(n as isize) as usize] / n as f64;
    let best = (-max_lag..=max_lag).fold(0isize, |m, l| if at(l).abs() > at(m).abs() { l } else { m });
    XcorrPeak { lag_frames: best, r: (at(best) / norm) as f32 }
}

/// How strongly `reference` is present in `tap` at any lag within
/// ±`max_lag` (0..1): ~1 when the tap carries it alone, ~0.7 for one of two
/// equal independent signals, ~0 when it is absent.
pub fn presence(reference: &[f32], tap: &[f32], max_lag: usize) -> f32 {
    xcorr_peak(reference, tap, max_lag).r.abs()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48000.0;

    fn tone(secs: f32, partials: &[(f32, f32)]) -> Vec<f32> {
        (0..(secs * SR) as usize)
            .map(|i| {
                let t = i as f64 / SR as f64;
                partials.iter().map(|(hz, a)| a * (2.0 * PI * *hz as f64 * t).sin() as f32).sum()
            })
            .collect()
    }

    /// Every harmonic of `f0` below Nyquist at 1/k: a saw without aliases.
    fn band_limited_saw(f0: f32, secs: f32) -> Vec<f32> {
        let partials: Vec<(f32, f32)> = (1..).map(|k| (k as f32 * f0, 0.5 / k as f32)).take_while(|(hz, _)| *hz < SR / 2.0).collect();
        tone(secs, &partials)
    }

    /// The textbook ramp: every harmonic, so those above Nyquist fold back.
    fn naive_saw(f0: f32, secs: f32) -> Vec<f32> {
        (0..(secs * SR) as usize).map(|i| ((i as f64 * f0 as f64 / SR as f64) % 1.0) as f32 - 0.5).collect()
    }

    #[test]
    fn the_pitch_of_a_tone_is_found_within_one_hertz() {
        for hz in [55.0f32, 261.63, 440.0, 1234.5] {
            let f = Spectrum::of(&tone(1.0, &[(hz, 0.5)]), SR).fundamental_hz().unwrap();
            assert!((f - hz).abs() < 1.0, "sine {hz} Hz measured {f} Hz");
        }
        // The saw's fundamental is its strongest partial; this tone's is
        // its second harmonic, and its pitch is still the fundamental.
        let saw = Spectrum::of(&band_limited_saw(196.0, 1.0), SR).fundamental_hz().unwrap();
        assert!((saw - 196.0).abs() < 1.0, "saw 196 Hz measured {saw}");
        let weak = Spectrum::of(&tone(1.0, &[(110.0, 0.2), (220.0, 0.6), (330.0, 0.3)]), SR).fundamental_hz().unwrap();
        assert!((weak - 110.0).abs() < 1.0, "weak fundamental 110 Hz measured {weak}");
        assert_eq!(Spectrum::of(&vec![0.0; 4800], SR).fundamental_hz(), None, "silence has no pitch");
    }

    #[test]
    fn a_saw_is_brighter_than_a_sine_of_the_same_pitch() {
        let sine = Spectrum::of(&tone(0.5, &[(220.0, 0.5)]), SR).centroid_hz();
        let saw = Spectrum::of(&band_limited_saw(220.0, 0.5), SR).centroid_hz();
        assert!((sine - 220.0).abs() < 30.0, "sine centroid {sine}");
        assert!(saw > 3.0 * sine, "saw centroid {saw} vs sine {sine}");
    }

    #[test]
    fn a_hard_clipped_sine_sits_at_its_ceiling_and_a_clean_one_never_does() {
        let clean = tone(0.5, &[(440.0, 0.4)]);
        let clipped: Vec<f32> = tone(0.5, &[(440.0, 1.0)]).iter().map(|s| s.clamp(-0.5, 0.5)).collect();
        assert_eq!(hard_saturation_share(&clean, 0.5), 0.0);
        let share = hard_saturation_share(&clipped, 0.5);
        assert!((share - 2.0 / 3.0).abs() < 0.01, "|sin| ≥ 0.5 two thirds of the time: {share}");
        assert!(flat_top_run(&clean) <= 2, "clean flat top {}", flat_top_run(&clean));
        assert!(flat_top_run(&clipped) > 30, "clipped flat top {}", flat_top_run(&clipped));
        assert!(non_fundamental_share(&clean, 440.0, SR) < 1e-4);
        assert!(non_fundamental_share(&clipped, 440.0, SR) > 0.01);
    }

    #[test]
    fn a_naive_saw_aliases_and_a_band_limited_one_does_not() {
        let f0 = 1661.2;
        let naive = Spectrum::of(&naive_saw(f0, 1.0), SR).inharmonic_share(f0);
        let clean = Spectrum::of(&band_limited_saw(f0, 1.0), SR).inharmonic_share(f0);
        assert!(naive > 0.005, "naive saw inharmonic share {naive}");
        assert!(clean < 1e-4, "band-limited saw inharmonic share {clean}");
    }

    #[test]
    fn cross_correlation_finds_a_known_delay_and_no_relation_between_two_noises() {
        let a: Vec<f32> = white(7).take(8192).collect();
        let late: Vec<f32> = std::iter::repeat(0.0).take(37).chain(a.iter().copied()).take(8192).collect();
        let p = xcorr_peak(&a, &late, 256);
        assert_eq!(p.lag_frames, 37);
        assert!(p.r > 0.99, "r {}", p.r);
        let early = &a[37..];
        assert_eq!(xcorr_peak(&a, early, 256).lag_frames, -37);
        let inverted: Vec<f32> = a.iter().map(|s| -0.3 * s).collect();
        let p = xcorr_peak(&a, &inverted, 256);
        assert_eq!(p.lag_frames, 0);
        assert!(p.r < -0.99, "inverted r {}", p.r);
        let other: Vec<f32> = white(8).take(8192).collect();
        assert!(presence(&a, &other, 256) < 0.05, "independent noises: {}", presence(&a, &other, 256));
        let both: Vec<f32> = a.iter().zip(&other).map(|(x, y)| x + y).collect();
        let half = presence(&a, &both, 256);
        assert!((half - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.05, "one of two equal noises: {half}");
        assert_eq!(presence(&a, &vec![0.0; 8192], 256), 0.0, "silence carries nothing");
    }

    #[test]
    fn pink_noise_has_less_high_end_than_white_and_both_repeat_per_seed() {
        let w: Vec<f32> = white(1).take(48000).collect();
        let p: Vec<f32> = pink(1).take(48000).collect();
        let (hw, hp) = (Spectrum::of(&w, SR).share_above(4000.0), Spectrum::of(&p, SR).share_above(4000.0));
        assert!((hw - 20.0 / 24.0).abs() < 0.03, "white above 4 kHz {hw}");
        assert!(hp < 0.4 * hw, "pink above 4 kHz {hp} vs white {hw}");
        assert!(peak(&w) <= 1.0 && peak(&p) < 1.0, "in range: {} {}", peak(&w), peak(&p));
        assert!(white(1).take(64).eq(white(1).take(64)) && !white(1).take(64).eq(white(2).take(64)));
        assert!(pink(3).take(64).eq(pink(3).take(64)));
    }
}
