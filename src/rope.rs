//! Verlet rope for patch cables, ported from the salen7in Kabelsalat page
//! (`salen7in/kabelsalat/cables.js`). A cable is a chain of point masses
//! under gravity, held together by distance constraints, with its plugged
//! ends pinned to the jacks. A free end (unplugged, falling) just hangs.
//!
//! The web toy steps once per 60 Hz frame. Here the step is fixed at 120 Hz
//! and the per-frame constants are converted, so the cable feels the same at
//! any display rate.

use eframe::egui::{Pos2, Vec2};

pub const POINTS: usize = 18;
pub const STEP: f32 = 1.0 / 120.0;
const ITERATIONS: usize = 8;
/// 0.8 px per 60 Hz frame², as in the web toy.
const GRAVITY: f32 = 0.8 * 3600.0;
/// Velocity kept per 60 Hz frame.
const DAMPING: f32 = 0.985;
const SLACK: f32 = 1.18;
const SLACK_EXTRA: f32 = 50.0;

/// Smoothed polyline samples per rope segment (see [`Rope::sample`]).
pub const SAMPLES_PER_SEG: usize = 4;
pub const MAX_SAMPLES: usize = (POINTS - 2) * SAMPLES_PER_SEG + 2;

#[derive(Clone, Copy)]
pub struct Rope {
    p: [Vec2; POINTS],
    prev: [Vec2; POINTS],
    rest: f32,
}

/// Where the cable stays: x within `min_x..=max_x`, y no lower than `floor`.
#[derive(Clone, Copy)]
pub struct Bounds {
    pub min_x: f32,
    pub max_x: f32,
    pub floor: f32,
}

impl Rope {
    /// Lay the rope out between `a` and `b` already sagging, so it does not
    /// buckle on the first steps. `side` (±1) leans the belly sideways, which
    /// breaks the symmetry of vertical runs so their slack swings out.
    pub fn new(a: Vec2, b: Vec2, side: f32) -> Self {
        let mut p = [Vec2::ZERO; POINTS];
        for (i, q) in p.iter_mut().enumerate() {
            let t = i as f32 / (POINTS - 1) as f32;
            let belly = (t * std::f32::consts::PI).sin();
            *q = a + (b - a) * t + Vec2::new(side * 24.0, 70.0) * belly;
        }
        Self { p, prev: p, rest: Self::rest_for(a, b) }
    }

    fn rest_for(a: Vec2, b: Vec2) -> f32 {
        ((b - a).length() * SLACK + SLACK_EXTRA) / (POINTS - 1) as f32
    }

    /// False once any point went NaN/infinite (an unlaid jack, rule 32);
    /// such a rope never recovers and must be laid out again.
    pub fn is_finite(&self) -> bool {
        self.p.iter().chain(self.prev.iter()).all(|q| q.x.is_finite() && q.y.is_finite())
    }

    pub fn start(&self) -> Vec2 { self.p[0] }
    pub fn end(&self) -> Vec2 { self.p[POINTS - 1] }

    /// One fixed step. `None` leaves that end free. While both ends are
    /// pinned the rope length follows their distance, so stretching a patch
    /// never snaps it taut; a free end keeps the last length. `slack_mul`
    /// scales the length (audio cables breathe with their level).
    pub fn step(&mut self, a: Option<Vec2>, b: Option<Vec2>, slack_mul: f32, bounds: Bounds) {
        if let (Some(a), Some(b)) = (a, b) {
            self.rest = Self::rest_for(a, b) * slack_mul;
        }
        let keep = DAMPING.powf(STEP * 60.0);
        let fall = Vec2::new(0.0, GRAVITY * STEP * STEP);
        for (p, prev) in self.p.iter_mut().zip(self.prev.iter_mut()) {
            let v = (*p - *prev) * keep;
            *prev = *p;
            *p += v + fall;
        }
        for _ in 0..ITERATIONS {
            self.pin(a, b);
            for i in 0..POINTS - 1 {
                let d = self.p[i + 1] - self.p[i];
                let dist = d.length().max(1e-3);
                let fix = d * ((dist - self.rest) / dist / 2.0);
                self.p[i] += fix;
                self.p[i + 1] -= fix;
            }
        }
        self.pin(a, b);
        for p in &mut self.p {
            p.x = p.x.clamp(bounds.min_x, bounds.max_x);
            p.y = p.y.min(bounds.floor);
        }
    }

    fn pin(&mut self, a: Option<Vec2>, b: Option<Vec2>) {
        if let Some(a) = a { self.p[0] = a; }
        if let Some(b) = b { self.p[POINTS - 1] = b; }
    }

    /// Add velocity (px/s) to point `i`.
    pub fn kick(&mut self, i: usize, v: Vec2) {
        self.prev[i] -= v * STEP;
    }

