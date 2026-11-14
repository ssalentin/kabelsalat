//! Audio I/O via cpal (ALSA host → PipeWire bridge).
//! Owned by the audio-io workstream; see PLAN.md §7 for the real-time rules.
//!
//! P0 test modes (all through `kabelsalat audio-test`):
//!
//! ```text
//! audio-test                  quick check: 512-frame buffer, 3 s quiet sine + stats
//! audio-test <buffer> <secs>  e.g. `audio-test 512 60` — 60 s soak at 512 frames
//! ```
//!
//! Environment overrides (args win over env):
//! - `KSALAT_BUFFER`   buffer frames: 128 / 256 / 512 / 1024 (default 512)
//! - `KSALAT_SECS`     test duration in seconds (default 3)
//! - `KSALAT_DUPLEX=1` also open the default input and meter it while playing
//! - `KSALAT_LOOPBACK=1` (with duplex) route input → output through an rtrb ring
//!   at low gain instead of just metering (opt-in: speaker/mic feedback risk)
//! - `KSALAT_FREQ`     sine frequency in Hz (default 440)
//!
//! Real-time discipline (PLAN.md §7): the callbacks below never allocate, lock,
//! log, or make syscalls. Statistics live in preallocated atomics; timing uses
//! `Instant::now()` (CLOCK_MONOTONIC via vDSO on aarch64 Linux — no syscall);
//! the optional duplex handoff uses a preallocated `rtrb` ring. All reporting
//! happens on the main thread.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// Upper bounds of the callback-duration histogram buckets, in microseconds.
/// One extra overflow bucket collects everything above the last bound.
const HIST_BOUNDS_US: [u64; 11] = [8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192];
const HIST_BUCKETS: usize = HIST_BOUNDS_US.len() + 1;

/// Lock-free callback statistics. Every field is written from the audio
/// callback with relaxed atomics and read by the main thread after the run.
struct Stats {
    callbacks: AtomicU64,
    frames: AtomicU64,
    /// All stream errors reported by cpal's error callback.
    errors: AtomicU64,
    /// Subset of `errors` with `ErrorKind::Xrun` (ALSA underrun/overrun).
    xruns: AtomicU64,
    /// Callbacks whose wall duration exceeded one buffer period (soft metric:
    /// a deadline miss that did not necessarily cause an audible xrun).
    over_period: AtomicU64,
    total_nanos: AtomicU64,
    max_nanos: AtomicU64,
    hist: [AtomicU64; HIST_BUCKETS],
}

