//! A gig take on disk (#400): what the recorder writes beside `master.wav`
//! and the rig stems, and what Virtual Soundcheck and the stem export read.
//!
//! A take folder holds:
//!
//! - `master.wav`, `<rig name>.wav`: the mix and the post-fader rig stems;
//! - `input-<n> <name>.wav`: each Audio In node's raw stereo input, as the
//!   engine took it from the device, before channel choice and gain;
//! - `events.jsonl`: one line per event, `{"frame":N,"ev":{..}}`: the MIDI
//!   the rack was played with and the setlist part active from that frame;
//! - `take.json`: the manifest ([`Manifest`]): which file is what, the
//!   length and the song/part markers by name.
//!
//! Every WAV of a take starts at frame 0 and is 32-bit float at the engine's
//! rate, so frame N is the same moment in each of them and in the events.
//! A frame of an event is the first frame of the engine block it was applied
//! in, which is where a replay applies it again.

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::control::Command;
use crate::matrix::SourceId;

pub const MANIFEST_FILE: &str = "take.json";
pub const EVENTS_FILE: &str = "events.jsonl";

/// What a take records of the performance, small and `Copy`: it crosses the
/// audio thread's rings.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TakeEv {
    NoteOn { note: u8, velocity: f32, source: SourceId, channel: u8 },
    NoteOff { note: u8, source: SourceId, channel: u8 },
    Cc { source: SourceId, channel: u8, cc: u8, value: u8 },
    PitchBend { source: SourceId, channel: u8, value: i16 },
    Pressure { source: SourceId, channel: u8, note: Option<u8>, value: u8 },
    Program { source: SourceId, channel: u8, program: u8 },
    /// The setlist part active from here (an index into the engine's part
    /// table, setlist order); `None` = no part.
    Part { part: Option<usize> },
}

impl TakeEv {
    /// The event a command is, when it is one a take records: what was
    /// played. Edits, transport and mixer moves are not part of a take.
    pub fn of(cmd: &Command) -> Option<Self> {
        Some(match *cmd {
            Command::NoteOn { note, velocity, source, channel } => TakeEv::NoteOn { note, velocity, source, channel },
            Command::NoteOff { note, source, channel } => TakeEv::NoteOff { note, source, channel },
            Command::Cc { source, channel, cc, value } => TakeEv::Cc { source, channel, cc, value },
            Command::PitchBend { source, channel, value } => TakeEv::PitchBend { source, channel, value },
            Command::Pressure { source, channel, note, value } => TakeEv::Pressure { source, channel, note, value },
            Command::Program { source, channel, program } => TakeEv::Program { source, channel, program },
            _ => return None,
        })
    }

    /// The command that plays this event again; a part marker has none (the
    /// engine steps its own part table).
    pub fn command(self) -> Option<Command> {
        Some(match self {
            TakeEv::NoteOn { note, velocity, source, channel } => Command::NoteOn { note, velocity, source, channel },
            TakeEv::NoteOff { note, source, channel } => Command::NoteOff { note, source, channel },
            TakeEv::Cc { source, channel, cc, value } => Command::Cc { source, channel, cc, value },
            TakeEv::PitchBend { source, channel, value } => Command::PitchBend { source, channel, value },
            TakeEv::Pressure { source, channel, note, value } => Command::Pressure { source, channel, note, value },
            TakeEv::Program { source, channel, program } => Command::Program { source, channel, program },
            TakeEv::Part { .. } => return None,
        })
    }
}

/// An event at its take frame.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Timed {
    pub frame: u64,
    pub ev: TakeEv,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StemFile {
    pub rig: u8,
    pub name: String,
    pub file: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct InputFile {
    /// The Audio In node the input fed when it was recorded.
    pub node: u32,
    pub name: String,
    pub file: String,
}

/// A song/part switch: the part active from `frame` on.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    pub frame: u64,
    /// Index into the setlist's parts, in setlist order (`None` = no part).
    pub part: Option<usize>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub song: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
}

