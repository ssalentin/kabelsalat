//! Backing-track markers (#291): named points in the file player's file.
//! Two markers bound a section; the player can jump to a marker, loop the
//! section it is in or stop at its end, and a marker can select a part of
//! the song, so the sound follows the arrangement.
//!
//! Split of work:
//! - The app keeps markers with their names (`Marker`, saved with the song
//!   in the setlist) and sends the engine what it needs of them
//!   (`MarkerTable`: times and parts, a fixed-size copy, so the audio thread
//!   neither allocates nor frees).
//! - The decode thread reads a WAV's cue points (`wav_cues`) and the
//!   overview the waveform is drawn from (`peaks`); see `player::FileInfo`.
//! - The engine does every action on its own clock (`engine::fill_player`):
//!   a jump of a synced track waits for the next bar line, and every seam
//!   (jump, section loop, stop) is a crossfade of `FADE_SECS`.

use serde::{Deserialize, Serialize};

/// Markers per file the engine follows.
pub const MAX_MARKERS: usize = 32;

/// How long the crossfade at a jump, a section loop's seam or a stop lasts.
/// 5 ms, the engine's gain ramp (rule 202): short enough to stay on the
/// beat, long enough that a seam between unrelated material has no step.
pub const FADE_SECS: f64 = 0.005;

/// Buckets of the waveform overview (`peaks`).
pub const PEAK_BUCKETS: usize = 512;

/// A marker as the app keeps it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    pub name: String,
    /// Seconds from the file's start.
    pub at: f64,
    /// The part of the song (index into `Song::parts`) the engine switches
    /// to when playback reaches or jumps to this marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part: Option<usize>,
}

impl Marker {
    pub fn new(name: impl Into<String>, at: f64) -> Self {
        Self { name: name.into(), at, part: None }
    }
}

/// Put `markers` in file order, times finite and not negative, at most
/// `MAX_MARKERS` (the engine follows no more), two markers never on one
/// spot (the later one of a pair closer than 1 ms is dropped: a section
/// that short is a mistake, and its loop would be a buzz).
pub fn normalize(markers: &mut Vec<Marker>) {
    markers.retain(|m| m.at.is_finite());
    for m in markers.iter_mut() {
        m.at = m.at.max(0.0);
    }
    markers.sort_by(|a, b| a.at.total_cmp(&b.at));
    markers.dedup_by(|b, a| b.at - a.at < 0.001);
    markers.truncate(MAX_MARKERS);
}

/// What the engine follows of a file's markers: times in file order and the
/// part each selects. `Copy`, fixed size.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct MarkerTable {
    n: u8,
    at: [f64; MAX_MARKERS],
    /// The part of the active song a marker selects, or -1.
    part: [i16; MAX_MARKERS],
}

impl Default for MarkerTable {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl MarkerTable {
    pub const EMPTY: MarkerTable = MarkerTable { n: 0, at: [0.0; MAX_MARKERS], part: [-1; MAX_MARKERS] };

    /// From markers in any order; as `normalize` orders and limits them.
    pub fn new(markers: &[Marker]) -> Self {
        let mut m = markers.to_vec();
        normalize(&mut m);
        let mut t = Self::EMPTY;
        for (i, m) in m.iter().enumerate() {
            t.at[i] = m.at;
            t.part[i] = m.part.map_or(-1, |p| p.min(i16::MAX as usize) as i16);
        }
        t.n = m.len() as u8;
        t
    }

    pub fn len(&self) -> usize {
        self.n as usize
    }

    /// Marker `i`'s time in seconds.
    pub fn at(&self, i: usize) -> Option<f64> {
        (i < self.len()).then(|| self.at[i])
    }

    /// The part marker `i` selects.
    pub fn part(&self, i: usize) -> Option<usize> {
        (i < self.len() && self.part[i] >= 0).then(|| self.part[i] as usize)
    }

    /// The section time `t` lies in: how many markers are at or before it.
    /// Section 0 runs from the file's start to the first marker; section
    /// `s` starts at marker `s - 1`.
    pub fn section_of(&self, t: f64) -> usize {
        self.section_at(t, 1.0)
    }

