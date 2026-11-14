# P1 built-in DSP suite (task-7)

`src/dsp.rs` — eight hand-rolled, RT-safe stereo processors behind the
`Processor` trait, plus `build_processor(&DspKind, sample_rate)`, the factory
the engine uses to instantiate `NodeKind::Dsp` graph nodes (task-10 wiring).
No new dependencies; everything is std-only.

## RT guarantees

- **Allocation-free, lock-free, syscall-free `process` and `set_param`.**
  All buffers (delay lines, lookahead ring, reverb tanks) are allocated in
  constructors. Proven by a test: a thread-local counting `#[global_allocator]`
  (test builds only) measures zero allocations across 64 blocks × 8 nodes with
  every parameter swept live (`process_and_set_param_are_allocation_free`).
- Stereo interleaved `f32`, processed in place. `frames` is defensively
  clamped to the buffer length.
- Knob changes are smoothed per sample (one-pole `Slew`, f64 state so large
  targets like delay-in-frames converge exactly). Biquad/SVF coefficients are
  rebuilt on `set_param` (block boundary), never per sample.
- Denormals flushed to zero in all feedback/state paths.
- Parameters are normalized **0..1**, clamped; unknown ids are ignored.
  `param_count()` / `param_name(id)` expose the tables below for knobs,
  MIDI-learn, and the control plane's `SetParam`.

## Nodes and parameters

Defaults are the constructor state (all nodes are immediately usable).

### Gain — `DspKind::Gain`
| id | name | mapping | default |
|----|------|---------|---------|
| 0 | gain | amplitude = (2v)²; 0 = mute, 0.5 = unity, 1 = +12 dB | 0.5 (unity) |

### Pan — `DspKind::Pan`
Constant-power: L = cos(v·π/2), R = sin(v·π/2).
| id | name | mapping | default |
|----|------|---------|---------|
| 0 | pan | 0 = hard left, 0.5 = center (−3 dB/side), 1 = hard right | 0.5 |

### Eq3 — `DspKind::Eq3`
Three cascaded RBJ biquads per channel (low shelf → peaking mid → high shelf),
transposed direct form II.
| id | name | mapping | default |
|----|------|---------|---------|
| 0 | low_gain | −18..+18 dB | 0.5 (0 dB) |
| 1 | mid_gain | −18..+18 dB | 0.5 (0 dB) |
| 2 | high_gain | −18..+18 dB | 0.5 (0 dB) |
| 3 | low_freq | 30..300 Hz (log) | 100 Hz |
| 4 | mid_freq | 200..8000 Hz (log) | 1000 Hz |
| 5 | high_freq | 2000..16000 Hz (log) | 8000 Hz |
| 6 | mid_q | 0.3..5 (log) | 0.707 |

At 0 dB all bands are exact identity (tested).

### Filter — `DspKind::Filter`
TPT state-variable filter (Cytomic/Simper), 12 dB/oct, stable under
per-sample modulation; LP and HP taps share one state.
| id | name | mapping | default |
|----|------|---------|---------|
| 0 | cutoff | 20..20000 Hz (log); **bipolar in DJ mode** (see below) | 1.0 (open) |
| 1 | resonance | Q = 0.5·32^v (0.5 damped → 16 near self-oscillation) | 0.0 |
| 2 | mode | [0, ⅓) = LP, [⅓, ⅔) = HP, [⅔, 1] = DJ combo | 0 (LP) |

DJ combo: cutoff 0.5 = open; turning left (<0.5) sweeps a highpass up to
20 kHz at the extreme; turning right (>0.5) sweeps a lowpass down to 20 Hz.

### Limiter — `DspKind::Limiter`
Lookahead peak limiter, **fixed 5 ms lookahead** (preallocated ring). Peak
envelope with instant attack / exponential release computes gain reduction
ahead of the delayed audio; fast gain attack, knob-controlled release.
`Limiter::latency_frames()` reports the lookahead for P2 latency compensation.
| id | name | mapping | default |
|----|------|---------|---------|
| 0 | ceiling | −12..0 dB output ceiling | 1.0 (0 dBFS) |
| 1 | drive | 0..+24 dB input gain | 0.0 (unity) |
| 2 | release | 10..500 ms (log) | ≈40 ms |

