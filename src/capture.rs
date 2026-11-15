//! System-audio capture (PLAN: "wire YouTube playback into the mix").
//!
//! Streams a PipeWire/Pulse **monitor** source into a lock-free ring the engine
//! consumes. Uses `parec` (PipeWire's PulseAudio compat) so no native PipeWire
//! dependency is needed. The subprocess and ring-fill run on their own thread —
//! the audio callback only pops samples (RT-safe).
//!
//! Feedback note: a sink's monitor captures everything played to that sink,
//! INCLUDING kabelsalat's own output if it targets the same sink. The engine
//! defaults the capture gain to 0 (muted); raising it while monitoring the
//! output sink can feed back. Capture a different source, or route kabelsalat
//! to a separate output, to avoid the loop.

use std::io::Read;
use std::process::{Child, Command, Stdio};

use rtrb::{Consumer, Producer, RingBuffer};

pub struct SystemCapture {
    consumer: Consumer<f32>,
    _child: Child,
    source: String,
}

impl SystemCapture {
    /// The default sink's monitor source — captures whatever plays out.
    pub fn default_monitor() -> Option<String> {
        let out = Command::new("pactl").arg("get-default-sink").output().ok()?;
        let sink = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if sink.is_empty() {
            None
        } else {
            Some(format!("{sink}.monitor"))
        }
    }

    /// Spawn `parec` on `source` and pump its float32 PCM into a ring.
    pub fn start(source: &str, sample_rate: u32, channels: usize) -> anyhow::Result<Self> {
        let cap = (sample_rate as usize) * channels * 2; // ~2 s headroom
        let (producer, consumer) = RingBuffer::new(cap);
        let mut child = Command::new("parec")
            .args([
                &format!("--device={source}"),
                "--format=float32le",
                &format!("--rate={sample_rate}"),
                &format!("--channels={channels}"),
                "--raw",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdout = child.stdout.take().expect("parec stdout piped");
        std::thread::Builder::new()
            .name("system-audio-capture".into())
            .spawn(move || pump(stdout, producer))?;
        Ok(Self { consumer, _child: child, source: source.to_string() })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// Fill `out` (stereo-interleaved) from the ring; zero-fill on underrun.
    /// Allocation-free: safe on the audio thread.
    pub fn read(&mut self, out: &mut [f32]) {
        for s in out.iter_mut() {
            *s = self.consumer.pop().unwrap_or(0.0);
        }
    }
}

fn pump(mut stdout: std::process::ChildStdout, mut producer: Producer<f32>) {
    let mut buf = [0u8; 8192];
    loop {
        match stdout.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                for chunk in buf[..n].chunks_exact(4) {
                    let s = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                    // Ring full → drop the sample; real-time capture can't block.
                    let _ = producer.push(s);
                }
            }
        }
    }
}
/// End-to-end capture self-test: play a tone to the default sink, capture it
/// via the monitor, and assert audio flows through the ring.
pub fn run_test() -> anyhow::Result<()> {
    let source = SystemCapture::default_monitor()
        .ok_or_else(|| anyhow::anyhow!("no default monitor source (pactl get-default-sink)"))?;
    let mut cap = SystemCapture::start(&source, 48000, 2)?;
    // Let parec connect to the (possibly suspended) monitor before playing.
    std::thread::sleep(std::time::Duration::from_millis(500));

    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let wav = std::path::PathBuf::from(home).join(".cache").join("ksalat-capture-test.wav");
    write_sine_wav(&wav, 48000, 2.0, 440.0)?;
    let _player = std::process::Command::new("paplay").arg(&wav).spawn()?;

    let mut buf = vec![0.0f32; 4800];
    let (mut nonzero, mut total) = (0usize, 0usize);
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2800);
    while std::time::Instant::now() < deadline {
        cap.read(&mut buf);
        for &s in &buf {
            if s.abs() > 1e-4 {
                nonzero += 1;
            }
            total += 1;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let _ = std::fs::remove_file(&wav);
    anyhow::ensure!(nonzero > 1000, "capture produced no audio ({nonzero} nonzero of {total})");
    println!("capture-test: OK — system audio captured ({nonzero} nonzero samples of {total} from {source})");
    Ok(())
}

fn write_sine_wav(path: &std::path::Path, sr: u32, secs: f32, freq: f32) -> anyhow::Result<()> {
    let n = (sr as f32 * secs) as usize;
    let mut data = Vec::with_capacity(n * 4);
    for i in 0..n {
        let t = i as f32 / sr as f32;
        let v = ((2.0 * std::f32::consts::PI * freq * t).sin() * 0.5 * 32767.0) as i16;
        data.extend_from_slice(&v.to_le_bytes());
        data.extend_from_slice(&v.to_le_bytes());
    }
    let data_len = data.len() as u32;
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&sr.to_le_bytes());
    wav.extend_from_slice(&(sr * 4).to_le_bytes());
    wav.extend_from_slice(&4u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(&data);
    std::fs::write(path, wav)?;
    Ok(())
}