    /// Kick the whole rope, weighted per point by `w(t)` with t in 0..=1.
    pub fn kick_all(&mut self, v: Vec2, w: impl Fn(f32) -> f32) {
        for i in 0..POINTS {
            self.kick(i, v * w(i as f32 / (POINTS - 1) as f32));
        }
    }

    /// Drag the point nearest `u` (0..=1 along the cable) toward `to`. Moving
    /// the position without its history gives it velocity, so a release swings.
    pub fn pull(&mut self, u: f32, to: Vec2, strength: f32) {
        let i = ((u * (POINTS - 1) as f32).round() as usize).clamp(1, POINTS - 2);
        self.p[i] += (to - self.p[i]) * strength;
    }

    /// Smooth polyline through the rope: quadratic segments between segment
    /// midpoints, the curve the web toy strokes. Returns the sample count.
    pub fn sample(&self, out: &mut [Pos2; MAX_SAMPLES]) -> usize {
        let mut n = 0;
        let mut from = self.p[0];
        out[n] = from.to_pos2();
        n += 1;
        for i in 1..POINTS - 1 {
            let ctrl = self.p[i];
            let to = (self.p[i] + self.p[i + 1]) * 0.5;
            for s in 1..=SAMPLES_PER_SEG {
                let t = s as f32 / SAMPLES_PER_SEG as f32;
                let u = 1.0 - t;
                out[n] = (from * (u * u) + ctrl * (2.0 * u * t) + to * (t * t)).to_pos2();
                n += 1;
            }
            from = to;
        }
        out[n] = self.p[POINTS - 1].to_pos2();
        n + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPEN: Bounds = Bounds { min_x: -1e4, max_x: 1e4, floor: 1e4 };

    fn settle(r: &mut Rope, a: Option<Vec2>, b: Option<Vec2>, secs: f32) {
        for _ in 0..(secs / STEP) as usize { r.step(a, b, 1.0, OPEN); }
    }

    fn speed(r: &Rope) -> f32 {
        r.p.iter().zip(r.prev.iter()).map(|(p, q)| (*p - *q).length()).fold(0.0, f32::max) / STEP
    }

    #[test]
    fn patched_cable_hangs_below_its_jacks_and_comes_to_rest() {
        let (a, b) = (Vec2::new(0.0, 0.0), Vec2::new(300.0, 0.0));
        let mut r = Rope::new(a, b, 1.0);
        settle(&mut r, Some(a), Some(b), 6.0);
        assert_eq!(r.start(), a);
        assert_eq!(r.end(), b);
        let low = r.p.iter().map(|p| p.y).fold(f32::MIN, f32::max);
        assert!(low > 60.0, "cable sags under gravity, lowest point {low}");
        assert!(speed(&r) < 5.0, "cable settles, still moving at {} px/s", speed(&r));
    }

    #[test]
    fn unplugged_end_drops() {
        let (a, b) = (Vec2::new(0.0, 0.0), Vec2::new(300.0, 0.0));
        let mut r = Rope::new(a, b, 1.0);
        settle(&mut r, Some(a), Some(b), 2.0);
        settle(&mut r, Some(a), None, 0.5);
        assert!(r.end().y > 150.0, "free end fell to {}", r.end().y);
    }

    #[test]
    fn vertical_run_swings_out_to_its_side() {
        let (a, b) = (Vec2::new(0.0, 0.0), Vec2::new(0.0, 200.0));
        let mut r = Rope::new(a, b, 1.0);
        settle(&mut r, Some(a), Some(b), 0.3);
        assert!(r.p[POINTS / 2].x > 1.0, "belly leans right, x = {}", r.p[POINTS / 2].x);
    }

    #[test]
    fn rope_stays_inside_bounds() {
        let (a, b) = (Vec2::new(0.0, 0.0), Vec2::new(300.0, 0.0));
        let mut r = Rope::new(a, b, 1.0);
        let floor = Bounds { min_x: -1e4, max_x: 1e4, floor: 20.0 };
        for _ in 0..240 { r.step(Some(a), Some(b), 1.0, floor); }
        assert!(r.p.iter().all(|p| p.y <= 20.0));
    }

    #[test]
    fn sample_starts_and_ends_on_the_plugs() {
        let (a, b) = (Vec2::new(10.0, 5.0), Vec2::new(200.0, 40.0));
        let mut r = Rope::new(a, b, -1.0);
        r.step(Some(a), Some(b), 1.0, OPEN);
        let mut out = [Pos2::ZERO; MAX_SAMPLES];
        let n = r.sample(&mut out);
        assert_eq!(n, MAX_SAMPLES);
        assert_eq!(out[0], Pos2::new(10.0, 5.0));
        assert_eq!(out[n - 1], Pos2::new(200.0, 40.0));
    }
}