impl Stats {
    fn new() -> Self {
        Self {
            callbacks: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            xruns: AtomicU64::new(0),
            over_period: AtomicU64::new(0),
            total_nanos: AtomicU64::new(0),
            max_nanos: AtomicU64::new(0),
            hist: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// Called at the end of every audio callback. No alloc, no locks.
    fn record(&self, nanos: u64, period_nanos: u64, frames: u64) {
        self.callbacks.fetch_add(1, Ordering::Relaxed);
        self.frames.fetch_add(frames, Ordering::Relaxed);
        self.total_nanos.fetch_add(nanos, Ordering::Relaxed);
        self.max_nanos.fetch_max(nanos, Ordering::Relaxed);
        if nanos > period_nanos {
            self.over_period.fetch_add(1, Ordering::Relaxed);
        }
        let micros = nanos / 1000;
        let mut bucket = HIST_BUCKETS - 1;
        for (i, &bound) in HIST_BOUNDS_US.iter().enumerate() {
            if micros < bound {
                bucket = i;
                break;
            }
        }
        self.hist[bucket].fetch_add(1, Ordering::Relaxed);
    }

    /// Main-thread reporting only.
    fn report(&self, label: &str, sample_rate: f64, period_nanos: u64) {
        let n = self.callbacks.load(Ordering::Relaxed);
        let f = self.frames.load(Ordering::Relaxed);
        let errors = self.errors.load(Ordering::Relaxed);
        let xruns = self.xruns.load(Ordering::Relaxed);
        let over = self.over_period.load(Ordering::Relaxed);
        let max_us = self.max_nanos.load(Ordering::Relaxed) as f64 / 1000.0;
        println!("--- {label} callback stats ---");
        println!("callbacks:         {n}");
        println!("frames:            {f}");
        if n > 0 {
            let avg_frames = f as f64 / n as f64;
            let avg_us = self.total_nanos.load(Ordering::Relaxed) as f64 / n as f64 / 1000.0;
            println!("avg frames/cb:     {avg_frames:.1}");
            println!("effective buffer:  {:.2} ms", avg_frames / sample_rate * 1000.0);
            println!("callback duration: avg {avg_us:.1} µs, max {max_us:.1} µs");
            println!(
                "deadline misses:   {over} (callback > {:.2} ms period)",
                period_nanos as f64 / 1e6
            );
            println!("duration histogram (µs buckets):");
            for (i, &bound) in HIST_BOUNDS_US.iter().enumerate() {
                let count = self.hist[i].load(Ordering::Relaxed);
                let pct = count as f64 / n as f64 * 100.0;
                let lower = if i == 0 { 0 } else { HIST_BOUNDS_US[i - 1] };
                println!("  [{lower:>5} .. {bound:>5}): {count:>8}  ({pct:5.1}%)");
            }
            let count = self.hist[HIST_BUCKETS - 1].load(Ordering::Relaxed);
            let pct = count as f64 / n as f64 * 100.0;
            println!(
                "  [{:>5} ..   inf): {count:>8}  ({pct:5.1}%)",
                HIST_BOUNDS_US[HIST_BOUNDS_US.len() - 1]
            );
        }
        println!("stream errors:     {errors} (xruns: {xruns})");
    }
}

struct TestConfig {
    buffer_frames: u32,
    duration: Duration,
    duplex: bool,
    loopback: bool,
    freq: f64,
}

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok()?.trim().parse().ok()
}

fn env_flag(key: &str) -> bool {
    matches!(std::env::var(key).as_deref(), Ok("1") | Ok("true"))
}

/// Positional args after the mode (`audio-test <buffer> <secs>`) win over env.
fn test_config() -> TestConfig {
    let args: Vec<String> = std::env::args().collect();
    let buffer_frames = args
        .get(2)
        .and_then(|s| s.parse::<u32>().ok())
        .or_else(|| env_u64("KSALAT_BUFFER").map(|v| v as u32))
        .unwrap_or(512);
    let secs = args
        .get(3)
        .and_then(|s| s.parse::<u64>().ok())
        .or_else(|| env_u64("KSALAT_SECS"))
        .unwrap_or(3);
    TestConfig {
        buffer_frames,
        duration: Duration::from_secs(secs.max(1)),
        duplex: env_flag("KSALAT_DUPLEX") || env_flag("KSALAT_LOOPBACK"),
        loopback: env_flag("KSALAT_LOOPBACK"),
        freq: env_u64("KSALAT_FREQ").map(|v| v as f64).unwrap_or(440.0),
    }
}

fn device_name(dev: &cpal::Device) -> String {
    dev.description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "<unknown>".into())
}

fn fixed_config(supported: &cpal::SupportedStreamConfig, buffer_frames: u32) -> cpal::StreamConfig {
    let mut config: cpal::StreamConfig = supported.clone().into();
    if let cpal::SupportedBufferSize::Range { min, max } = supported.buffer_size() {
        if buffer_frames < *min || buffer_frames > *max {
            println!(
                "warning: requested buffer {buffer_frames} outside supported range {min}..={max}"
            );
        }
    }
    config.buffer_size = cpal::BufferSize::Fixed(buffer_frames);
    config
}