Below ceiling it is bit-exact passthrough (delayed by the lookahead).

### Saturation — `DspKind::Saturation`
Odd-symmetric tanh waveshaper: wet = tanh(x·drive), bounded ±1 pre-trim.
| id | name | mapping | default |
|----|------|---------|---------|
| 0 | drive | input gain 1..40 (exponential) | 0.0 (gentle) |
| 1 | mix | dry/wet blend | 1.0 (wet) |
| 2 | output | linear trim 0..2 | 0.5 (unity) |

### Delay — `DspKind::Delay`
Max 2 s preallocated stereo line. Fractional (linear-interpolated) reads with
slewed delay time → click-free live sweeps. One-pole lowpass in the feedback
path; ping-pong cross-feeds channels. Feedback capped at 0.95 (always decays).
| id | name | mapping | default |
|----|------|---------|---------|
| 0 | time | 1..2000 ms (log) | 300 ms |
| 1 | feedback | 0..0.95 | 0.35 (→0.33) |
| 2 | mix | dry/wet | 0.5 |
| 3 | pingpong | <0.5 stereo, ≥0.5 ping-pong | 0 (stereo) |
| 4 | damping | feedback LP 20 kHz..500 Hz (0 = bright) | 0 (bright) |

### Reverb — `DspKind::Reverb`
Freeverb-style Schroeder tank: 8 damped comb filters + 4 series allpasses per
channel, classic tunings scaled from 44.1 kHz, +23-sample stereo spread on the
right channel. Fixed input gain 0.01 keeps the tank bounded at max size.
| id | name | mapping | default |
|----|------|---------|---------|
| 0 | size | comb feedback 0.70..0.98 (room size / tail length) | 0.5 |
| 1 | damping | HF absorption 0..0.4 in comb loop | 0.5 |
| 2 | mix | dry/wet | 0.3 |
| 3 | width | wet stereo width (0 = mono, 1 = full) | 1.0 |

## Tests (27 in `dsp::tests`, all green)

- **Impulse/step responses:** delay echo lands at the exact configured frame
  (±2), feedback echoes decay by the feedback ratio, ping-pong alternates
  L→R→L; limiter impulse emerges exactly `latency_frames()` later; LP step
  settles at 1, HP step settles at 0; reverb impulse excites a tail that
  decays >60 dB over 4 s.
- **Param extremes:** every node swept to 0 and 1 (and out-of-range values
  clamped) — EQ/filter stay finite and bounded; gain mute/unity/+12 dB; pan
  hard L/R with constant-power check; saturation transfer matches tanh and
  stays bounded on hot noise; limiter holds a −12 dB ceiling under +24 dB
  drive; reverb bounded on full-scale noise at max size, no damping.
- **Silence-in → silence-out:** all 8 nodes (factory test) plus dedicated
  filter/limiter/reverb checks; delay and reverb tails ring out to <1e-5.
- **Transparency:** flat EQ is identity, mix=0 passthrough for
  delay/reverb/saturation, DJ filter center open, limiter below ceiling
  bit-exact.
- **RT proof:** counting-allocator test — zero allocations in
  `process`/`set_param` across all nodes with live parameter sweeps.
- **Robustness:** oversized `frames`, zero frames, empty buffer.

## Verification (2026-11-14, aarch64, rustup)

```
CARGO_TARGET_DIR=$HOME/.cache/ksalat-dsp-target cargo test   # 78 passed; 0 failed (27 dsp)
CARGO_TARGET_DIR=$HOME/.cache/ksalat-dsp-target cargo build  # green
```

`src/dsp.rs` compiles warning-free. No other files touched; no dependencies
added; no commits (lead owns git).

## Integration notes (for task-10)

- `dsp::build_processor(kind, sr)` returns `Box<dyn Processor>` for any
  `DspKind`; construct off-thread, then move to the audio thread (`Send`).
- `SetParam { node, param, value }` maps 1:1 to `set_param(id, value as f32)`;
  values are already expected normalized.
- The limiter is the only node with latency (5 ms); `latency_frames()` is
  available when P2 compensation lands.
- The engine's interim fixed-rack `DelayLine` (engine.rs) is superseded by
  `dsp::Delay` once the generic graph renderer lands.
