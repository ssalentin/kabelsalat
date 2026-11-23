//! File player (backing tracks) — ARCHITECTURE §8 P2.
//!
//! AUM FilePlayer parity (issue #84): loop (seamless restart), tempo sync
//! (varispeed from the file's original BPM — never time-stretch), number of
//! beats (the loop's length in beats, so the tempo is derived from the file
//! and an aligned loop never drifts), beat offset, and a play-enable that
//! starts at the next bar boundary while the transport rolls.
//!
//! Split of work:
//! - This module's thread owns every file I/O: parse the WAV (16/24-bit PCM
//!   or 32-bit float — the recorder's own format), upmix to stereo, and hand
//!   the decoded buffer to the engine through a lock-free ring. It parks on
//!   its channel between loads (rule 47).
//! - The engine owns the clock: it reads the decoded buffer at a fractional
//!   position advanced per frame. While sync is on and the master transport
//!   rolls, the position is DERIVED from the song clock (the metronome's own
//!   beat factor, rule 43), so a beats-aligned loop is locked by construction
//!   — a BPM change is followed within the same block, with no phase jump.
//!   No decoding, allocation, or file I/O on the audio thread; big buffers
//!   leave it through a retirement ring (rule 107 discipline).
//!
//! Compressed formats (mp3/flac/ogg via symphonia) are a follow-up.

use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;

use rtrb::{Producer, PushError};

/// Default original BPM when a file carries no "nnn bpm" in its name.
pub const DEFAULT_BPM: f32 = 120.0;

enum PlayerMsg {
    Play(std::path::PathBuf, u64),
}

/// What the decode thread found in the file it loaded last, beside the
/// audio (#291): the frontend reads it, the engine never does.
#[derive(Clone, Debug, PartialEq)]
pub struct FileInfo {
    /// The load this is (`PlayerControl::play`'s answer, `PlayerPub::file`).
    pub id: u64,
    pub path: std::path::PathBuf,
    pub duration: f64,
    /// The file's own cue points.
    pub cues: Vec<crate::markers::Marker>,
    /// The waveform overview (`markers::peaks`).
    pub peaks: Vec<f32>,
}

/// UI-side handle: tell the decode thread what to play.
#[derive(Clone)]
pub struct PlayerControl {
    tx: Sender<PlayerMsg>,
    next_id: Arc<std::sync::atomic::AtomicU64>,
    info: Arc<std::sync::Mutex<Option<Arc<FileInfo>>>>,
}

impl PlayerControl {
    /// Load `path` and start playback (the engine arms the file at the next
    /// bar line when sync is on and the transport rolls; see `load_player`).
    /// Answers the load's id: markers are sent for it
    /// (`Command::SetPlayerMarkers`), and the engine says which load plays.
    pub fn play(&self, path: impl Into<std::path::PathBuf>) -> u64 {
        let id = self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let _ = self.tx.send(PlayerMsg::Play(path.into(), id));
        id
    }

    /// The file loaded last, once the decode thread has read it.
    pub fn info(&self) -> Option<Arc<FileInfo>> {
        self.info.lock().ok()?.clone()
    }
}

/// A decoded file, ready for the engine: interleaved STEREO samples at the
/// file's own rate (the engine reads it at a fractional position, so the
/// varispeed rate can change every frame) plus the original BPM discovered
/// from the filename.
#[derive(Clone)]
pub struct PlayerData {
    pub samples: Arc<[f32]>,
    pub sample_rate: u32,
    /// "nnn bpm" in the filename (AUM's default for the original tempo).
    pub bpm: Option<f32>,
    /// Which load this is (`PlayerControl::play`); 0 for a buffer made in
    /// place. The engine follows only the markers sent for this id.
    pub id: u64,
}

impl PlayerData {
    pub fn frames(&self) -> usize {
        self.samples.len() / 2
    }

    pub fn duration(&self) -> f64 {
        self.frames() as f64 / self.sample_rate.max(1) as f64
    }

    /// Build from a parsed WAV: upmix mono/multichannel to stereo.
    pub fn from_wav(w: Wav) -> Self {
        let file_ch = w.channels.max(1);
        let frames = w.data.len() / file_ch;
        let mut samples = Vec::with_capacity(frames * 2);
        for f in 0..frames {
            samples.push(sample_at(&w.data, f, 0, file_ch));
            samples.push(sample_at(&w.data, f, file_ch - 1, file_ch));
        }
        Self { samples: samples.into(), sample_rate: w.sample_rate, bpm: None, id: 0 }
    }

    /// Interleaved stereo at `sample_rate`, for tests and self-tests.
    pub fn from_samples(samples: Vec<f32>, sample_rate: u32) -> Self {
        Self { samples: samples.into(), sample_rate, bpm: None, id: 0 }
    }
}

/// "nnn bpm" in the filename, AUM's default for the file's original tempo.
/// Digits (optionally with a decimal point) immediately before "bpm",
/// case-insensitive, clamped to the transport's tempo range.
pub fn filename_bpm(path: &Path) -> Option<f32> {
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    let idx = name.find("bpm")?;
    let before = name[..idx].trim_end_matches(|c: char| c.is_whitespace() || c == '_' || c == '-');
    let num: String = before
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ',')
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    if !num.chars().any(|c| c.is_ascii_digit()) {
        return None;
    }
    let bpm: f32 = num.trim_start_matches('.').replace(',', ".").parse().ok()?;
    (20.0..=300.0).contains(&bpm).then_some(bpm)
}

/// File-player playback settings (AUM FilePlayer options). Engine-owned,
/// applied to whatever file is loaded; set through `Command::SetPlayerCfg`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlayerCfg {
    /// Loop: restart the file seamlessly at its end.
    pub loop_on: bool,
    /// Sync: playback rate follows the transport tempo (varispeed).
    pub sync: bool,
    /// Number of Beats: the loop's length in beats. With loop+sync on, the
    /// file's tempo is DERIVED from it (`ref_bpm`), so one pass lasts exactly
    /// this many song beats and an aligned loop never drifts. 0 = unset.
    pub beats: f32,
    /// Beat Offset (±): where in the file playback starts, in beats.
    pub offset: f32,
    /// Original BPM: the file's own tempo (adopted from "nnn bpm" in the
    /// filename at load, overridable).
    pub bpm: f32,
}

impl Default for PlayerCfg {
    fn default() -> Self {
        Self { loop_on: false, sync: false, beats: 0.0, offset: 0.0, bpm: DEFAULT_BPM }
    }
}

impl PlayerCfg {
    /// The file's reference tempo. AUM "Number of Beats": with loop AND sync
    /// on and beats set, the tempo is derived from the file duration so one
    /// pass is exactly `beats` song beats; otherwise it is the original BPM.
    pub fn ref_bpm(&self, duration: f64) -> f64 {
        if self.loop_on && self.sync && self.beats > 0.0 && duration > 0.0 {
            self.beats as f64 * 60.0 / duration
        } else {
            self.bpm as f64
        }
    }

