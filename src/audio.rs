//! Audio I/O via cpal (ALSA host → PipeWire bridge).
//! Owned by the audio-io workstream; see PLAN.md §7 for the real-time rules.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// P0 spike: enumerate devices, play a short quiet sine through the default
/// output, report callback statistics (count, average buffer, effective latency).
pub fn run_test() -> anyhow::Result<()> {
    let host = cpal::default_host();
    println!("audio host: {}", host.id().name());

    println!("output devices:");
    for dev in host.output_devices()? {
        println!("  - {}", dev.name().unwrap_or_else(|_| "<unknown>".into()));
    }

    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no default output device found"))?;
    println!("using output: {}", device.name()?);

    let supported = device.default_output_config()?;
    println!("default config: {supported:?}");
    let sample_rate = supported.sample_rate().0 as f64;
    let channels = supported.channels() as usize;
    let config: cpal::StreamConfig = supported.into();

    // Sine state. The callback must not allocate; everything is captured by value.
    let mut phase = 0.0f64;
    let phase_inc = 440.0 / sample_rate;
    let callbacks = Arc::new(AtomicU64::new(0));
    let frames = Arc::new(AtomicU64::new(0));
    let (cb_count, frame_count) = (callbacks.clone(), frames.clone());

    let stream = device.build_output_stream(
        &config,
        move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
            cb_count.fetch_add(1, Ordering::Relaxed);
            frame_count.fetch_add((data.len() / channels) as u64, Ordering::Relaxed);
            for frame in data.chunks_mut(channels) {
                phase += phase_inc;
                if phase >= 1.0 {
                    phase -= 1.0;
                }
                let sample = (phase * std::f64::consts::TAU).sin() as f32 * 0.1;
                for s in frame.iter_mut() {
                    *s = sample;
                }
            }
        },
        |err| eprintln!("audio stream error: {err}"),
        None,
    )?;

    stream.play()?;
    println!("playing 440 Hz sine (quiet) for 3 seconds...");
    std::thread::sleep(std::time::Duration::from_secs(3));
    drop(stream);

    let n = callbacks.load(Ordering::Relaxed);
    let f = frames.load(Ordering::Relaxed);
    if n == 0 {
        anyhow::bail!("no audio callbacks ran — stream did not start");
    }
    let avg_frames = f as f64 / n as f64;
    println!("--- callback stats ---");
    println!("callbacks:        {n}");
    println!("frames:           {f}");
    println!("avg frames/cb:    {avg_frames:.1}");
    println!("effective buffer: {:.2} ms", avg_frames / sample_rate * 1000.0);
    println!("P0 audio path: OK");
    Ok(())
}