impl Marker {
    /// "Song · Part", or what is known of it.
    pub fn label(&self) -> String {
        match (self.song.is_empty(), self.name.is_empty(), self.part) {
            (false, false, _) => format!("{} · {}", self.song, self.name),
            (false, true, _) => self.song.clone(),
            (true, false, _) => self.name.clone(),
            (true, true, Some(p)) => format!("Part {}", p + 1),
            (true, true, None) => "No part".into(),
        }
    }
}

/// `take.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub sample_rate: u32,
    /// Frames in every WAV of the take; 0 while it still records (a crashed
    /// take: the length is then read from the files).
    #[serde(default)]
    pub frames: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master: Option<String>,
    #[serde(default)]
    pub stems: Vec<StemFile>,
    #[serde(default)]
    pub inputs: Vec<InputFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events: Option<String>,
    #[serde(default)]
    pub markers: Vec<Marker>,
    /// MIDI events in `events` (markers not counted).
    #[serde(default)]
    pub midi_events: u64,
    /// Hardware MIDI devices by scan index at the time of the take: the
    /// `Hardware(i)` of an event is `midi_ports[i]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub midi_ports: Vec<String>,
    /// Frames a ring could not hold (0 on a clean take).
    #[serde(default)]
    pub dropped_frames: u64,
}

impl Manifest {
    pub fn write(&self, folder: &Path) -> std::io::Result<()> {
        // Written beside and renamed: a reader never sees half a manifest.
        let tmp = folder.join(format!("{MANIFEST_FILE}.tmp"));
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(tmp, folder.join(MANIFEST_FILE))
    }
}

/// A take read from its folder.
#[derive(Clone, Debug)]
pub struct Take {
    pub folder: PathBuf,
    pub manifest: Manifest,
    /// Every event, in file order (frames never decrease).
    pub events: Vec<Timed>,
}

impl Take {
    /// Read `take.json` and `events.jsonl`. A folder from before #400 (WAVs
    /// only) opens too: its master and stems are found by name.
    pub fn open(folder: &Path) -> Result<Take, String> {
        let mut manifest = match std::fs::read(folder.join(MANIFEST_FILE)) {
            Ok(bytes) => serde_json::from_slice::<Manifest>(&bytes)
                .map_err(|e| format!("{}: {e}", folder.join(MANIFEST_FILE).display()))?,
            Err(_) => legacy_manifest(folder)?,
        };
        if manifest.frames == 0 {
            // A take that never finalized: as long as its shortest file.
            let files = manifest.master.iter().chain(manifest.stems.iter().map(|s| &s.file)).chain(manifest.inputs.iter().map(|i| &i.file));
            manifest.frames = files.filter_map(|f| WavStream::open(&folder.join(f)).ok()).map(|w| w.frames).min().unwrap_or(0);
        }
        let mut events = Vec::new();
        if let Some(name) = &manifest.events {
            if let Ok(f) = std::fs::File::open(folder.join(name)) {
                for line in BufReader::new(f).lines() {
                    let Ok(line) = line else { break };
                    // A line cut short by a crash ends the list.
                    match serde_json::from_str::<Timed>(&line) {
                        Ok(t) => events.push(t),
                        Err(_) => break,
                    }
                }
            }
        }
        if manifest.markers.is_empty() {
            manifest.markers = events
                .iter()
                .filter_map(|t| match t.ev {
                    TakeEv::Part { part } => Some(Marker { frame: t.frame, part, ..Marker::default() }),
                    _ => None,
                })
                .collect();
        }
        Ok(Take { folder: folder.to_path_buf(), manifest, events })
    }

    pub fn seconds(&self) -> f64 {
        self.manifest.frames as f64 / self.manifest.sample_rate.max(1) as f64
    }

    /// The marker in force at `frame`: the last one at or before it.
    pub fn marker_at(&self, frame: u64) -> Option<usize> {
        self.manifest.markers.iter().rposition(|m| m.frame <= frame)
    }