    /// Where playback starts, in file beats (AUM "Beat Offset", ±): the
    /// offset wrapped into one pass of the file.
    pub fn start_beats(&self, ref_bpm: f64, duration: f64) -> f64 {
        let total = (ref_bpm * duration / 60.0).max(1e-9);
        (self.offset as f64).rem_euclid(total)
    }
}

/// Read one stereo frame at fractional position `pos` (in file frames) with
/// cubic Hermite interpolation. `wrap` makes the neighbors loop around the
/// file ends, so a looping seam interpolates the file's own end→start
/// transition — no gap, no jump (test: the seam renders a whole-cycle sine
/// exactly). Allocation-free.
pub fn read_frame(data: &[f32], frames: usize, pos: f64, wrap: bool) -> [f32; 2] {
    if frames == 0 {
        return [0.0, 0.0];
    }
    let pos = if wrap {
        pos.rem_euclid(frames as f64)
    } else {
        pos.clamp(0.0, (frames - 1) as f64)
    };
    let i = pos.floor() as usize;
    let f = (pos - i as f64) as f32;
    let n = frames as isize;
    let idx = |k: isize| -> usize {
        let mut j = i as isize + k;
        if wrap {
            j = j.rem_euclid(n);
        } else {
            j = j.clamp(0, n - 1);
        }
        j as usize
    };
    let (i0, i1, i2, i3) = (idx(-1), idx(0), idx(1), idx(2));
    let mut out = [0.0f32; 2];
    for c in 0..2 {
        let (p0, p1) = (data[i0 * 2 + c], data[i1 * 2 + c]);
        let (p2, p3) = (data[i2 * 2 + c], data[i3 * 2 + c]);
        let m1 = (p2 - p0) * 0.5;
        let m2 = (p3 - p1) * 0.5;
        out[c] = (2.0 * f * f * f - 3.0 * f * f + 1.0) * p1
            + (f * f * f - 2.0 * f * f + f) * m1
            + (-2.0 * f * f * f + 3.0 * f * f) * p2
            + (f * f * f - f * f) * m2;
    }
    out
}

/// Spawn the decode thread; return the UI-side control. The thread parses
/// WAVs off both the audio and UI threads and pushes each decoded buffer
/// into `loader` (the engine pops it at block boundaries). It parks on its
/// channel between loads and exits when the app drops the engine's ring.
pub fn start(mut loader: Producer<PlayerData>) -> PlayerControl {
    let (tx, rx) = mpsc::channel();
    let info: Arc<std::sync::Mutex<Option<Arc<FileInfo>>>> = Default::default();
    let found = info.clone();
    std::thread::Builder::new()
        .name("file-player".into())
        .spawn(move || {
            while let Ok(PlayerMsg::Play(path, id)) = rx.recv() {
                match load_file_info(&path) {
                    Ok((mut data, cues)) => {
                        data.id = id;
                        let file = FileInfo { id, path, duration: data.duration(), cues, peaks: crate::markers::peaks(&data.samples) };
                        // The info first: when the engine says this load
                        // plays, the frontend already finds its markers.
                        if let Ok(mut slot) = found.lock() {
                            *slot = Some(Arc::new(file));
                        }
                        push_loaded(&mut loader, data);
                    }
                    Err(e) => eprintln!("file-player: cannot open {}: {e:#}", path.display()),
                }
            }
        })
        .expect("spawn file-player thread");
    PlayerControl { tx, next_id: Default::default(), info }
}

/// Decode a WAV file into a `PlayerData` (parse, upmix, filename BPM).
pub fn load_file(path: &Path) -> anyhow::Result<PlayerData> {
    load_file_info(path).map(|(data, _)| data)
}

/// `load_file`, and the file's cue points as markers (#291).
pub fn load_file_info(path: &Path) -> anyhow::Result<(PlayerData, Vec<crate::markers::Marker>)> {
    let bytes = std::fs::read(path)?;
    let cues = crate::markers::wav_cues(&bytes);
    let mut data = PlayerData::from_wav(parse_wav_bytes(&bytes)?);
    data.bpm = filename_bpm(path);
    Ok((data, cues))
}

/// Hand one decoded buffer to the engine. The ring holds a handful of
/// slots; the engine drains one per render block, so this parks briefly
/// rather than spinning (rule 47) and gives up if the app is gone.
fn push_loaded(loader: &mut Producer<PlayerData>, mut data: PlayerData) {
    while let Err(PushError::Full(d)) = loader.push(data) {
        if loader.is_abandoned() {
            return; // engine side dropped: nobody will ever drain this
        }
        data = d;
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

fn sample_at(data: &[f32], frame: usize, channel: usize, channels: usize) -> f32 {
    data.get(frame * channels + channel).copied().unwrap_or(0.0)
}

pub(crate) struct Wav {
    sample_rate: u32,
    channels: usize,
    /// Interleaved samples, normalized to -1..1.
    pub(crate) data: Vec<f32>,
}

pub(crate) fn parse_wav(file: &mut std::fs::File) -> anyhow::Result<Wav> {
    use std::io::Read;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    parse_wav_bytes(&bytes)
}

fn parse_wav_bytes(bytes: &[u8]) -> anyhow::Result<Wav> {
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
                _ => anyhow::bail!(unsupported(format, bits)),
            }
        }
        i = body + size + (size & 1);
    }
    if sample_rate == 0 || channels == 0 || data.is_empty() {
        anyhow::bail!("WAV missing fmt/data chunks");
    }
    Ok(Wav { sample_rate, channels, data })
}

fn unsupported(format: u16, bits: u16) -> String {
    format!("unsupported WAV (format {format}, {bits}-bit): 16/24-bit PCM or 32-bit float")
}