    /// Section `s` as seconds `[from, to)` in a file `duration` long.
    pub fn bounds(&self, s: usize, duration: f64) -> (f64, f64) {
        self.bounds_at(s, 1.0, duration)
    }

    /// `section_of` for a position in units of `rate` per second (file
    /// frames). The engine compares in frames: a position converted to
    /// seconds and back could land one section early at a marker.
    pub fn section_at(&self, x: f64, rate: f64) -> usize {
        self.at[..self.len()].iter().take_while(|&&m| m * rate <= x).count()
    }

    /// `bounds` in units of `rate` per second, in a file `end` units long.
    pub fn bounds_at(&self, s: usize, rate: f64, end: f64) -> (f64, f64) {
        let from = if s == 0 { 0.0 } else { self.at(s - 1).map_or(end, |m| m * rate) };
        let to = self.at(s).map_or(end, |m| m * rate);
        (from.min(end), to.min(end))
    }
}

/// What happens when playback reaches the end of the section it is in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AtEnd {
    /// On into the next section.
    #[default]
    Continue,
    /// Back to the section's start, for as long as this is set.
    Loop,
    /// Stop there, once; the next start goes on from the next section.
    Stop,
}

impl AtEnd {
    pub fn id(self) -> &'static str {
        match self {
            AtEnd::Continue => "continue",
            AtEnd::Loop => "loop",
            AtEnd::Stop => "stop",
        }
    }
}

/// A marker action (`Command::PlayerMarker`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkerAction {
    /// To marker `n` (index in file order).
    Jump(u8),
    /// To the marker after the section playing (or after a jump's target
    /// that still waits for its bar line).
    Next,
    /// To the marker before the section playing.
    Prev,
    /// Loop the section playing: `None` toggles.
    Loop(Option<bool>),
    /// Stop at the end of the section playing: `None` toggles.
    StopAtEnd(Option<bool>),
}

impl AtEnd {
    /// The mode after a Loop or StopAtEnd action; a jump changes nothing.
    pub fn after(self, action: MarkerAction) -> AtEnd {
        let set = |on: Option<bool>, mode: AtEnd| match on.unwrap_or(self != mode) {
            true => mode,
            false if self == mode => AtEnd::Continue,
            false => self,
        };
        match action {
            MarkerAction::Loop(on) => set(on, AtEnd::Loop),
            MarkerAction::StopAtEnd(on) => set(on, AtEnd::Stop),
            _ => self,
        }
    }
}

/// The marker a Jump, Next or Prev goes to, from section `section` (or from
/// the target of a jump that still waits), or `None` when there is none.
pub fn target(table: &MarkerTable, section: usize, waiting: Option<usize>, action: MarkerAction) -> Option<usize> {
    let n = table.len();
    let to = match (action, waiting) {
        (MarkerAction::Jump(m), _) => m as usize,
        (MarkerAction::Next, Some(w)) => w + 1,
        (MarkerAction::Prev, Some(w)) => w.checked_sub(1)?,
        (MarkerAction::Next, None) => section,
        // Section s starts at marker s - 1; the one before it is s - 2. In
        // the first marker's section Prev goes to its start; before any
        // marker there is none to go back to.
        (MarkerAction::Prev, None) => section.checked_sub(1)?.saturating_sub(1),
        _ => return None,
    };
    (to < n).then_some(to)
}