/// P0 spike: enumerate devices, play a quiet sine through the default output
/// (optionally metering/looping the default input), and report callback
/// statistics: duration histogram, xrun/error counts, latency estimates.
pub fn run_test() -> anyhow::Result<()> {
    let cfg = test_config();
    let host = cpal::default_host();
    println!("audio host: {}", host.id().name());
    println!(
        "test config: buffer {} frames, {} s, duplex={}, loopback={}, freq {} Hz",
        cfg.buffer_frames,
        cfg.duration.as_secs(),
        cfg.duplex,
        cfg.loopback,
        cfg.freq
    );

    println!("output devices:");
    for dev in host.output_devices()? {
        println!("  - {}", device_name(&dev));
    }
    if cfg.duplex {
        println!("input devices:");
        for dev in host.input_devices()? {
            println!("  - {}", device_name(&dev));
        }
    }

    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no default output device found"))?;
    println!("using output: {}", device_name(&device));

    let supported = device.default_output_config()?;
    println!("default output config: {supported:?}");
    let sample_rate = supported.sample_rate() as f64;
    let channels = supported.channels() as usize;
    let config = fixed_config(&supported, cfg.buffer_frames);
    let period_nanos = (cfg.buffer_frames as f64 / sample_rate * 1e9) as u64;

    let out_stats = Arc::new(Stats::new());
    // Latest cpal playback-latency estimate (playback timestamp − callback
    // timestamp), written by the callback, read by the main thread.
    let latency_nanos = Arc::new(AtomicU64::new(0));

    // Optional duplex handoff: preallocated lock-free ring, input → output.
    let ring_capacity = cfg.buffer_frames as usize * channels * 4;
    let (mut lb_producer, mut lb_consumer) = rtrb::RingBuffer::<f32>::new(ring_capacity);
    let lb_dropped = Arc::new(AtomicU64::new(0));
    let lb_starved = Arc::new(AtomicU64::new(0));

    // ---- output stream -----------------------------------------------------
    let mut phase = 0.0f64;
    let phase_inc = cfg.freq / sample_rate;
    let (stats, lat, starved) = (
        out_stats.clone(),
        latency_nanos.clone(),
        lb_starved.clone(),
    );
    let loopback = cfg.loopback;
    let stream = device.build_output_stream(
        config,
        move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
            // vDSO clock read, not a syscall — RT-safe.
            let start = Instant::now();
            let ts = info.timestamp();
            if let Some(d) = ts.playback.checked_duration_since(ts.callback) {
                lat.store(d.as_nanos() as u64, Ordering::Relaxed);
            }
            for frame in data.chunks_mut(channels) {
                phase += phase_inc;
                if phase >= 1.0 {
                    phase -= 1.0;
                }
                let sine = (phase * std::f64::consts::TAU).sin() as f32;
                if loopback {
                    // Quiet sine stays audible as a reference; mic clamped to
                    // [-1, 1] at low gain — the default source is a processed
                    // mic whose float peaks can far exceed 1.0 (filter gain).
                    for s in frame.iter_mut() {
                        match lb_consumer.pop() {
                            Ok(mic) => *s = sine * 0.03 + mic.clamp(-1.0, 1.0) * 0.25,
                            Err(_) => {
                                starved.fetch_add(1, Ordering::Relaxed);
                                *s = sine * 0.03;
                            }
                        }
                    }
                } else {
                    let sample = sine * 0.1;
                    for s in frame.iter_mut() {
                        *s = sample;
                    }
                }
            }
            let frames = (data.len() / channels) as u64;
            stats.record(start.elapsed().as_nanos() as u64, period_nanos, frames);
        },
        {
            let stats = out_stats.clone();
            move |err: cpal::Error| {
                // No logging in the callback (RT rule); count, report later.
                stats.errors.fetch_add(1, Ordering::Relaxed);
                if matches!(err.kind(), cpal::ErrorKind::Xrun) {
                    stats.xruns.fetch_add(1, Ordering::Relaxed);
                }
            }
        },
        None,
    )?;
    stream.play()?;

    // ---- optional input stream (duplex) ------------------------------------
    let in_stats = Arc::new(Stats::new());
    let input_peak = Arc::new(AtomicU32::new(0)); // f32 bits; peak >= 0 so bit order == float order
    let mut input_stream = None;
    let mut input_rate = sample_rate;
    if cfg.duplex {
        let in_device = host
            .default_input_device()
            .ok_or_else(|| anyhow::anyhow!("no default input device found"))?;
        println!("using input: {}", device_name(&in_device));
        let in_supported = in_device.default_input_config()?;
        println!("default input config: {in_supported:?}");
        input_rate = in_supported.sample_rate() as f64;
        let in_channels = in_supported.channels() as usize;
        let in_config = fixed_config(&in_supported, cfg.buffer_frames);
        let in_period_nanos = (cfg.buffer_frames as f64 / input_rate * 1e9) as u64;
        let (stats, peak, dropped) =
            (in_stats.clone(), input_peak.clone(), lb_dropped.clone());
        let do_loopback = cfg.loopback;
        input_stream = Some(in_device.build_input_stream(
            in_config,
            move |data: &[f32], _info: &cpal::InputCallbackInfo| {
                let start = Instant::now();
                let mut max = 0.0f32;
                for &s in data {
                    let a = s.abs();
                    if a > max {
                        max = a;
                    }
                    if do_loopback && lb_producer.push(s).is_err() {
                        dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                peak.fetch_max(max.to_bits(), Ordering::Relaxed);
                let frames = (data.len() / in_channels) as u64;
                stats.record(start.elapsed().as_nanos() as u64, in_period_nanos, frames);
            },
            {
                let stats = in_stats.clone();
                move |err: cpal::Error| {
                    stats.errors.fetch_add(1, Ordering::Relaxed);
                    if matches!(err.kind(), cpal::ErrorKind::Xrun) {
                        stats.xruns.fetch_add(1, Ordering::Relaxed);
                    }
                }
            },
            None,
        )?);
        if let Some(s) = &input_stream {
            s.play()?;
        }
    }

    println!(
        "playing {} Hz sine (quiet) for {} s{}...",
        cfg.freq,
        cfg.duration.as_secs(),
        if cfg.duplex { " with duplex input" } else { "" }
    );
    let start = Instant::now();
    let mut next_tick = Duration::from_secs(10);
    while start.elapsed() < cfg.duration {
        std::thread::sleep(Duration::from_millis(250));
        if start.elapsed() >= next_tick {
            let e = out_stats.errors.load(Ordering::Relaxed);
            let x = out_stats.xruns.load(Ordering::Relaxed);
            let n = out_stats.callbacks.load(Ordering::Relaxed);
            println!(
                "[{:>3.0}s] callbacks: {n}, errors: {e}, xruns: {x}",
                start.elapsed().as_secs_f64()
            );
            next_tick += Duration::from_secs(10);
        }
    }
    drop(stream);
    drop(input_stream);

    // ---- report (main thread only) ------------------------------------------
    out_stats.report("output", sample_rate, period_nanos);
    println!(
        "buffer latency:    {:.2} ms ({} frames @ {sample_rate:.0} Hz)",
        cfg.buffer_frames as f64 / sample_rate * 1000.0,
        cfg.buffer_frames
    );
    let lat = latency_nanos.load(Ordering::Relaxed);
    if lat > 0 {
        println!(
            "cpal latency est:  {:.2} ms (playback − callback timestamp, last cb)",
            lat as f64 / 1e6
        );
    } else {
        println!("cpal latency est:  n/a (host provided no timestamps)");
    }
    if cfg.duplex {
        in_stats.report("input", input_rate, period_nanos);
        let peak = f32::from_bits(input_peak.load(Ordering::Relaxed));
        println!("input peak:        {peak:.4} ({:.1} dBFS)", 20.0 * peak.max(1e-9).log10());
        if cfg.loopback {
            println!(
                "loopback ring:     {} samples dropped (full), {} output slots starved (empty)",
                lb_dropped.load(Ordering::Relaxed),
                lb_starved.load(Ordering::Relaxed)
            );
        }
    }

    let n = out_stats.callbacks.load(Ordering::Relaxed);
    let errors = out_stats.errors.load(Ordering::Relaxed);
    let in_errors = in_stats.errors.load(Ordering::Relaxed);
    if n == 0 {
        anyhow::bail!("no audio callbacks ran — stream did not start");
    }
    if errors > 0 || in_errors > 0 {
        anyhow::bail!(
            "stream errors occurred: output {errors} ({} xruns), input {in_errors}",
            out_stats.xruns.load(Ordering::Relaxed)
        );
    }
    println!("P0 audio path: OK (0 errors, 0 xruns)");
    Ok(())
}