    /// Where marker `i`'s section ends: the next marker, or the take's end.
    pub fn marker_end(&self, i: usize) -> u64 {
        self.manifest.markers.get(i + 1).map_or(self.manifest.frames, |m| m.frame)
    }
}

fn legacy_manifest(folder: &Path) -> Result<Manifest, String> {
    let mut m = Manifest { version: 0, ..Manifest::default() };
    let dir = std::fs::read_dir(folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    let mut names: Vec<String> = dir
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.to_ascii_lowercase().ends_with(".wav"))
        .collect();
    names.sort();
    for n in names {
        let Ok(w) = WavStream::open(&folder.join(&n)) else { continue };
        m.sample_rate = w.sample_rate;
        if n == "master.wav" {
            m.master = Some(n);
        } else {
            let rig = m.stems.len() as u8;
            m.stems.push(StemFile { rig, name: n.trim_end_matches(".wav").to_string(), file: n });
        }
    }
    if m.master.is_none() && m.stems.is_empty() {
        return Err(format!("{} holds no take (no take.json and no WAV kabelsalat wrote)", folder.display()));
    }
    Ok(m)
}

/// One line of a take list.
#[derive(Clone, Debug, Serialize)]
pub struct TakeSummary {
    pub folder: String,
    pub name: String,
    pub seconds: f64,
    pub stems: usize,
    pub inputs: usize,
    pub markers: usize,
    pub midi_events: u64,
    pub master: bool,
}

/// The takes under `dir`, newest first (take folders are named by their
/// start time). Reads manifests only; run it off the UI thread.
pub fn list(dir: &Path) -> Vec<TakeSummary> {
    let mut folders: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|d| d.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
        .unwrap_or_default();
    folders.sort();
    folders.reverse();
    folders
        .iter()
        .filter_map(|f| {
            let t = Take::open(f).ok()?;
            Some(TakeSummary {
                folder: f.to_string_lossy().into_owned(),
                name: f.file_name()?.to_string_lossy().into_owned(),
                seconds: t.seconds(),
                stems: t.manifest.stems.len(),
                inputs: t.manifest.inputs.len(),
                markers: t.manifest.markers.len(),
                midi_events: t.manifest.midi_events,
                master: t.manifest.master.is_some(),
            })
        })
        .collect()
}

/// A take WAV read a piece at a time: Virtual Soundcheck streams hours of
/// input without holding them in memory. Reads what the recorder writes
/// (32-bit float) and 16/24-bit PCM.
pub struct WavStream {
    file: BufReader<std::fs::File>,
    data_start: u64,
    pub frames: u64,
    pub channels: usize,
    pub sample_rate: u32,
    format: u16,
    bits: u16,
    bytes: Vec<u8>,
}

impl WavStream {
    pub fn open(path: &Path) -> Result<Self, String> {
        let err = |e: String| format!("{}: {e}", path.display());
        let mut file = std::fs::File::open(path).map_err(|e| err(e.to_string()))?;
        let len = file.metadata().map_err(|e| err(e.to_string()))?.len();
        let mut head = [0u8; 12];
        if file.read_exact(&mut head).is_err() || &head[0..4] != b"RIFF" || &head[8..12] != b"WAVE" {
            return Err(err("not a RIFF/WAVE file".into()));
        }
        let (mut format, mut channels, mut sample_rate, mut bits) = (0u16, 0usize, 0u32, 0u16);
        let mut at = 12u64;
        loop {
            let mut ch = [0u8; 8];
            if file.seek(SeekFrom::Start(at)).is_err() || file.read_exact(&mut ch).is_err() {
                return Err(err("no data chunk".into()));
            }
            let size = u32::from_le_bytes([ch[4], ch[5], ch[6], ch[7]]) as u64;
            let body = at + 8;
            if &ch[0..4] == b"fmt " {
                let mut f = [0u8; 16];
                file.read_exact(&mut f).map_err(|e| err(e.to_string()))?;
                format = u16::from_le_bytes([f[0], f[1]]);
                channels = u16::from_le_bytes([f[2], f[3]]) as usize;
                sample_rate = u32::from_le_bytes([f[4], f[5], f[6], f[7]]);
                bits = u16::from_le_bytes([f[14], f[15]]);
            } else if &ch[0..4] == b"data" {
                if channels == 0 || sample_rate == 0 || !matches!((format, bits), (3, 32) | (1, 16) | (1, 24)) {
                    return Err(err(format!("unsupported WAV (format {format}, {bits}-bit)")));
                }
                // A take cut off by a crash has a header behind its data:
                // the file's real length decides, never more than it holds.
                let on_disk = len.saturating_sub(body);
                let size = if size == 0 || size > on_disk { on_disk } else { size };
                let frame_bytes = (channels * bits as usize / 8) as u64;
                return Ok(Self {
                    file: BufReader::with_capacity(1 << 16, file),
                    data_start: body,
                    frames: size / frame_bytes,
                    channels,
                    sample_rate,
                    format,
                    bits,
                    bytes: Vec::new(),
                });
            }
            at = body + size + (size & 1);
        }
    }