/// The cue points of a WAV file as markers: the `cue ` chunk's sample
/// offsets, named by the `labl` entries of a `LIST`/`adtl` chunk where the
/// file has them, else "Marker 1", "Marker 2", … in file order. A file
/// without cue points, or one that is no WAV, has none.
pub fn wav_cues(bytes: &[u8]) -> Vec<Marker> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Vec::new();
    }
    let u32_at = |i: usize| bytes.get(i..i + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let mut rate = 0u32;
    let mut cues: Vec<(u32, u32)> = Vec::new(); // (id, sample offset)
    let mut labels: Vec<(u32, String)> = Vec::new();
    let mut i = 12;
    while let (Some(id), Some(size)) = (bytes.get(i..i + 4), u32_at(i + 4)) {
        let (body, size) = (i + 8, size as usize);
        let end = body.saturating_add(size).min(bytes.len());
        if id == b"fmt " {
            rate = u32_at(body + 4).unwrap_or(0);
        } else if id == b"cue " {
            let n = u32_at(body).unwrap_or(0) as usize;
            // Each cue point is 24 bytes: id, position, chunk id, chunk
            // start, block start, sample offset. In a plain PCM file the
            // sample offset is the frame; the position repeats it.
            for k in 0..n.min(4096) {
                let p = body + 4 + k * 24;
                if p + 24 > end {
                    break;
                }
                if let (Some(id), Some(offset)) = (u32_at(p), u32_at(p + 20)) {
                    cues.push((id, offset));
                }
            }
        } else if id == b"LIST" && bytes.get(body..body + 4) == Some(b"adtl") {
            let mut j = body + 4;
            while let (Some(sub), Some(len)) = (bytes.get(j..j + 4), u32_at(j + 4)) {
                let len = len as usize;
                let sub_end = (j + 8).saturating_add(len).min(end);
                if j + 8 > end {
                    break;
                }
                if sub == b"labl" && j + 12 <= sub_end {
                    let text = &bytes[j + 12..sub_end];
                    let text = &text[..text.iter().position(|&b| b == 0).unwrap_or(text.len())];
                    if let Some(id) = u32_at(j + 8) {
                        labels.push((id, String::from_utf8_lossy(text).trim().to_string()));
                    }
                }
                j = j + 8 + len + (len & 1);
            }
        }
        i = body + size + (size & 1);
    }
    if rate == 0 {
        return Vec::new();
    }
    cues.sort_by_key(|c| c.1);
    let mut out: Vec<Marker> = cues
        .iter()
        .enumerate()
        .map(|(k, &(id, offset))| {
            let name = labels.iter().find(|l| l.0 == id && !l.1.is_empty()).map(|l| l.1.clone());
            Marker::new(name.unwrap_or_else(|| format!("Marker {}", k + 1)), offset as f64 / rate as f64)
        })
        .collect();
    normalize(&mut out);
    out
}

/// The waveform overview: the peak of `PEAK_BUCKETS` equal slices of
/// interleaved stereo `samples` (fewer when the file is shorter).
pub fn peaks(samples: &[f32]) -> Vec<f32> {
    let frames = samples.len() / 2;
    let buckets = PEAK_BUCKETS.min(frames);
    (0..buckets)
        .map(|b| {
            let (from, to) = (b * frames / buckets, ((b + 1) * frames / buckets).max(b * frames / buckets + 1));
            samples[from * 2..to.min(frames) * 2].iter().fold(0.0f32, |m, s| m.max(s.abs()))
        })
        .collect()
}

