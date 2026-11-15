//! File player (backing tracks) — PLAN P2.
//!
//! Decodes an audio file on its own thread and streams stereo f32 samples into a
//! lock-free ring the engine consumes (same RT pattern as `capture.rs`). The
//! engine only pops samples and multiplies by a gain — no decoding, allocation,
//! or file I/O on the audio thread.
//!
//! Control is split: the engine owns the `PlayerSink` (ring consumer); the UI
//! owns the `PlayerControl` (a channel to the decode thread) and calls
//! `play(path)` / `stop()`. This keeps variable-length paths off the fixed-size
//! control-plane command bus.
//!
//! First milestone: 16-bit PCM WAV (mono/stereo), linearly resampled to the
//! engine rate. Compressed formats (mp3/flac/ogg via symphonia) are a follow-up.

use std::io::Read;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

use rtrb::{Consumer, Producer, RingBuffer};

enum PlayerMsg {
    Play(PathBuf),
    Stop,
}

/// UI-side handle: tell the decode thread what to play.
#[derive(Clone)]
pub struct PlayerControl {
    tx: Sender<PlayerMsg>,
}

impl PlayerControl {
    pub fn play(&self, path: impl Into<PathBuf>) {
        let _ = self.tx.send(PlayerMsg::Play(path.into()));
    }
    pub fn stop(&self) {
        let _ = self.tx.send(PlayerMsg::Stop);
    }
}

/// Engine-side handle: pop decoded samples (RT-safe).
pub struct PlayerSink {
    consumer: Consumer<f32>,
}

impl PlayerSink {
    pub fn read(&mut self, out: &mut [f32]) {
        for s in out.iter_mut() {
            *s = self.consumer.pop().unwrap_or(0.0);
        }
    }
}

/// Spawn the decode thread; return (engine sink, UI control).
pub fn start(sample_rate: u32, channels: usize) -> (PlayerSink, PlayerControl) {
    let cap = (sample_rate as usize) * channels * 4; // ~4 s headroom
    let (producer, consumer) = RingBuffer::new(cap);
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("file-player".into())
        .spawn(move || decode_loop(rx, producer, sample_rate, channels))
        .expect("spawn file-player thread");
    (PlayerSink { consumer }, PlayerControl { tx })
}

fn decode_loop(rx: Receiver<PlayerMsg>, mut producer: Producer<f32>, sr: u32, ch: usize) {
    let mut playing = false;
    loop {
    // Wait for a command (idle) or poll for stop while playing.
    let msg = if playing {
        match rx.try_recv() {
            Ok(m) => Some(m),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => break,
        }
    } else {
        match rx.recv() {
            Ok(m) => Some(m),
            Err(_) => break,
        }
    };
    if let Some(PlayerMsg::Stop) = msg {
        playing = false;
        continue;
    }
    if let Some(PlayerMsg::Play(path)) = msg {
        playing = true;
        match std::fs::File::open(&path) {
            Ok(f) => stream_wav(f, &mut producer, sr, ch, &rx, &mut playing),
            Err(e) => eprintln!("file-player: cannot open {}: {e}", path.display()),
        }
        playing = false;
    }
    }
}