    /// Frames `from..from + n` as interleaved stereo into `out` (cleared
    /// first): a mono file on both sides, further channels left out, and
    /// silence past the end.
    pub fn read_stereo(&mut self, from: u64, n: usize, out: &mut Vec<f32>) -> Result<(), String> {
        out.clear();
        out.resize(n * 2, 0.0);
        let have = self.frames.saturating_sub(from).min(n as u64) as usize;
        if have == 0 {
            return Ok(());
        }
        let sample_bytes = self.bits as usize / 8;
        let frame_bytes = self.channels * sample_bytes;
        self.bytes.resize(have * frame_bytes, 0);
        self.file.seek(SeekFrom::Start(self.data_start + from * frame_bytes as u64)).map_err(|e| e.to_string())?;
        self.file.read_exact(&mut self.bytes).map_err(|e| e.to_string())?;
        let decode = |b: &[u8]| -> f32 {
            match (self.format, self.bits) {
                (3, 32) => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                (1, 16) => i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
                _ => (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0,
            }
        };
        for (i, frame) in self.bytes.chunks_exact(frame_bytes).enumerate() {
            let l = decode(&frame[..sample_bytes]);
            let r = if self.channels > 1 { decode(&frame[sample_bytes..2 * sample_bytes]) } else { l };
            out[i * 2] = l;
            out[i * 2 + 1] = r;
        }
        Ok(())
    }
}

/// Sample format of an exported file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    /// 24-bit PCM: what a DAW or a mastering service takes.
    #[default]
    Pcm24,
    Pcm16,
    /// 32-bit float, bit for bit what the take holds.
    Float32,
}

impl ExportFormat {
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "pcm24" | "24" => Some(Self::Pcm24),
            "pcm16" | "16" => Some(Self::Pcm16),
            "float32" | "float" | "32" => Some(Self::Float32),
            _ => None,
        }
    }

    fn bytes(self) -> usize {
        match self {
            Self::Pcm24 => 3,
            Self::Pcm16 => 2,
            Self::Float32 => 4,
        }
    }
}

