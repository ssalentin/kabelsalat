//! A gain that reaches a new value over a fixed time instead of in one
//! sample (#274): a rig fader or an instrument's LEVEL that a setlist part
//! recalls, a controller or an agent moves while a note is held. The ramp
//! takes `RAMP_S` whatever the block size. Fixed size, no allocation.

/// How long a gain takes to reach a new value.
pub const RAMP_S: f64 = 0.005;

/// A stereo gain on its way to `to`, `left` frames from it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GainRamp {
    now: (f32, f32),
    to: (f32, f32),
    step: (f32, f32),
    left: u32,
}

impl GainRamp {
    pub const UNITY: GainRamp = GainRamp::at(1.0);

    /// A gain of `g` on both channels that is not moving.
    pub const fn at(g: f32) -> Self {
        GainRamp { now: (g, g), to: (g, g), step: (0.0, 0.0), left: 0 }
    }

    /// Head for `to` over `frames` frames, from wherever the gain is now.
    pub fn set(&mut self, to: (f32, f32), frames: u32) {
        if to == self.to {
            return;
        }
        self.to = to;
        self.left = frames.max(1);
        let n = self.left as f32;
        self.step = ((to.0 - self.now.0) / n, (to.1 - self.now.1) / n);
    }

    /// At zero and staying there.
    pub fn is_silent(&self) -> bool {
        self.now == (0.0, 0.0) && self.to == (0.0, 0.0)
    }

    /// At unity and staying there.
    pub fn is_unity(&self) -> bool {
        self.now == (1.0, 1.0) && self.to == (1.0, 1.0) && self.left == 0
    }

    /// Scale one interleaved stereo block, moving the gain on.
    pub fn apply(&mut self, buf: &mut [f32]) {
        if self.left == 0 {
            let (l, r) = self.now;
            if (l, r) != (1.0, 1.0) {
                for f in buf.chunks_exact_mut(2) {
                    f[0] *= l;
                    f[1] *= r;
                }
            }
            return;
        }
        for f in buf.chunks_exact_mut(2) {
            if self.left > 0 {
                self.left -= 1;
                self.now = if self.left == 0 { self.to } else { (self.now.0 + self.step.0, self.now.1 + self.step.1) };
            }
            f[0] *= self.now.0;
            f[1] *= self.now.1;
        }
    }

    /// Crossfade `dry` and `wet` in place inside `wet` (`0 = dry`, `1 = wet`)
    /// so toggling an effect's bypass on a part switch does not click (#275).
    pub fn mix_dry_wet(&mut self, dry: &[f32], wet: &mut [f32]) {
        if self.left == 0 {
            let (l, r) = self.now;
            if (l, r) == (1.0, 1.0) {
                return;
            }
            for (d, w) in dry.chunks_exact(2).zip(wet.chunks_exact_mut(2)) {
                w[0] = d[0] + (w[0] - d[0]) * l;
                w[1] = d[1] + (w[1] - d[1]) * r;
            }
            return;
        }
        for (d, w) in dry.chunks_exact(2).zip(wet.chunks_exact_mut(2)) {
            if self.left > 0 {
                self.left -= 1;
                self.now = if self.left == 0 { self.to } else { (self.now.0 + self.step.0, self.now.1 + self.step.1) };
            }
            w[0] = d[0] + (w[0] - d[0]) * self.now.0;
            w[1] = d[1] + (w[1] - d[1]) * self.now.1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The gain each frame of `blocks` blocks of `block` frames applies.
    fn gains(g: &mut GainRamp, blocks: usize, block: usize) -> Vec<f32> {
        let mut out = Vec::new();
        for _ in 0..blocks {
            let mut buf = vec![1.0f32; block * 2];
            g.apply(&mut buf);
            out.extend(buf.chunks_exact(2).map(|f| f[0]));
        }
        out
    }

    #[test]
    fn a_new_gain_takes_the_same_time_at_every_block_size() {
        for block in [32, 64, 256, 1024] {
            let mut g = GainRamp::at(0.4);
            g.set((0.8, 0.8), 240);
            let seen = gains(&mut g, 2048 / block, block);
            let reached = seen.iter().position(|&x| x == 0.8).unwrap();
            assert_eq!(reached, 239, "{block}-frame blocks: the gain lands on frame 240");
            let largest = seen.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
            assert!(largest <= 0.4 / 240.0 * 1.01, "{block}-frame blocks: it moves in even steps ({largest})");
        }
    }

    #[test]
    fn a_new_target_mid_ramp_turns_from_where_the_gain_is() {
        let mut g = GainRamp::at(0.0);
        g.set((1.0, 1.0), 100);
        let up = gains(&mut g, 1, 50);
        g.set((0.0, 0.0), 100);
        let down = gains(&mut g, 1, 200);
        assert!((down[0] - up[49]).abs() <= 0.011, "no jump at the turn: {} then {}", up[49], down[0]);
        assert!(g.is_silent(), "and it reaches silence");
    }

    #[test]
    fn a_gain_at_rest_scales_each_channel() {
        let mut g = GainRamp::at(1.0);
        g.set((0.5, 0.0), 1);
        let mut buf = vec![1.0f32; 8];
        g.apply(&mut buf);
        g.apply(&mut buf);
        assert_eq!(buf, vec![0.25, 0.0, 0.25, 0.0, 0.25, 0.0, 0.25, 0.0]);
    }

    #[test]
    fn mix_dry_wet_crossfades_cleanly_between_dry_and_wet() {
        let mut g = GainRamp::UNITY;
        assert!(g.is_unity());
        g.set((0.0, 0.0), 4);
        assert!(!g.is_unity());
        let dry = vec![1.0f32; 8];
        let mut wet = vec![0.0f32; 8];
        g.mix_dry_wet(&dry, &mut wet);
        assert_eq!(wet, vec![0.25, 0.25, 0.5, 0.5, 0.75, 0.75, 1.0, 1.0]);
        assert!(g.is_silent());
    }
}