/// How long a WAV the player can read lasts, from its chunk headers alone:
/// the pre-show check looks at every backing track of a show (#278).
pub fn wav_seconds(path: &Path) -> anyhow::Result<f64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let mut head = [0u8; 12];
    if file.read_exact(&mut head).is_err() || &head[0..4] != b"RIFF" || &head[8..12] != b"WAVE" {
        anyhow::bail!("not a RIFF/WAVE file");
    }
    let mut fmt: Option<(u16, u16, u32, u16)> = None;
    let mut chunk = [0u8; 8];
    while file.read_exact(&mut chunk).is_ok() {
        let size = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]) as u64;
        if &chunk[0..4] == b"fmt " && size >= 16 {
            let mut b = [0u8; 26];
            let n = size.min(26) as usize;
            file.read_exact(&mut b[..n])?;
            file.seek(SeekFrom::Current((size - n as u64 + (size & 1)) as i64))?;
            let mut format = u16::from_le_bytes([b[0], b[1]]);
            if format == 0xFFFE && n >= 26 {
                format = u16::from_le_bytes([b[24], b[25]]);
            }
            let channels = u16::from_le_bytes([b[2], b[3]]);
            let rate = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
            fmt = Some((format, channels, rate, u16::from_le_bytes([b[14], b[15]])));
        } else if &chunk[0..4] == b"data" {
            let Some((format, channels, rate, bits)) = fmt.filter(|f| f.1 > 0 && f.2 > 0) else { break };
            if !matches!((format, bits), (1, 16) | (1, 24) | (3, 32)) {
                anyhow::bail!(unsupported(format, bits));
            }
            let size = size.min(len.saturating_sub(file.stream_position()?));
            return Ok(size as f64 / (channels as u64 * bits as u64 / 8 * rate as u64) as f64);
        } else {
            file.seek(SeekFrom::Current((size + (size & 1)) as i64))?;
        }
    }
    anyhow::bail!("WAV missing fmt/data chunks")
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

    /// What `wav_seconds` accepts, the player plays, and it tells the length
    /// without reading the samples.
    #[test]
    fn wav_seconds_reads_the_length_of_what_the_player_plays() {
        let dir = std::env::temp_dir().join(format!("ksalat-wav-seconds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (format, bits, ch, rate) in [(3u16, 32u16, 2u16, 48000u32), (1, 24, 1, 44100), (1, 16, 2, 44100), (1, 8, 1, 8000)] {
            let frame = (ch * bits / 8) as usize;
            let bytes = wav_bytes(format, bits, rate, ch, &vec![0u8; frame * rate as usize / 2]);
            let p = dir.join(format!("{format}-{bits}.wav"));
            std::fs::write(&p, &bytes).unwrap();
            let plays = parse(&bytes).is_ok();
            match wav_seconds(&p) {
                Ok(s) => assert!(plays && (s - 0.5).abs() < 1e-9, "{format}/{bits}: {s} s, plays {plays}"),
                Err(e) => assert!(!plays && e.to_string() == unsupported(format, bits), "{format}/{bits}: {e}"),
            }
        }
        let text = dir.join("notes.wav");
        std::fs::write(&text, b"just some text, not audio").unwrap();
        assert_eq!(wav_seconds(&text).unwrap_err().to_string(), "not a RIFF/WAVE file");
        assert!(wav_seconds(&dir.join("gone.wav")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A parsed file becomes stereo at its own rate (rule 54: the player
    /// reads what the recorder writes — float32 round trip).
    #[test]
    fn parsed_files_upmix_to_stereo_at_file_rate() {
        // Mono float32: L and R both carry the sample.
        let mono: Vec<u8> = [0.25f32, -0.75, 0.5].iter().flat_map(|f| f.to_le_bytes()).collect();
        let d = PlayerData::from_wav(parse(&wav_bytes(3, 32, 44100, 1, &mono)).unwrap());
        assert_eq!((d.sample_rate, d.frames()), (44100, 3));
        assert_eq!(&d.samples[..], &[0.25, 0.25, -0.75, -0.75, 0.5, 0.5]);

        // 3-channel float32: L = ch0, R = ch2.
        let tri: Vec<u8> = [0.1f32, 0.2, 0.3, 0.4, 0.5, 0.6].iter().flat_map(|f| f.to_le_bytes()).collect();
        let d = PlayerData::from_wav(parse(&wav_bytes(3, 32, 48000, 3, &tri)).unwrap());
        assert_eq!(&d.samples[..], &[0.1, 0.3, 0.4, 0.6]);
    }

    #[test]
    fn filename_bpm_reads_the_tempo_aum_style() {
        let bpm = |s: &str| filename_bpm(Path::new(s));
        assert_eq!(bpm("/x/take 120 bpm.wav"), Some(120.0));
        assert_eq!(bpm("/x/90BPM Groove.wav"), Some(90.0));
        assert_eq!(bpm("/x/groove-87.5bpm.wav"), Some(87.5));
        assert_eq!(bpm("/x/backing_300_bpm.wav"), Some(300.0));
        assert_eq!(bpm("/x/bpm120.wav"), None, "digits come before 'bpm'");
        assert_eq!(bpm("/x/9 bpm.wav"), None, "outside the tempo range");
        assert_eq!(bpm("/x/take.wav"), None);
        assert_eq!(bpm("/x/way too long name with bpm but no digits.wav"), None);
    }

    /// AUM "Number of Beats": with loop+sync on, the reference tempo comes
    /// from the file duration, so one pass lasts exactly `beats` song beats.
    #[test]
    fn ref_bpm_derives_from_beats_only_for_looped_sync() {
        let mut cfg = PlayerCfg::default();
        cfg.beats = 8.0;
        // 8 beats over 4 s = 120 BPM.
        assert!((cfg.ref_bpm(4.0) - 120.0).abs() < 1e-9, "loop+sync derives the tempo");
        cfg.sync = false;
        assert!((cfg.ref_bpm(4.0) - DEFAULT_BPM as f64).abs() < 1e-9, "sync off: original BPM");
        cfg.sync = true;
        cfg.loop_on = false;
        assert!((cfg.ref_bpm(4.0) - DEFAULT_BPM as f64).abs() < 1e-9, "one-shot: original BPM");
        cfg.loop_on = true;
        cfg.beats = 0.0;
        assert!((cfg.ref_bpm(4.0) - DEFAULT_BPM as f64).abs() < 1e-9, "no beats set: original BPM");
    }

    #[test]
    fn start_beats_wraps_the_offset_into_one_pass() {
        let mut cfg = PlayerCfg::default();
        cfg.beats = 4.0;
        cfg.loop_on = true;
        cfg.sync = true;
        // 4 s at 60 BPM = 4 beats: offset 5 → 1; −0.5 → 3.5.
        cfg.offset = 5.0;
        assert_eq!(cfg.start_beats(60.0, 4.0), 1.0);
        cfg.offset = -0.5;
        assert!((cfg.start_beats(60.0, 4.0) - 3.5).abs() < 1e-9);
        cfg.offset = 0.0;
        assert_eq!(cfg.start_beats(60.0, 4.0), 0.0);
    }

    /// The seam read interpolates the file's own end→start transition: a
    /// whole-cycle sine rendered across the wrap stays on the sine (cubic
    /// Hermite is exact enough that any index slip would stand out), and a
    /// rate-1 read of integer positions is bit-exact.
    #[test]
    fn read_frame_wraps_the_seam_without_a_click() {
        const SR: f64 = 48000.0;
        let frames = SR as usize; // exactly 440 cycles of 440 Hz
        let samples: Vec<f32> = (0..frames)
            .flat_map(|i| {
                let s = (440.0 * i as f64 / SR * std::f64::consts::TAU).sin() as f32;
                [s, -s]
            })
            .collect();
        // Integer positions: exact.
        assert_eq!(read_frame(&samples, frames, 0.0, true), [samples[0], samples[1]]);
        assert_eq!(read_frame(&samples, frames, 1234.0, true), [samples[2468], samples[2469]]);
        // Across the wrap the sine continues, not jumps: sample the 400
        // frames around it against the ideal.
        let mut worst = 0.0f32;
        for k in 0..400 {
            let pos = frames as f64 - 200.0 + k as f64 * 0.25;
            let [l, r] = read_frame(&samples, frames, pos, true);
            let ideal = (440.0 * pos / SR * std::f64::consts::TAU).sin() as f32;
            worst = worst.max((l - ideal).abs()).max((r + ideal).abs());
        }
        assert!(worst < 2e-5, "seam read is continuous (worst {worst})");
        // No wrap: the tail clamps instead of ringing.
        let [l, _] = read_frame(&samples, frames, frames as f64 + 3.0, false);
        assert_eq!(l, samples[(frames - 1) * 2]);
    }
}

/// Headless engine fixtures shared by the unit tests and `player-test`
/// (issue #84): a real `GraphEngine` rendering block by block, a file of
/// beat-aligned bursts, and onset measurement on the song clock.
pub mod selftest {
    use super::*;
    use crate::control::{command_bus, Command, CommandBus, Graph};
    use crate::engine::{GraphEngine, GraphPub, PlayerPub};
    use crate::transport::Transport;
    use std::sync::Mutex;

    pub const OUT_SR: f64 = 48_000.0;
    /// Not the engine rate, so every test also exercises the rate conversion.
    pub const FILE_SR: u32 = 44_100;

    /// Engine + the player's channels, as `build_audio` wires them.
    pub struct Rig {
        pub engine: GraphEngine,
        pub bus: CommandBus,
        pub load: Producer<PlayerData>,
        pub back: rtrb::Consumer<Arc<[f32]>>,
        pub published: Arc<Mutex<GraphPub>>,
    }

    impl Rig {
        pub fn new() -> Self {
            let (bus, drain) = command_bus();
            let mut engine = GraphEngine::new(Graph::default(), drain, OUT_SR, vec![]);
            let published = Arc::new(Mutex::new(GraphPub::default()));
            engine.set_graph_pub(published.clone());
            let (load, rx) = rtrb::RingBuffer::new(4);
            let (back_tx, back) = rtrb::RingBuffer::new(4);
            engine.set_player_channels(rx, back_tx);
            Self { engine, bus, load, back, published }
        }

        /// Deliver a decoded file (the decode thread's job in the app).
        pub fn load(&mut self, d: PlayerData) {
            assert!(self.load.push(d).is_ok(), "player ring full");
        }

        pub fn send(&mut self, cmd: Command) {
            assert!(self.bus.send(cmd), "command bus full");
        }

        pub fn state(&self) -> PlayerPub {
            self.published.lock().unwrap().player
        }

        /// Render `frames` frames in blocks of `block`; `f` sees each block's
        /// player output plus the song beat of its first frame and the
        /// per-frame beat increment (the tempo is constant within a block).
        pub fn run(&mut self, frames: usize, block: usize, mut f: impl FnMut(&[f32], f64, f64)) {
            let mut buf = vec![0.0f32; block * 2];
            let mut done = 0;
            while done < frames {
                let n = block.min(frames - done);
                let t = self.engine.transport();
                let inc = Transport::beats_per_frame(t.bpm, OUT_SR);
                self.engine.render(&mut buf[..n * 2], 2);
                f(self.engine.player_block(n), t.beats, inc);
                done += n;
            }
        }

        /// Run and collect the song beat of every burst onset (the first
        /// loud frame after at least 2000 quiet ones).
        pub fn onsets(&mut self, frames: usize, block: usize) -> Vec<f64> {
            let mut out = Vec::new();
            let mut quiet_for = 1_000_000usize;
            self.run(frames, block, |p, b0, inc| {
                for (i, s) in p.chunks(2).enumerate() {
                    if s[0].abs() > 0.3 {
                        if quiet_for > 2000 {
                            out.push(b0 + i as f64 * inc);
                        }
                        quiet_for = 0;
                    } else {
                        quiet_for += 1;
                    }
                }
            });
            out
        }
    }

    /// A file of `frames` frames at `FILE_SR` with an instant-attack burst at
    /// every `every`-th of `beats` equal slices (a pass is `beats` beats long).
    pub fn burst_file(frames: usize, beats: usize, every: usize) -> PlayerData {
        let mut s = vec![0.0f32; frames * 2];
        for k in (0..beats).step_by(every) {
            let at = k * frames / beats;
            for n in 0..400.min(frames - at) {
                let v = 0.9 * (-(n as f32) / 40.0).exp() * (std::f32::consts::TAU * 1000.0 * n as f32 / FILE_SR as f32).cos();
                s[(at + n) * 2] = v;
                s[(at + n) * 2 + 1] = v;
            }
        }
        PlayerData::from_samples(s, FILE_SR)
    }

    /// A 4-beat loop that lasts exactly 4 beats at `bpm` (a beats-aligned loop).
    pub fn aligned_loop(bpm: f64) -> PlayerData {
        burst_file((4.0 * 60.0 / bpm * FILE_SR as f64).round() as usize, 4, 1)
    }

    /// Worst distance of any onset from the integer beat grid.
    pub fn worst_off_grid(onsets: &[f64]) -> f64 {
        onsets.iter().map(|b| (b - b.round()).abs()).fold(0.0, f64::max)
    }

    /// Frames' worth of beats at `bpm` (the tolerance unit).
    pub fn frames_to_beats(frames: f64, bpm: f64) -> f64 {
        frames * Transport::beats_per_frame(bpm, OUT_SR)
    }

    /// A whole-cycle sine file for seam measurements: 441 Hz over 1 s at
    /// `FILE_SR` is exactly 441 cycles, so loop-around is the sine itself.
    pub fn sine_loop() -> PlayerData {
        let n = FILE_SR as usize;
        let s: Vec<f32> = (0..n)
            .flat_map(|i| {
                let v = (441.0 * i as f64 / FILE_SR as f64 * std::f64::consts::TAU).sin() as f32;
                [v, -v]
            })
            .collect();
        PlayerData::from_samples(s, FILE_SR)
    }

    /// (a) Loop seam: render `secs` of the looping sine (sync off, so rate =
    /// file/engine) across several seams in odd block sizes. Returns the
    /// worst deviation from the ideal continuous sine.
    pub fn seam_check(secs: f64) -> f32 {
        let mut r = Rig::new();
        r.send(Command::SetPlayerCfg { loop_on: Some(true), sync: Some(false), beats: None, offset: None, bpm: None });
        r.load(sine_loop());
        let (mut worst, mut i) = (0.0f32, 0usize);
        r.run((secs * OUT_SR) as usize, 61, |p, _, _| {
            for s in p.chunks(2) {
                let ideal = (441.0 * i as f64 / OUT_SR * std::f64::consts::TAU).sin() as f32;
                worst = worst.max((s[0] - ideal).abs()).max((s[1] + ideal).abs());
                i += 1;
            }
        });
        worst
    }

    /// (b) Drift: an aligned loop (Number of Beats set) under a rolling
    /// transport, `secs` long. Returns (onset count, worst off-grid beats).
    pub fn drift_check(secs: f64, bpm: f64) -> (usize, f64) {
        let mut r = Rig::new();
        r.send(Command::SetBpm { bpm: bpm as f32 });
        r.send(Command::SetPlayerCfg { loop_on: Some(true), sync: Some(true), beats: Some(4.0), offset: None, bpm: Some(100.0) });
        r.load(aligned_loop(100.0));
        r.send(Command::TransportPlay);
        let on = r.onsets((secs * OUT_SR) as usize, 512);
        (on.len(), worst_off_grid(&on))
    }

    /// The same loop without Number of Beats, its file running 0.37 % off the
    /// 100 BPM it is labelled with: the contrast showing why Beats exists.
    pub fn drift_without_beats(secs: f64) -> f64 {
        let mut r = Rig::new();
        r.send(Command::SetBpm { bpm: 120.0 });
        r.send(Command::SetPlayerCfg { loop_on: Some(true), sync: Some(true), beats: Some(0.0), offset: None, bpm: Some(100.0) });
        r.load(aligned_loop(100.37));
        r.send(Command::TransportPlay);
        worst_off_grid(&r.onsets((secs * OUT_SR) as usize, 512))
    }

    /// (c) A BPM change mid-play, between beats: returns (worst off-grid
    /// before, worst off-grid after, seconds between onsets before, after).
    pub fn tempo_change_check() -> (f64, f64, f64, f64) {
        let mut r = Rig::new();
        r.send(Command::SetBpm { bpm: 120.0 });
        r.send(Command::SetPlayerCfg { loop_on: Some(true), sync: Some(true), beats: Some(4.0), offset: None, bpm: Some(100.0) });
        r.load(aligned_loop(100.0));
        r.send(Command::TransportPlay);
        let before = r.onsets(5 * 48_000 + 12_345, 512); // ends mid-beat
        r.send(Command::SetBpm { bpm: 90.0 });
        let after = r.onsets(8 * 48_000, 512);
        let gap = |v: &[f64], bpm: f64| v.windows(2).map(|w| (w[1] - w[0]) * 60.0 / bpm).sum::<f64>() / (v.len().max(2) - 1) as f64;
        (worst_off_grid(&before), worst_off_grid(&after), gap(&before, 120.0), gap(&after, 90.0))
    }
}

/// Marker fixtures and measurements (#291), shared by the unit tests and
/// `player-test`.
pub mod marker_test {
    use super::selftest::*;
    use crate::control::Command;
    use crate::markers::{Marker, MarkerAction, MarkerTable};

    /// A 16-beat file at 100 BPM with a burst every 4 beats, markers on the
    /// bar lines at file beats 4, 8 and 12.
    pub fn bar_file() -> (super::PlayerData, MarkerTable) {
        let frames = (16.0 * 0.6 * FILE_SR as f64) as usize;
        let marks: Vec<Marker> = [4.0, 8.0, 12.0].iter().enumerate().map(|(i, b)| Marker::new(format!("Bar {}", i + 1), b * 0.6)).collect();
        (burst_file(frames, 16, 4), MarkerTable::new(&marks))
    }

    /// Jump next/prev of a synced track under a rolling transport: the
    /// burst at each marker must sound ON a bar line. Returns the song
    /// beats of every burst: expected 0, 4, 8, 12.
    pub fn bar_jumps() -> Vec<f64> {
        let mut r = Rig::new();
        r.send(Command::SetBpm { bpm: 100.0 });
        r.send(Command::SetPlayerCfg { loop_on: Some(false), sync: Some(true), beats: None, offset: None, bpm: Some(100.0) });
        let (mut data, table) = bar_file();
        data.id = 7;
        r.send(Command::SetPlayerMarkers { file: 7, markers: table });
        r.load(data);
        r.run(512, 512, |_, _, _| {});
        r.send(Command::TransportPlay);
        let beat = 28_800usize; // frames per beat at 100 BPM
        let mut on = r.onsets(beat * 5 + beat * 3 / 10, 512); // to beat 5.3
        r.send(Command::PlayerMarker { action: MarkerAction::Next });
        on.extend(r.onsets(beat * 4 + beat * 2 / 10, 512)); // to beat 9.5: Next from section 1 → marker 1 at the bar, beat 8
        r.send(Command::PlayerMarker { action: MarkerAction::Prev });
        on.extend(r.onsets(beat * 4 + beat / 2, 512));
        on
    }

    /// A sine file with markers on whole cycles (441 Hz at 44.1 kHz is 100
    /// frames a cycle; 20000 and 40000 are whole cycles).
    pub fn sine_sections() -> (super::PlayerData, MarkerTable) {
        let marks = [20_000.0, 40_000.0].iter().enumerate().map(|(i, f)| Marker::new(format!("S{i}"), f / FILE_SR as f64)).collect::<Vec<_>>();
        (sine_loop(), MarkerTable::new(&marks))
    }

    fn started() -> Rig {
        let mut r = Rig::new();
        r.send(Command::SetPlayerCfg { loop_on: Some(false), sync: Some(false), beats: None, offset: None, bpm: None });
        let (mut d, t) = sine_sections();
        d.id = 3;
        r.send(Command::SetPlayerMarkers { file: 3, markers: t });
        r.load(d);
        r.run(512, 512, |_, _, _| {});
        r
    }

    /// Loop section 1 (marker 0 to marker 1) for `secs`: the worst second
    /// difference of the sine (a click is far above 2·(1−cos ω) ≈ 0.0033),
    /// its peak, and the section still playing.
    pub fn section_loop(secs: f64) -> (f32, f32, u8) {
        let mut r = started();
        r.send(Command::PlayerMarker { action: MarkerAction::Jump(0) });
        r.run(1, 1, |_, _, _| {});
        r.send(Command::PlayerMarker { action: MarkerAction::Loop(Some(true)) });
        let (mut worst, mut peak, mut hist) = (0.0f32, 0.0f32, Vec::<f32>::new());
        r.run((secs * OUT_SR) as usize, 61, |p, _, _| hist.extend(p.chunks(2).map(|s| s[0])));
        // Skip the jump's own crossfade: judge from 10 ms on.
        for w in hist[480..].windows(3) {
            worst = worst.max((w[2] - 2.0 * w[1] + w[0]).abs());
            peak = peak.max(w[1].abs());
        }
        (worst, peak, r.state().section)
    }

    /// Stop at the end of section 0 from the start: (samples sounding after
    /// the stop's fade, the position it stopped at in seconds, the peak
    /// once started again, where that start began).
    pub fn stop_at_end() -> (f32, f32, f32, f32) {
        let mut r = started();
        r.send(Command::PlayerMarker { action: MarkerAction::StopAtEnd(Some(true)) });
        // Section 0 ends at 20000 file frames = 21333 output frames.
        r.run(21_333 + 400, 61, |_, _, _| {});
        let st = r.state();
        let mut tail = 0.0f32;
        r.run(4800, 61, |p, _, _| tail = p.iter().fold(tail, |m, s| m.max(s.abs())));
        let stopped_at = r.state().pos;
        r.send(Command::SetPlayerEnable { on: true });
        let mut first = None;
        let mut peak = 0.0f32;
        r.run(2000, 61, |p, _, _| {
            peak = p.iter().fold(peak, |m, s| m.max(s.abs()));
            first.get_or_insert(p[0]);
        });
        assert!(!st.playing && st.at_end == crate::markers::AtEnd::Continue, "{st:?}");
        (tail, stopped_at, peak, r.state().pos)
    }
}

/// `kabelsalat player-test`: the real decode thread feeding a real engine,
/// then the loop seam, the beat lock over five minutes, and a tempo change
/// (issue #84). Every number printed is measured, not asserted from a model.
pub fn run_test() -> anyhow::Result<()> {
    use selftest::*;
    let path = "/usr/share/sounds/alsa/Front_Center.wav";
    anyhow::ensure!(Path::new(path).exists(), "test WAV missing: {path}");

    // 1. Decode thread → engine: a real WAV reaches the player's output.
    let mut r = Rig::new();
    let (dummy, _unused) = rtrb::RingBuffer::new(1);
    let ctrl = start(std::mem::replace(&mut r.load, dummy));
    ctrl.play(path);
    let (mut peak, mut loaded) = (0.0f32, false);
    for _ in 0..200 {
        r.run(480, 480, |p, _, _| peak = p.iter().fold(peak, |m, s| m.max(s.abs())));
        loaded = r.state().loaded;
        if loaded && peak > 0.01 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    anyhow::ensure!(loaded && peak > 0.01, "decoded file never reached the engine (loaded {loaded}, peak {peak})");
    println!("player-test: decode → engine OK (peak {peak:.3}, {:.2} s file)", r.state().duration);

    // 2. (a) Loop seam: a whole-cycle sine across 10 seams stays on the ideal sine.
    let worst = seam_check(10.0);
    anyhow::ensure!(worst < 5e-4, "loop seam deviates by {worst}");
    println!("player-test: (a) loop seam clean — 10 s across 10 seams, worst deviation from the ideal sine {worst:.2e}");

    // 3. (b) Drift: an aligned loop over five minutes at 120 BPM and one at 87.
    let tol = frames_to_beats(3.0, 120.0);
    let (n, worst) = drift_check(300.0, 120.0);
    anyhow::ensure!(n >= 590 && worst < tol, "drift: {n} onsets, worst {worst} beats (tolerance {tol})");
    println!(
        "player-test: (b) drift — 300 s @120 BPM, {n} beat onsets, worst {worst:.5} beats ({:.2} frames) off the metronome grid",
        worst / frames_to_beats(1.0, 120.0)
    );
    let (n87, worst87) = drift_check(120.0, 87.0);
    let tol87 = frames_to_beats(3.0, 87.0);
    anyhow::ensure!(n87 >= 170 && worst87 < tol87, "drift @87: {n87} onsets, worst {worst87}");
    println!("player-test: (b) drift — 120 s @87 BPM, {n87} onsets, worst {worst87:.5} beats ({:.2} frames)", worst87 / frames_to_beats(1.0, 87.0));
    let loose = drift_without_beats(20.0);
    anyhow::ensure!(loose > 0.05, "contrast case should drift (worst {loose})");
    println!("player-test: (b) contrast — the same loop on a 0.37 % off file WITHOUT Number of Beats drifts {loose:.3} beats in 20 s");

    // 4. (c) BPM change mid-beat: followed with no phase jump.
    let (before, after, gap_b, gap_a) = tempo_change_check();
    anyhow::ensure!(before < frames_to_beats(3.0, 120.0) && after < frames_to_beats(3.0, 90.0), "tempo change: off grid {before} / {after}");
    anyhow::ensure!((gap_b - 0.5).abs() < 2e-3 && (gap_a - 60.0 / 90.0).abs() < 2e-3, "beat gaps {gap_b} / {gap_a}");
    println!(
        "player-test: (c) tempo change 120→90 BPM mid-beat followed — worst off-grid {before:.5} before / {after:.5} after, beat gap {gap_b:.4} s → {gap_a:.4} s"
    );

    // 5. Markers (#291): cue points from a real file, bar-exact jumps,
    // a seamless section loop and a stop at the section's end.
    let pcm = vec![0i16; 44_100 * 2 * 2];
    let cued = std::env::temp_dir().join(format!("ksalat-player-test-{}.wav", std::process::id()));
    std::fs::write(&cued, crate::markers::wav_with_cues(44_100, 2, &pcm, &[(0, "Intro"), (44_100, "Chorus")]))?;
    let (_, cues) = load_file_info(&cued)?;
    let _ = std::fs::remove_file(&cued);
    anyhow::ensure!(cues.len() == 2 && cues[1].name == "Chorus" && cues[1].at == 1.0, "cue points: {cues:?}");
    println!("player-test: (d) a WAV's cue points read as markers: {}", cues.iter().map(|m| format!("{} @ {:.1} s", m.name, m.at)).collect::<Vec<_>>().join(", "));
    let on = marker_test::bar_jumps();
    let tol = frames_to_beats(3.0, 100.0);
    anyhow::ensure!(on.len() == 4 && [0.0, 4.0, 8.0, 12.0].iter().zip(&on).all(|(w, b)| (w - b).abs() < tol), "bar jumps: onsets at beats {on:?}");
    println!(
        "player-test: (d) synced jumps (next, then previous marker) land on the bar — bursts at song beats {} (tolerance {:.1} frames)",
        on.iter().map(|b| format!("{b:.4}")).collect::<Vec<_>>().join(", "),
        tol / frames_to_beats(1.0, 100.0)
    );
    let (worst, peak, section) = marker_test::section_loop(6.0);
    anyhow::ensure!(worst < 0.0034 && peak > 0.99 && peak < 1.001 && section == 1, "section loop: second difference {worst}, peak {peak}, section {section}");
    println!("player-test: (d) a section loops 13 times with no gap or click — worst second difference {worst:.5} (a clean 441 Hz sine is 0.0033), peak {peak:.4}");
    let (tail, at, peak, restarted) = marker_test::stop_at_end();
    anyhow::ensure!(tail == 0.0 && (at - 20_000.0 / 44_100.0).abs() < 0.002 && peak > 0.9 && restarted > at, "stop at end: tail {tail}, at {at}, peak {peak}, restarted {restarted}");
    println!("player-test: (d) stop at the section's end — silent after {at:.4} s, and the next start sounds on from there");
    println!("player-test: OK");
    Ok(())
}

#[cfg(test)]
mod engine_tests {
    use super::selftest::*;
    use crate::control::Command;

    /// (a) The loop seam is the file's own end→start transition: no click.
    #[test]
    fn loop_seam_is_clean() {
        let worst = seam_check(3.5);
        assert!(worst < 5e-4, "seam deviates from the ideal sine by {worst}");
    }

    /// (b) A beats-aligned loop stays on the metronome's beat grid.
    #[test]
    fn aligned_loop_stays_locked_to_the_beat_grid() {
        let (n, worst) = drift_check(60.0, 120.0);
        assert!(n >= 100, "expected an onset per beat, got {n}");
        assert!(worst < frames_to_beats(3.0, 120.0), "drifted {worst} beats");
    }

    #[test]
    fn without_number_of_beats_a_slightly_off_file_drifts() {
        let worst = drift_without_beats(20.0);
        assert!(worst > 0.05, "contrast case should drift, worst {worst}");
    }

    /// (c) A BPM change mid-beat is followed: still on the grid, and the
    /// pass gets longer in time exactly as the tempo drops.
    #[test]
    fn a_bpm_change_mid_play_is_followed() {
        let (b, a, gap_b, gap_a) = tempo_change_check();
        let tol = frames_to_beats(3.0, 90.0);
        assert!(b < tol && a < tol, "off grid before {b} / after {a}");
        assert!((gap_b - 0.5).abs() < 2e-3, "beat gap at 120 BPM = {gap_b} s");
        assert!((gap_a - 60.0 / 90.0).abs() < 2e-3, "beat gap at 90 BPM = {gap_a} s");
    }

    /// In 3/4 the next bar line after beat 5.3 is beat 6, not 8 (#87).
    #[test]
    fn play_enable_waits_for_the_bar_line_of_the_time_signature() {
        let mut r = Rig::new();
        r.send(Command::SetBpm { bpm: 120.0 });
        r.send(Command::SetTimeSignature { num: 3, den: 4 });
        r.send(Command::SetPlayerCfg { loop_on: Some(true), sync: Some(true), beats: Some(4.0), offset: None, bpm: Some(100.0) });
        r.send(Command::TransportPlay);
        r.run((2.65 * OUT_SR) as usize, 512, |_, _, _| {}); // beat 5.3
        r.load(aligned_loop(100.0));
        let on = r.onsets(5 * 48_000, 512);
        assert!(r.state().playing && !r.state().pending, "{:?}", r.state());
        assert!((on[0] - 6.0).abs() < frames_to_beats(3.0, 120.0), "first onset at beat {}, wanted the bar line 6", on[0]);
    }

    /// Sync + a rolling transport: enabling mid-bar waits for the NEXT bar
    /// line (beat 8 after beat 5.3), and the loop then sits on the grid.
    #[test]
    fn play_enable_starts_at_the_next_bar_boundary() {
        let mut r = Rig::new();
        r.send(Command::SetBpm { bpm: 120.0 });
        r.send(Command::SetPlayerCfg { loop_on: Some(true), sync: Some(true), beats: Some(4.0), offset: None, bpm: Some(100.0) });
        r.send(Command::TransportPlay);
        r.run((2.65 * OUT_SR) as usize, 512, |_, _, _| {}); // beat 5.3
        r.load(aligned_loop(100.0));
        let on = r.onsets(5 * 48_000, 512);
        assert!(r.state().playing && !r.state().pending, "{:?}", r.state());
        let first = on[0];
        assert!((first - 8.0).abs() < frames_to_beats(3.0, 120.0), "first onset at beat {first}, wanted the bar line 8");
        assert!(worst_off_grid(&on) < frames_to_beats(3.0, 120.0));
    }

    /// While waiting for the bar line the player is pending, silent.
    #[test]
    fn a_pending_start_is_silent_until_the_bar() {
        let mut r = Rig::new();
        r.send(Command::SetBpm { bpm: 120.0 });
        r.send(Command::SetPlayerCfg { loop_on: Some(true), sync: Some(true), beats: Some(4.0), offset: None, bpm: Some(100.0) });
        r.send(Command::TransportPlay);
        r.run((2.65 * OUT_SR) as usize, 512, |_, _, _| {}); // beat 5.3
        r.load(aligned_loop(100.0));
        let mut peak = 0.0f32;
        r.run((1.0 * OUT_SR) as usize, 512, |p, _, _| peak = p.iter().fold(peak, |m, s| m.max(s.abs()))); // to beat 7.3
        assert!(r.state().pending && r.state().enabled && !r.state().playing, "{:?}", r.state());
        assert_eq!(peak, 0.0, "nothing sounds before the bar line");
    }

    /// Without sync the file starts at once and ignores the transport.
    #[test]
    fn without_sync_it_starts_now_at_its_own_rate() {
        let mut r = Rig::new();
        r.send(Command::SetBpm { bpm: 120.0 });
        r.send(Command::TransportPlay);
        r.send(Command::SetPlayerCfg { loop_on: Some(false), sync: Some(false), beats: None, offset: None, bpm: None });
        r.run(4800, 512, |_, _, _| {});
        r.load(aligned_loop(100.0));
        let mut first = None;
        let mut at = 0usize;
        r.run(4800, 512, |p, _, _| {
            for s in p.chunks(2) {
                if first.is_none() && s[0].abs() > 0.3 {
                    first = Some(at);
                }
                at += 1;
            }
        });
        assert_eq!(first, Some(0), "starts on the first frame of its block");
        assert!(r.state().playing);
    }

    /// A count-in is not the song: the file waits for the downbeat after it.
    #[test]
    fn a_synced_player_lands_on_beat_zero_after_the_count_in() {
        let mut r = Rig::new();
        r.send(Command::SetBpm { bpm: 120.0 });
        r.send(Command::SetCountIn { enabled: true, bars: 1 });
        r.send(Command::SetPlayerCfg { loop_on: Some(true), sync: Some(true), beats: Some(4.0), offset: None, bpm: Some(100.0) });
        r.load(aligned_loop(100.0));
        r.run(2000, 512, |_, _, _| {}); // loaded, transport stopped: sounding now
        r.send(Command::TransportPlay);
        let mut during = 0.0f32;
        let mut frame = 0usize;
        let mut first = None;
        r.run(4 * 48_000, 480, |p, _, _| {
            for s in p.chunks(2) {
                if frame < 96_000 {
                    during = during.max(s[0].abs()); // the 2 s count-in
                } else if first.is_none() && s[0].abs() > 0.3 {
                    first = Some(frame);
                }
                frame += 1;
            }
        });
        assert_eq!(during, 0.0, "silent during the count-in");
        let first = first.expect("the file starts after the count-in");
        assert!((first as i64 - 96_000).abs() <= 3, "first onset at frame {first}, wanted 96000");
    }

    /// Beat Offset shifts the start: +1 beat on a file with one burst per pass
    /// puts the burst 3 beats after the downbeat.
    #[test]
    fn beat_offset_shifts_the_start_point() {
        let mut r = Rig::new();
        r.send(Command::SetBpm { bpm: 100.0 });
        r.send(Command::SetPlayerCfg { loop_on: Some(true), sync: Some(true), beats: Some(4.0), offset: Some(1.0), bpm: Some(100.0) });
        let frames = (4.0 * 60.0 / 100.0 * FILE_SR as f64) as usize;
        r.load(burst_file(frames, 4, 4)); // a single burst at file beat 0
        r.run(512, 512, |_, _, _| {}); // loaded while the transport is stopped
        r.send(Command::TransportPlay); // arms the start for beat 0
        let on = r.onsets(10 * 48_000, 512);
        assert!((on[0] - 3.0).abs() < frames_to_beats(3.0, 100.0), "burst at beat {}", on[0]);
        assert!((on[1] - 7.0).abs() < frames_to_beats(3.0, 100.0), "next pass at beat {}", on[1]);
    }

    /// A one-shot ends and stays silent; Loop switched on revives it.
    #[test]
    fn a_one_shot_stops_at_its_end_and_loop_revives_it() {
        let mut r = Rig::new();
        r.send(Command::SetPlayerCfg { loop_on: Some(false), sync: Some(false), beats: None, offset: None, bpm: None });
        r.load(sine_loop());
        r.run(2 * 48_000, 512, |_, _, _| {});
        assert!(r.state().loaded && !r.state().playing, "{:?}", r.state());
        let mut tail = 0.0f32;
        r.run(4800, 512, |p, _, _| tail = p.iter().fold(tail, |m, s| m.max(s.abs())));
        assert_eq!(tail, 0.0);
        r.send(Command::SetPlayerCfg { loop_on: Some(true), sync: None, beats: None, offset: None, bpm: None });
        let mut peak = 0.0f32;
        r.run(4800, 512, |p, _, _| peak = p.iter().fold(peak, |m, s| m.max(s.abs())));
        assert!(peak > 0.5 && r.state().playing, "loop on restarts it ({peak})");
    }

    /// The replaced file goes back through the retire ring, never dropped on
    /// the audio thread, and the filename BPM is adopted on load.
    #[test]
    fn a_replaced_file_is_retired_off_the_audio_thread() {
        let mut r = Rig::new();
        r.load(sine_loop());
        r.run(512, 512, |_, _, _| {});
        let mut d = aligned_loop(100.0);
        d.bpm = Some(87.5);
        r.load(d);
        r.run(1024, 512, |_, _, _| {}); // the publication trails the load by a block
        let back = r.back.pop().expect("old buffer handed back");
        assert_eq!(back.len(), FILE_SR as usize * 2);
        assert_eq!(r.state().bpm, 87.5, "filename BPM adopted");
    }

    /// #291: next/previous marker of a synced track land on the bar line.
    #[test]
    fn synced_marker_jumps_land_on_the_bar() {
        use super::selftest::*;
        let on = super::marker_test::bar_jumps();
        assert_eq!(on.len(), 4, "{on:?}");
        for (want, got) in [0.0, 4.0, 8.0, 12.0].iter().zip(&on) {
            assert!((want - got).abs() < frames_to_beats(3.0, 100.0), "burst at beat {got}, wanted {want}: {on:?}");
        }
    }

    /// #291: a looped section repeats with no gap or click at the seam.
    #[test]
    fn a_looped_section_repeats_without_a_click() {
        let (worst, peak, section) = super::marker_test::section_loop(3.0);
        assert!(worst < 0.0034, "second difference {worst}");
        assert!(peak > 0.99 && peak < 1.001, "peak {peak}");
        assert_eq!(section, 1);
    }

    /// #291: stop at the section end, silent, and the next start goes on.
    #[test]
    fn stop_at_the_sections_end_resumes_from_the_next_section() {
        let (tail, at, peak, restarted) = super::marker_test::stop_at_end();
        assert_eq!(tail, 0.0);
        assert!((at - 20_000.0 / 44_100.0).abs() < 0.002, "stopped at {at}");
        assert!(peak > 0.9 && restarted > at, "restart peak {peak}, pos {restarted}");
    }

    /// Markers sent for another load do not apply to this one.
    #[test]
    fn markers_of_another_file_are_ignored() {
        use super::selftest::*;
        let mut r = Rig::new();
        let (_, table) = super::marker_test::sine_sections();
        r.send(Command::SetPlayerMarkers { file: 99, markers: table });
        let mut d = sine_loop();
        d.id = 1;
        r.load(d);
        r.run(512, 512, |_, _, _| {});
        assert_eq!(r.state().markers, 0);
        r.send(Command::PlayerMarker { action: crate::markers::MarkerAction::Jump(0) });
        r.run(512, 512, |_, _, _| {});
        assert!(r.state().pos < 0.05, "no jump: {}", r.state().pos);
    }

    /// Backing tracks default to stop-at-end (`loop_on == false`): loading a
    /// track with default config plays through once and stops silently at end.
    #[test]
    fn backing_track_defaults_to_stop_at_end_without_looping() {
        assert!(!super::PlayerCfg::default().loop_on, "default player config must not loop");
        let mut r = Rig::new();
        r.load(sine_loop());
        let mut played_peak = 0.0f32;
        r.run(2 * 48_000, 512, |p, _, _| played_peak = p.iter().fold(played_peak, |m, s| m.max(s.abs())));
        assert!(played_peak > 0.5, "backing track played audio ({played_peak})");
        assert!(r.state().loaded && !r.state().playing && !r.state().loop_on, "stopped at end: {:?}", r.state());
        let mut after_peak = 0.0f32;
        r.run(24_000, 512, |p, _, _| after_peak = p.iter().fold(after_peak, |m, s| m.max(s.abs())));
        assert_eq!(after_peak, 0.0, "silent after track end, no unexpected loop");
    }
}