/// What to export from a take.
#[derive(Clone, Debug, Default)]
pub struct ExportSpec {
    /// The full show mixdown: the take's `master.wav` (the mix as it was
    /// heard); a take without one gets the plain sum of its stems.
    pub mixdown: bool,
    /// Rig stems, by rig index; `None` = every stem of the take.
    pub stems: Option<Vec<u8>>,
    /// The raw inputs too.
    pub inputs: bool,
    /// Frame range; `None` = the whole take.
    pub range: Option<(u64, u64)>,
    pub format: ExportFormat,
    /// Where the files go; `None` = `<take>/export`, `-2`, … so an export
    /// never overwrites another.
    pub out: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Exported {
    pub folder: String,
    pub files: Vec<String>,
    pub seconds: f64,
    /// The mixdown is the sum of the stems (the take has no master).
    pub mixdown_from_stems: bool,
}

/// How far the running (or last) export is, in thousandths of its files'
/// frames. The panel shows it as a bar; `soundcheck_status` reports it.
pub static EXPORT_PROGRESS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Write the chosen files of `take`, a second at a time. Runs as long as the
/// take is long: call it on a worker thread, never the UI or audio thread.
pub fn export(take: &Take, spec: &ExportSpec) -> Result<Exported, String> {
    let m = &take.manifest;
    let (from, to) = spec.range.unwrap_or((0, m.frames));
    let to = to.min(m.frames);
    if from >= to {
        return Err(format!("nothing to export: the range {from}..{to} is empty (the take has {} frames)", m.frames));
    }
    let stems: Vec<&StemFile> = match &spec.stems {
        None => m.stems.iter().collect(),
        Some(rigs) => {
            if let Some(missing) = rigs.iter().find(|r| !m.stems.iter().any(|s| s.rig == **r)) {
                return Err(format!("the take has no stem of rig {missing} (0-based); its stems: {:?}", m.stems.iter().map(|s| s.rig).collect::<Vec<_>>()));
            }
            m.stems.iter().filter(|s| rigs.contains(&s.rig)).collect()
        }
    };
    let mixdown_from_stems = spec.mixdown && m.master.is_none();
    if mixdown_from_stems && m.stems.is_empty() {
        return Err("the take has neither a master nor stems to mix down".into());
    }
    // (output name, the files summed into it)
    let mut jobs: Vec<(String, Vec<&str>)> = Vec::new();
    if spec.mixdown {
        match &m.master {
            Some(f) => jobs.push(("mixdown".into(), vec![f])),
            None => jobs.push(("mixdown".into(), m.stems.iter().map(|s| s.file.as_str()).collect())),
        }
    }
    for s in stems {
        jobs.push((format!("stem {}", s.name), vec![&s.file]));
    }
    if spec.inputs {
        for i in &m.inputs {
            jobs.push((format!("input {}", i.name), vec![&i.file]));
        }
    }
    if jobs.is_empty() {
        return Err("nothing chosen to export: ask for the mixdown, stems or inputs".into());
    }
    let out = match &spec.out {
        Some(p) => {
            std::fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
            p.clone()
        }
        None => fresh_dir(&take.folder, "export").map_err(|e| format!("cannot create the export folder: {e}"))?,
    };
    let sr = m.sample_rate.max(1);
    let frames = to - from;
    let data_bytes = frames * 2 * spec.format.bytes() as u64;
    if data_bytes > u32::MAX as u64 - 36 {
        return Err("the range is over 4 GiB as a WAV file: export a shorter range (a song at a time)".into());
    }
    let mut files = Vec::new();
    let (mut sum, mut one, mut bytes) = (Vec::new(), Vec::new(), Vec::new());
    let total = (jobs.len() as u64 * frames).max(1);
    let mut done = 0u64;
    EXPORT_PROGRESS.store(0, std::sync::atomic::Ordering::Relaxed);
    for (name, sources) in &jobs {
        let mut streams = sources.iter().map(|f| WavStream::open(&take.folder.join(f))).collect::<Result<Vec<_>, _>>()?;
        let path = out.join(format!("{}.wav", name.replace(['/', '\\'], "-")));
        let io = |e: std::io::Error| format!("{}: {e}", path.display());
        let mut w = std::io::BufWriter::new(std::fs::File::create(&path).map_err(io)?);
        w.write_all(&wav_header(sr, spec.format, data_bytes as u32)).map_err(io)?;
        let mut at = from;
        while at < to {
            let n = (to - at).min(sr as u64) as usize;
            sum.clear();
            sum.resize(n * 2, 0.0f32);
            for s in &mut streams {
                s.read_stereo(at, n, &mut one)?;
                for (a, b) in sum.iter_mut().zip(&one) {
                    *a += *b;
                }
            }
            encode(&sum, spec.format, &mut bytes);
            w.write_all(&bytes).map_err(io)?;
            at += n as u64;
            done += n as u64;
            EXPORT_PROGRESS.store((done * 1000 / total) as u32, std::sync::atomic::Ordering::Relaxed);
        }
        w.flush().map_err(io)?;
        files.push(path.to_string_lossy().into_owned());
    }
    Ok(Exported { folder: out.to_string_lossy().into_owned(), files, seconds: frames as f64 / sr as f64, mixdown_from_stems })
}

fn fresh_dir(parent: &Path, base: &str) -> std::io::Result<PathBuf> {
    let mut n = 1u32;
    loop {
        let p = parent.join(if n == 1 { base.to_string() } else { format!("{base}-{n}") });
        match std::fs::create_dir(&p) {
            Ok(()) => return Ok(p),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(e),
        }
    }
}

fn wav_header(sample_rate: u32, format: ExportFormat, data_size: u32) -> [u8; 44] {
    let bytes = format.bytes() as u16;
    let block_align = 2 * bytes;
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data_size).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&(if format == ExportFormat::Float32 { 3u16 } else { 1 }).to_le_bytes());
    h[22..24].copy_from_slice(&2u16.to_le_bytes());
    h[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    h[28..32].copy_from_slice(&(sample_rate * block_align as u32).to_le_bytes());
    h[32..34].copy_from_slice(&block_align.to_le_bytes());
    h[34..36].copy_from_slice(&(bytes * 8).to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_size.to_le_bytes());
    h
}

