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
//! First milestone: 16/24-bit PCM or 32-bit float WAV (mono/stereo), linearly resampled to the
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

/// Decode a WAV (16/24-bit PCM, 32-bit float), resample to `sr`, upmix to `ch`, push to the ring.
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
    let frames = wav.data.len() / file_ch;
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

fn sample_at(data: &[f32], frame: usize, channel: usize, channels: usize) -> f32 {
    data.get(frame * channels + channel).copied().unwrap_or(0.0)
}

struct Wav {
    sample_rate: u32,
    channels: usize,
    /// Interleaved samples, normalized to -1..1.
    data: Vec<f32>,
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
    let mut format = 0u16;
    let mut data: Vec<f32> = Vec::new();
    let mut i = 12;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let size = u32::from_le_bytes([bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]]) as usize;
        let body = i + 8;
        if id == b"fmt " && body + 16 <= bytes.len() {
            format = u16::from_le_bytes([bytes[body], bytes[body + 1]]);
            // WAVE_FORMAT_EXTENSIBLE: the real format is the subformat GUID's first 2 bytes.
            if format == 0xFFFE && body + 26 <= bytes.len() {
                format = u16::from_le_bytes([bytes[body + 24], bytes[body + 25]]);
            }
            channels = u16::from_le_bytes([bytes[body + 2], bytes[body + 3]]) as usize;
            sample_rate = u32::from_le_bytes([bytes[body + 4], bytes[body + 5], bytes[body + 6], bytes[body + 7]]);
            bits = u16::from_le_bytes([bytes[body + 14], bytes[body + 15]]);
        } else if id == b"data" {
            let end = (body + size).min(bytes.len());
            // 16/24-bit PCM and 32-bit float (kabelsalat's own recordings).
            let raw = &bytes[body..end];
            match (format, bits) {
                (1, 16) => data.extend(raw.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)),
                (1, 24) => data.extend(raw.chunks_exact(3).map(|c| {
                    (i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8) as f32 / 8_388_608.0
                })),
                (3, 32) => data.extend(raw.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))),
                _ => anyhow::bail!("unsupported WAV (format {format}, {bits}-bit): 16/24-bit PCM or 32-bit float"),
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

#[cfg(test)]
mod wav_format_tests {
    use super::*;

    fn wav_bytes(format: u16, bits: u16, sr: u32, ch: u16, data: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(b"RIFF");
        b.extend(((36 + data.len()) as u32).to_le_bytes());
        b.extend(b"WAVEfmt ");
        b.extend(16u32.to_le_bytes());
        b.extend(format.to_le_bytes());
        b.extend(ch.to_le_bytes());
        b.extend(sr.to_le_bytes());
        b.extend((sr * ch as u32 * bits as u32 / 8).to_le_bytes());
        b.extend((ch * bits / 8).to_le_bytes());
        b.extend(bits.to_le_bytes());
        b.extend(b"data");
        b.extend((data.len() as u32).to_le_bytes());
        b.extend(data);
        b
    }

    fn parse(bytes: &[u8]) -> anyhow::Result<Wav> {
        let p = std::env::temp_dir().join(format!("ksalat-wav-{}-{}.wav", std::process::id(), bytes.len()));
        std::fs::write(&p, bytes)?;
        let r = parse_wav(&mut std::fs::File::open(&p)?);
        let _ = std::fs::remove_file(&p);
        r
    }

    #[test]
    fn reads_float32_recordings_pcm24_and_pcm16() {
        let floats: Vec<u8> = [0.5f32, -0.25, 1.0, -1.0].iter().flat_map(|f| f.to_le_bytes()).collect();
        let w = parse(&wav_bytes(3, 32, 48000, 2, &floats)).expect("float WAV (our own recordings)");
        assert_eq!((w.sample_rate, w.channels), (48000, 2));
        assert_eq!(w.data, vec![0.5, -0.25, 1.0, -1.0]);

        // 24-bit: +0.5 = 0x400000, -0.5 = 0xC00000.
        let w = parse(&wav_bytes(1, 24, 44100, 1, &[0x00, 0x00, 0x40, 0x00, 0x00, 0xC0])).expect("pcm24");
        assert!((w.data[0] - 0.5).abs() < 1e-6 && (w.data[1] + 0.5).abs() < 1e-6, "{:?}", w.data);

        let w = parse(&wav_bytes(1, 16, 44100, 1, &16384i16.to_le_bytes())).expect("pcm16");
        assert!((w.data[0] - 0.5).abs() < 1e-6);

        assert!(parse(&wav_bytes(1, 8, 44100, 1, &[128])).is_err(), "8-bit is refused with a reason");
    }
}