/// Decode a 16-bit PCM WAV, resample to `sr`, upmix to `ch`, push to the ring.
/// Checks `rx` for Stop between blocks so playback can be interrupted.
fn stream_wav(
    mut file: std::fs::File,
    producer: &mut Producer<f32>,
    sr: u32,
    ch: usize,
    rx: &Receiver<PlayerMsg>,
    playing: &mut bool,
) {
    let wav = match parse_wav(&mut file) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("file-player: {e}");
            return;
        }
    };
    let file_sr = wav.sample_rate.max(1);
    let file_ch = wav.channels.max(1);
    let ratio = file_sr as f64 / sr as f64;
    let frames = wav.data.len() / (2 * file_ch);
    let mut pos = 0.0f64;
    let mut block = [0.0f32; 1024];
    while *playing {
        // Check for a stop/new command without blocking.
        match rx.try_recv() {
            Ok(PlayerMsg::Stop) => {
                *playing = false;
                return;
            }
            Ok(PlayerMsg::Play(_)) => {
                // A new play request: abort this file (loop will pick it up).
                *playing = false;
                return;
            }
            Err(TryRecvError::Disconnected) => {
                *playing = false;
                return;
            }
            Err(TryRecvError::Empty) => {}
        }
        let mut n = 0;
        while n < block.len() / ch {
            let i = pos as usize;
            if i + 1 >= frames {
                *playing = false;
                break;
            }
            let fr = (pos - i as f64) as f32;
            for c in 0..ch {
                let src_c = c.min(file_ch - 1);
                let a = sample_at(&wav.data, i, src_c, file_ch);
                let b = sample_at(&wav.data, i + 1, src_c, file_ch);
                block[n * ch + c] = a * (1.0 - fr) + b * fr;
            }
            n += 1;
            pos += ratio;
        }
        for &s in &block[..n * ch] {
            if producer.push(s).is_err() {
                // Ring full: brief sleep so we don't spin-drop the whole file.
                std::thread::sleep(std::time::Duration::from_millis(2));
                let _ = producer.push(s);
            }
        }
        if n == 0 {
            break;
        }
    }
}

fn sample_at(data: &[i16], frame: usize, channel: usize, channels: usize) -> f32 {
    let idx = frame * channels + channel;
    data.get(idx).map(|&s| s as f32 / 32768.0).unwrap_or(0.0)
}

struct Wav {
    sample_rate: u32,
    channels: usize,
    data: Vec<i16>,
}

fn parse_wav(file: &mut std::fs::File) -> anyhow::Result<Wav> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        anyhow::bail!("not a RIFF/WAVE file");
    }
    let mut sample_rate = 0u32;
    let mut channels = 0usize;
    let mut bits = 0u16;
    let mut data: Vec<i16> = Vec::new();
    let mut i = 12;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let size = u32::from_le_bytes([bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]]) as usize;
        let body = i + 8;
        if id == b"fmt " && body + 16 <= bytes.len() {
            channels = u16::from_le_bytes([bytes[body + 2], bytes[body + 3]]) as usize;
            sample_rate = u32::from_le_bytes([bytes[body + 4], bytes[body + 5], bytes[body + 6], bytes[body + 7]]);
            bits = u16::from_le_bytes([bytes[body + 14], bytes[body + 15]]);
        } else if id == b"data" {
            let end = (body + size).min(bytes.len());
            if bits != 16 {
                anyhow::bail!("only 16-bit PCM WAV supported (got {bits}-bit)");
            }
            for chunk in bytes[body..end].chunks_exact(2) {
                data.push(i16::from_le_bytes([chunk[0], chunk[1]]));
            }
        }
        i = body + size + (size & 1);
    }
    if sample_rate == 0 || channels == 0 || data.is_empty() {
        anyhow::bail!("WAV missing fmt/data chunks");
    }
    Ok(Wav { sample_rate, channels, data })
}
/// End-to-end player self-test: play a system WAV and assert audio flows
/// through the ring (decode → resample → upmix → ring).
pub fn run_test() -> anyhow::Result<()> {
    let path = "/usr/share/sounds/alsa/Front_Center.wav";
    anyhow::ensure!(std::path::Path::new(path).exists(), "test WAV missing: {path}");
    let (mut sink, ctrl) = start(48000, 2);
    ctrl.play(path);
    let mut buf = vec![0.0f32; 4800];
    let (mut nonzero, mut total) = (0usize, 0usize);
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
    while std::time::Instant::now() < deadline {
        sink.read(&mut buf);
        for &s in &buf {
            if s.abs() > 1e-4 {
                nonzero += 1;
            }
            total += 1;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    ctrl.stop();
    anyhow::ensure!(nonzero > 1000, "player produced no audio ({nonzero} nonzero of {total})");
    println!("player-test: OK — backing track decoded and streamed ({nonzero} nonzero samples of {total})");
    Ok(())
}