/// A 32-bit float stereo WAV for tests of what reads takes (the UI's too).
#[cfg(test)]
pub(crate) fn write_test_wav(path: &Path, sr: u32, samples: &[f32]) {
    let mut bytes = wav_header(sr, ExportFormat::Float32, (samples.len() * 4) as u32).to_vec();
    for s in samples {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, bytes).unwrap();
}

/// PCM is rounded and clamped to full scale: a float take can peak above it.
fn encode(samples: &[f32], format: ExportFormat, out: &mut Vec<u8>) {
    out.clear();
    for &s in samples {
        match format {
            ExportFormat::Float32 => out.extend_from_slice(&s.to_le_bytes()),
            ExportFormat::Pcm16 => {
                let v = (s as f64 * 32768.0).round().clamp(-32768.0, 32767.0) as i16;
                out.extend_from_slice(&v.to_le_bytes());
            }
            ExportFormat::Pcm24 => {
                let v = (s as f64 * 8_388_608.0).round().clamp(-8_388_608.0, 8_388_607.0) as i32;
                out.extend_from_slice(&v.to_le_bytes()[..3]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ksalat-take-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_float(path: &Path, sr: u32, samples: &[f32]) {
        let mut bytes = wav_header(sr, ExportFormat::Float32, (samples.len() * 4) as u32).to_vec();
        encode(samples, ExportFormat::Float32, &mut Vec::new());
        for s in samples {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        std::fs::write(path, bytes).unwrap();
    }

    fn ramp(frames: usize, scale: f32) -> Vec<f32> {
        (0..frames).flat_map(|i| [i as f32 * scale, -(i as f32) * scale]).collect()
    }

    #[test]
    fn every_event_survives_its_line_in_the_file() {
        let hw = SourceId::Hardware(3);
        let all = [
            TakeEv::NoteOn { note: 60, velocity: 0.8123, source: hw, channel: 2 },
            TakeEv::NoteOff { note: 60, source: SourceId::Keys, channel: 0 },
            TakeEv::Cc { source: hw, channel: 1, cc: 74, value: 99 },
            TakeEv::PitchBend { source: hw, channel: 0, value: -8192 },
            TakeEv::Pressure { source: hw, channel: 0, note: None, value: 5 },
            TakeEv::Pressure { source: hw, channel: 0, note: Some(61), value: 6 },
            TakeEv::Program { source: hw, channel: 15, program: 127 },
            TakeEv::Part { part: Some(4) },
            TakeEv::Part { part: None },
        ];
        for (i, ev) in all.into_iter().enumerate() {
            let t = Timed { frame: i as u64 * 480, ev };
            let line = serde_json::to_string(&t).unwrap();
            assert!(!line.contains('\n'), "one line per event: {line}");
            assert_eq!(serde_json::from_str::<Timed>(&line).unwrap(), t, "{line}");
            match ev {
                TakeEv::Part { .. } => assert!(ev.command().is_none()),
                _ => assert_eq!(TakeEv::of(&ev.command().unwrap()), Some(ev), "a played event is its command again"),
            }
        }
        assert_eq!(TakeEv::of(&Command::Panic), None, "an edit is not part of a take");
    }

    #[test]
    fn a_stream_reads_any_range_and_silence_past_the_end() {
        let d = scratch("stream");
        let samples = ramp(1000, 0.001);
        write_float(&d.join("a.wav"), 48000, &samples);
        let mut w = WavStream::open(&d.join("a.wav")).unwrap();
        assert_eq!((w.frames, w.channels, w.sample_rate), (1000, 2, 48000));
        let mut out = Vec::new();
        w.read_stereo(990, 20, &mut out).unwrap();
        assert_eq!(&out[..20], &samples[1980..2000]);
        assert!(out[20..].iter().all(|x| *x == 0.0), "past the end is silence");
        w.read_stereo(3, 2, &mut out).unwrap();
        assert_eq!(out, &samples[6..10], "a read back in the file");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_take_cut_off_by_a_crash_opens_at_the_length_on_disk() {
        let d = scratch("crash");
        let samples = ramp(500, 0.001);
        // The header still says 0 bytes (never patched) and the last event
        // line is half written.
        let mut bytes = wav_header(48000, ExportFormat::Float32, 0).to_vec();
        samples.iter().for_each(|s| bytes.extend_from_slice(&s.to_le_bytes()));
        std::fs::write(d.join("master.wav"), bytes).unwrap();
        let good = serde_json::to_string(&Timed { frame: 0, ev: TakeEv::Part { part: Some(1) } }).unwrap();
        std::fs::write(d.join(EVENTS_FILE), format!("{good}\n{{\"frame\":12,\"ev\":{{\"cc")).unwrap();
        Manifest { version: 1, sample_rate: 48000, master: Some("master.wav".into()), events: Some(EVENTS_FILE.into()), ..Manifest::default() }
            .write(&d)
            .unwrap();
        let t = Take::open(&d).unwrap();
        assert_eq!(t.manifest.frames, 500);
        assert_eq!(t.events.len(), 1, "the whole lines are kept");
        assert_eq!(t.manifest.markers, vec![Marker { frame: 0, part: Some(1), ..Marker::default() }], "markers come from the events");
        let _ = std::fs::remove_dir_all(&d);
    }

    fn take_with_stems(tag: &str, master: bool) -> (PathBuf, Vec<f32>, Vec<f32>) {
        let d = scratch(tag);
        let (a, b) = (ramp(48000 + 700, 0.00001), ramp(48000 + 700, -0.000004));
        write_float(&d.join("Rig A.wav"), 48000, &a);
        write_float(&d.join("Rig B.wav"), 48000, &b);
        write_float(&d.join("input-1 Mic.wav"), 48000, &a);
        if master {
            write_float(&d.join("master.wav"), 48000, &b);
        }
        Manifest {
            version: 1,
            sample_rate: 48000,
            frames: 48700,
            master: master.then(|| "master.wav".into()),
            stems: vec![
                StemFile { rig: 0, name: "Rig A".into(), file: "Rig A.wav".into() },
                StemFile { rig: 1, name: "Rig B".into(), file: "Rig B.wav".into() },
            ],
            inputs: vec![InputFile { node: 1, name: "Mic".into(), file: "input-1 Mic.wav".into() }],
            markers: vec![
                Marker { frame: 0, part: Some(0), song: "One".into(), name: "Intro".into() },
                Marker { frame: 20000, part: Some(1), song: "One".into(), name: "Verse".into() },
            ],
            ..Manifest::default()
        }
        .write(&d)
        .unwrap();
        (d, a, b)
    }

    fn read_all(path: &str) -> (WavStream, Vec<f32>) {
        let mut w = WavStream::open(Path::new(path)).unwrap();
        let mut out = Vec::new();
        let n = w.frames as usize;
        w.read_stereo(0, n, &mut out).unwrap();
        (w, out)
    }

    #[test]
    fn a_float_export_of_a_marker_range_is_the_takes_samples() {
        let (d, a, b) = take_with_stems("export", true);
        let t = Take::open(&d).unwrap();
        assert_eq!((t.marker_at(19999), t.marker_at(20000), t.marker_end(0), t.marker_end(1)), (Some(0), Some(1), 20000, 48700));
        let spec = ExportSpec { mixdown: true, stems: Some(vec![1]), inputs: true, range: Some((20000, t.marker_end(1))), format: ExportFormat::Float32, out: None };
        let x = export(&t, &spec).unwrap();
        assert_eq!(x.files.len(), 3, "{x:?}");
        assert!(!x.mixdown_from_stems);
        let names: Vec<String> = x.files.iter().map(|f| Path::new(f).file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["mixdown.wav", "stem Rig B.wav", "input Mic.wav"]);
        assert_eq!(read_all(&x.files[0]).1, &b[40000..], "the mixdown is the master");
        assert_eq!(read_all(&x.files[1]).1, &b[40000..]);
        assert_eq!(read_all(&x.files[2]).1, &a[40000..]);
        // A second export never overwrites the first.
        let again = export(&t, &spec).unwrap();
        assert_ne!(again.folder, x.folder);
        assert!(export(&t, &ExportSpec { stems: Some(vec![7]), ..spec.clone() }).unwrap_err().contains("no stem of rig 7"));
        assert!(export(&t, &ExportSpec { range: Some((9, 9)), ..spec }).unwrap_err().contains("empty"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_take_without_a_master_mixes_its_stems_down_and_pcm_rounds() {
        let (d, a, b) = take_with_stems("mix", false);
        let t = Take::open(&d).unwrap();
        let x = export(&t, &ExportSpec { mixdown: true, stems: Some(vec![]), format: ExportFormat::Pcm24, ..ExportSpec::default() }).unwrap();
        assert!(x.mixdown_from_stems);
        assert_eq!(x.files.len(), 1);
        let (w, got) = read_all(&x.files[0]);
        assert_eq!((w.frames, w.bits, w.format), (48700, 24, 1));
        for (i, g) in got.iter().enumerate() {
            let want = a[i] + b[i];
            assert!((g - want).abs() <= 1.0 / 8_388_608.0, "sample {i}: {g} for {want}");
        }
        let x16 = export(&t, &ExportSpec { stems: Some(vec![0]), format: ExportFormat::Pcm16, ..ExportSpec::default() }).unwrap();
        let (w, got) = read_all(&x16.files[0]);
        assert_eq!(w.bits, 16);
        assert!(got.iter().zip(&a).all(|(g, w)| (g - w).abs() <= 1.0 / 32768.0));
        assert!(export(&t, &ExportSpec { stems: Some(vec![]), ..ExportSpec::default() }).unwrap_err().contains("nothing chosen"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_folder_of_wavs_from_before_the_manifest_opens_as_a_take() {
        let d = scratch("legacy");
        write_float(&d.join("master.wav"), 44100, &ramp(100, 0.001));
        write_float(&d.join("Keys.wav"), 44100, &ramp(100, 0.001));
        let t = Take::open(&d).unwrap();
        assert_eq!((t.manifest.frames, t.manifest.sample_rate, t.manifest.stems.len()), (100, 44100, 1));
        assert_eq!(t.manifest.master.as_deref(), Some("master.wav"));
        let l = list(d.parent().unwrap());
        assert!(l.iter().any(|s| s.folder == d.to_string_lossy()), "{l:?}");
        let empty = scratch("empty");
        assert!(Take::open(&empty).is_err());
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&empty);
    }
}