/// A WAV as bytes with cue points and labels, as an editor writes them
/// (tests and `player-test`): 16-bit PCM, the `cue ` and `LIST` chunks after
/// the data, a label per cue unless its name is empty.
pub fn wav_with_cues(rate: u32, channels: u16, pcm: &[i16], cues: &[(u32, &str)]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend(b"RIFF\0\0\0\0WAVEfmt ");
    b.extend(16u32.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(channels.to_le_bytes());
    b.extend(rate.to_le_bytes());
    b.extend((rate * channels as u32 * 2).to_le_bytes());
    b.extend((channels * 2).to_le_bytes());
    b.extend(16u16.to_le_bytes());
    b.extend(b"data");
    b.extend(((pcm.len() * 2) as u32).to_le_bytes());
    for s in pcm {
        b.extend(s.to_le_bytes());
    }
    b.extend(b"cue ");
    b.extend(((4 + cues.len() * 24) as u32).to_le_bytes());
    b.extend((cues.len() as u32).to_le_bytes());
    for (k, (frame, _)) in cues.iter().enumerate() {
        b.extend((k as u32 + 1).to_le_bytes());
        b.extend(frame.to_le_bytes());
        b.extend(b"data");
        b.extend(0u32.to_le_bytes());
        b.extend(0u32.to_le_bytes());
        b.extend(frame.to_le_bytes());
    }
    let mut adtl: Vec<u8> = b"adtl".to_vec();
    for (k, (_, name)) in cues.iter().enumerate() {
        if name.is_empty() {
            continue;
        }
        let text_len = name.len() + 1;
        adtl.extend(b"labl");
        adtl.extend(((4 + text_len) as u32).to_le_bytes());
        adtl.extend((k as u32 + 1).to_le_bytes());
        adtl.extend(name.as_bytes());
        adtl.push(0);
        if text_len % 2 == 1 {
            adtl.push(0);
        }
    }
    b.extend(b"LIST");
    b.extend((adtl.len() as u32).to_le_bytes());
    b.extend(adtl);
    let riff = (b.len() - 8) as u32;
    b[4..8].copy_from_slice(&riff.to_le_bytes());
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marks(at: &[f64]) -> Vec<Marker> {
        at.iter().enumerate().map(|(i, &t)| Marker::new(format!("M{i}"), t)).collect()
    }

    #[test]
    fn a_wavs_cue_points_become_named_markers_in_file_order() {
        let pcm = vec![0i16; 44_100 * 2 * 3];
        // Written out of order, one without a label, as editors do.
        let bytes = wav_with_cues(44_100, 2, &pcm, &[(88_200, "Chorus"), (0, "Intro"), (44_100, "")]);
        let got = wav_cues(&bytes);
        assert_eq!(got.iter().map(|m| (m.name.as_str(), m.at)).collect::<Vec<_>>(), [("Intro", 0.0), ("Marker 2", 1.0), ("Chorus", 2.0)]);
        assert!(got.iter().all(|m| m.part.is_none()), "a file's cue selects no part");
        // The player reads the same bytes as audio: the chunks after `data` are skipped.
        let path = std::env::temp_dir().join(format!("ksalat-cues-{}.wav", std::process::id()));
        std::fs::write(&path, &bytes).unwrap();
        let data = crate::player::load_file(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!((data.frames(), data.sample_rate), (44_100 * 3, 44_100));
    }

    #[test]
    fn a_file_without_cues_a_truncated_one_and_no_wav_have_no_markers() {
        let plain = wav_with_cues(48_000, 1, &[0i16; 480], &[]);
        assert!(wav_cues(&plain).is_empty());
        let cued = wav_with_cues(48_000, 1, &[0i16; 480], &[(100, "A"), (200, "B")]);
        for cut in [0, 11, 12, 20, 60, cued.len() - 30, cued.len() - 3] {
            let _ = wav_cues(&cued[..cut]); // never panics, whatever it finds
        }
        assert!(wav_cues(b"just some text, not audio").is_empty());
        // A chunk size that runs past the file ends the scan.
        let mut bad = cued.clone();
        let at = bad.windows(4).position(|w| w == b"cue ").unwrap();
        bad[at + 4..at + 8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(wav_cues(&bad).len(), 2, "the cue points inside the file are still read");
    }

    #[test]
    fn markers_are_kept_in_order_apart_and_within_the_engines_table() {
        let mut m = marks(&[4.0, f64::NAN, 1.0, -2.0, 1.0004, 8.0]);
        normalize(&mut m);
        assert_eq!(m.iter().map(|m| m.at).collect::<Vec<_>>(), [0.0, 1.0, 4.0, 8.0]);
        let mut many = marks(&(0..50).map(|i| i as f64).collect::<Vec<_>>());
        normalize(&mut many);
        assert_eq!(many.len(), MAX_MARKERS);
        let t = MarkerTable::new(&marks(&[4.0, 1.0]));
        assert_eq!((t.len(), t.at(0), t.at(1), t.at(2)), (2, Some(1.0), Some(4.0), None));
    }

    #[test]
    fn a_section_runs_from_its_marker_to_the_next_or_the_files_end() {
        let mut m = marks(&[2.0, 4.0]);
        m[1].part = Some(3);
        let t = MarkerTable::new(&m);
        assert_eq!([0.0, 1.99, 2.0, 3.0, 4.0, 9.0].map(|x| t.section_of(x)), [0, 0, 1, 1, 2, 2]);
        assert_eq!(t.bounds(0, 6.0), (0.0, 2.0));
        assert_eq!(t.bounds(1, 6.0), (2.0, 4.0));
        assert_eq!(t.bounds(2, 6.0), (4.0, 6.0));
        assert_eq!(t.bounds(2, 3.0), (3.0, 3.0), "a marker past the end bounds nothing");
        assert_eq!((t.part(0), t.part(1), t.part(2)), (None, Some(3), None));
        assert_eq!(MarkerTable::EMPTY.bounds(0, 6.0), (0.0, 6.0), "no markers: the file is one section");
    }

    #[test]
    fn next_and_prev_go_from_the_section_playing_or_from_a_waiting_jump() {
        let t = MarkerTable::new(&marks(&[0.0, 2.0, 4.0]));
        let go = |section, waiting, a| target(&t, section, waiting, a);
        assert_eq!(go(1, None, MarkerAction::Next), Some(1), "from the first section to the second marker");
        assert_eq!(go(3, None, MarkerAction::Next), None, "the last section has no next");
        assert_eq!(go(3, None, MarkerAction::Prev), Some(1));
        assert_eq!(go(1, None, MarkerAction::Prev), Some(0), "in the first section: back to its start");
        assert_eq!(go(0, None, MarkerAction::Prev), None, "before the first marker: none to go back to");
        assert_eq!(go(1, Some(1), MarkerAction::Next), Some(2), "a second press goes one further");
        assert_eq!(go(1, Some(2), MarkerAction::Next), None);
        assert_eq!(go(3, Some(1), MarkerAction::Prev), Some(0));
        assert_eq!(go(3, Some(0), MarkerAction::Prev), None);
        assert_eq!(go(0, None, MarkerAction::Jump(2)), Some(2));
        assert_eq!(go(0, None, MarkerAction::Jump(3)), None, "no such marker");
        assert_eq!(go(1, None, MarkerAction::Loop(None)), None);
    }

    #[test]
    fn loop_and_stop_are_one_choice_and_each_toggles() {
        let c = AtEnd::Continue;
        assert_eq!(c.after(MarkerAction::Loop(None)), AtEnd::Loop);
        assert_eq!(AtEnd::Loop.after(MarkerAction::Loop(None)), AtEnd::Continue);
        assert_eq!(AtEnd::Loop.after(MarkerAction::StopAtEnd(None)), AtEnd::Stop, "stop takes over from loop");
        assert_eq!(AtEnd::Stop.after(MarkerAction::Loop(Some(false))), AtEnd::Stop, "switching loop off leaves stop alone");
        assert_eq!(AtEnd::Stop.after(MarkerAction::StopAtEnd(Some(false))), AtEnd::Continue);
        assert_eq!(AtEnd::Stop.after(MarkerAction::StopAtEnd(Some(true))), AtEnd::Stop);
        assert_eq!(AtEnd::Loop.after(MarkerAction::Next), AtEnd::Loop, "a jump keeps the loop");
    }

    #[test]
    fn the_overview_holds_each_slices_peak() {
        let mut s = vec![0.0f32; 4096 * 2];
        s[2000 * 2 + 1] = -0.75; // right channel, negative
        let p = peaks(&s);
        assert_eq!(p.len(), PEAK_BUCKETS);
        let b = 2000 * PEAK_BUCKETS / 4096;
        assert_eq!(p[b], 0.75);
        assert_eq!(p.iter().filter(|&&v| v > 0.0).count(), 1);
        assert_eq!(peaks(&[0.5, 0.5, -1.0, 0.0]).len(), 2, "a file shorter than the overview");
        assert!(peaks(&[]).is_empty());
    }
}
