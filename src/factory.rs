//! Factory sounds (#221): a piano, an electric piano and a drum kit that play
//! with no plugin installed.
//!
//! A small built-in sample player. The samples are reduced copies of three
//! CC0 FreePats banks (`assets/factory/*/LICENSE`, rebuilt by
//! `scripts/factory-sounds.sh`), embedded in the binary as FLAC. A factory
//! instrument is an ordinary rig instrument: a `NodeKind::Clap` whose plugin
//! id is one of [`PIANO_ID`], [`EPIANO_ID`], [`DRUMS_ID`], played by a
//! `ClapVoice` whose backend is a [`Player`] instead of a plugin. Everything
//! that follows plugins (voice binding, rigs, matrix, sessions, mixer) works
//! unchanged; `ClapVoice::new_plugin` answers these ids.
//!
//! Threads: the samples are decoded once per process on their own thread
//! (rule 46); until a bank is ready its voice stays silent. The audio thread
//! only reads the decoded bank (`OnceLock::get`), mixes into fixed voices and
//! never allocates.

use crate::host::VoiceCmd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub const PIANO_ID: &str = "kabelsalat.factory.piano";
pub const EPIANO_ID: &str = "kabelsalat.factory.epiano";
pub const DRUMS_ID: &str = "kabelsalat.factory.drums";

/// The three factory instruments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Piano,
    Epiano,
    Drums,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Piano, Kind::Epiano, Kind::Drums];

    pub fn id(self) -> &'static str {
        match self {
            Kind::Piano => PIANO_ID,
            Kind::Epiano => EPIANO_ID,
            Kind::Drums => DRUMS_ID,
        }
    }

    /// The unit's display name.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Piano => "Factory Piano",
            Kind::Epiano => "Factory Electric Piano",
            Kind::Drums => "Factory Drum Kit",
        }
    }

    /// The one sound of the instrument, as `list_sounds` shows it.
    pub fn sound(self) -> (&'static str, &'static str) {
        match self {
            Kind::Piano => ("Piano", "Upright Piano"),
            Kind::Epiano => ("Electric Piano", "FM Electric Piano"),
            Kind::Drums => ("Drums", "Synth Drum Kit"),
        }
    }

    /// What the keyboard dock and the Plugins browser show it as (rule 192).
    pub fn keyboard_type(self) -> crate::keyboard::KeyboardType {
        use crate::keyboard::KeyboardType;
        match self {
            Kind::Piano => KeyboardType::Piano,
            Kind::Epiano => KeyboardType::ElectricPiano,
            Kind::Drums => KeyboardType::Drums,
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// The factory instrument a plugin id names.
pub fn kind_of(plugin_id: &str) -> Option<Kind> {
    Kind::ALL.into_iter().find(|k| k.id() == plugin_id)
}

// --- the samples -------------------------------------------------------

/// One embedded sample: file, the MIDI note it was played at.
struct Src {
    flac: &'static [u8],
    root: u8,
}

macro_rules! src {
    ($dir:literal, $file:literal, $root:expr) => {
        Src { flac: include_bytes!(concat!("../assets/factory/", $dir, "/", $file)), root: $root }
    };
}

const PIANO: &[Src] = &[
    src!("piano", "a0.flac", 21), src!("piano", "ds1.flac", 27), src!("piano", "a1.flac", 33),
    src!("piano", "ds2.flac", 39), src!("piano", "a2.flac", 45), src!("piano", "ds3.flac", 51),
    src!("piano", "a3.flac", 57), src!("piano", "ds4.flac", 63), src!("piano", "a4.flac", 69),
    src!("piano", "ds5.flac", 75), src!("piano", "a5.flac", 81), src!("piano", "ds6.flac", 87),
    src!("piano", "a6.flac", 93), src!("piano", "ds7.flac", 99), src!("piano", "a7.flac", 105),
];

const EPIANO: &[Src] = &[
    src!("epiano", "fs1.flac", 30), src!("epiano", "fs2.flac", 42), src!("epiano", "c3.flac", 48),
    src!("epiano", "fs3.flac", 54), src!("epiano", "c4.flac", 60), src!("epiano", "fs4.flac", 66),
    src!("epiano", "c5.flac", 72), src!("epiano", "fs5.flac", 78), src!("epiano", "fs6.flac", 90),
];

/// The kit: each file with the General MIDI notes that play it (the dock's
/// drum pads, `keyboard::DRUM_PADS`, and the rest of the GM drum range).
const DRUMS: &[(Src, &[u8])] = &[
    (src!("drums", "kick.flac", 0), &[35, 36]),
    (src!("drums", "kick2.flac", 0), &[]),
    (src!("drums", "claves.flac", 0), &[37, 56, 75]),
    (src!("drums", "snare.flac", 0), &[38]),
    (src!("drums", "clap.flac", 0), &[39]),
    (src!("drums", "snare2.flac", 0), &[40]),
    (src!("drums", "tom_low.flac", 0), &[41, 43, 45]),
    (src!("drums", "hat_closed.flac", 0), &[42]),
    (src!("drums", "hat_pedal.flac", 0), &[44]),
    (src!("drums", "hat_open.flac", 0), &[46]),
    (src!("drums", "tom_mid.flac", 0), &[47, 48]),
    (src!("drums", "crash.flac", 0), &[49, 52, 55, 57]),
    (src!("drums", "tom_high.flac", 0), &[50]),
    (src!("drums", "ride.flac", 0), &[51, 53, 59]),
    (src!("drums", "shaker.flac", 0), &[54, 69, 70]),
];

/// One decoded sample and the notes it plays.
struct Region {
    lo: u8,
    hi: u8,
    root: u8,
    rate: f32,
    data: Vec<f32>,
}

/// A decoded instrument.
pub struct Bank {
    regions: Vec<Region>,
    /// Drum kits ring out whole: a key release does not stop a hit.
    one_shot: bool,
}

fn decode(flac: &[u8]) -> Option<(u32, Vec<f32>)> {
    let mut r = claxon::FlacReader::new(std::io::Cursor::new(flac)).ok()?;
    let info = r.streaminfo();
    let scale = 1.0 / (1u64 << (info.bits_per_sample - 1)) as f32;
    let ch = info.channels as usize;
    let mut out = Vec::new();
    let mut frame = 0usize;
    let mut acc = 0.0f32;
    for s in r.samples() {
        acc += s.ok()? as f32 * scale;
        frame += 1;
        if frame == ch {
            out.push(acc / ch as f32);
            frame = 0;
            acc = 0.0;
        }
    }
    Some((info.sample_rate, out))
}

fn build(kind: Kind) -> Bank {
    let mut regions = Vec::new();
    match kind {
        Kind::Piano | Kind::Epiano => {
            let table = if kind == Kind::Piano { PIANO } else { EPIANO };
            let (floor, ceil) = if kind == Kind::Piano { (0u8, 127u8) } else { (0, 127) };
            for (i, s) in table.iter().enumerate() {
                let Some((rate, data)) = decode(s.flac) else { continue };
                // Each sample plays up to halfway to its neighbours.
                let lo = if i == 0 { floor } else { (table[i - 1].root + s.root) / 2 + 1 };
                let hi = table.get(i + 1).map_or(ceil, |n| (s.root + n.root) / 2);
                regions.push(Region { lo, hi, root: s.root, rate: rate as f32, data });
            }
        }
        Kind::Drums => {
            for (s, notes) in DRUMS {
                let Some((rate, data)) = decode(s.flac) else { continue };
                for &n in *notes {
                    regions.push(Region { lo: n, hi: n, root: n, rate: rate as f32, data: data.clone() });
                }
            }
        }
    }
    Bank { regions, one_shot: kind == Kind::Drums }
}

static BANKS: [OnceLock<Bank>; 3] = [OnceLock::new(), OnceLock::new(), OnceLock::new()];
static STARTED: [AtomicBool; 3] = [AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false)];

/// Start decoding `kind` on its own thread (once per process).
pub fn load(kind: Kind) {
    if STARTED[kind.index()].swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new().name("kabelsalat-factory-decode".into()).spawn(move || {
        let _ = BANKS[kind.index()].set(build(kind));
    });
    if spawned.is_err() {
        STARTED[kind.index()].store(false, Ordering::Release);
    }
}

/// Whether `kind` has been decoded.
pub fn ready(kind: Kind) -> bool {
    BANKS[kind.index()].get().is_some()
}

/// Wait (self-tests only) until `kind` is decoded.
pub fn wait_ready(kind: Kind, timeout: Duration) -> bool {
    load(kind);
    let t0 = Instant::now();
    while !ready(kind) {
        if t0.elapsed() > timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    true
}

// --- the player --------------------------------------------------------

const VOICES: usize = 32;
/// Attack ramp, against the click of a sample that starts mid-wave.
const ATTACK_S: f32 = 0.002;
/// Release of a let-go piano key.
const RELEASE_S: f32 = 0.25;
/// A hard stop (all sound off, a stolen voice).
const KILL_S: f32 = 0.01;

#[derive(Clone, Copy)]
struct Voice {
    active: bool,
    note: u8,
    region: u16,
    pos: f64,
    step: f64,
    gain: f32,
    env: f32,
    attack: f32,
    released: bool,
    held_by_pedal: bool,
    rel: f32,
    age: u64,
}

const IDLE: Voice = Voice {
    active: false, note: 0, region: 0, pos: 0.0, step: 0.0, gain: 0.0, env: 0.0, attack: 0.0,
    released: false, held_by_pedal: false, rel: 0.0, age: 0,
};

/// The audio half of a factory voice. `render` allocates nothing.
pub struct Player {
    kind: Kind,
    sample_rate: f64,
    voices: [Voice; VOICES],
    sustain: bool,
    clock: u64,
    attack_step: f32,
    release_coef: f32,
    kill_coef: f32,
}

impl Player {
    pub fn new(kind: Kind, sample_rate: f64) -> Self {
        load(kind);
        let coef = |s: f32| (-6.9 / (s * sample_rate as f32)).exp();
        Self {
            kind,
            sample_rate,
            voices: [IDLE; VOICES],
            sustain: false,
            clock: 0,
            attack_step: 1.0 / (ATTACK_S * sample_rate as f32),
            release_coef: coef(RELEASE_S),
            kill_coef: coef(KILL_S),
        }
    }

    fn bank(&self) -> Option<&'static Bank> {
        BANKS[self.kind.index()].get()
    }

    /// Whether any voice still sounds.
    pub fn sounding(&self) -> bool {
        self.voices.iter().any(|v| v.active)
    }

    fn note_on(&mut self, note: u8, velocity: f64) {
        let Some(bank) = self.bank() else { return };
        let Some(ri) = bank.regions.iter().position(|r| r.lo <= note && note <= r.hi) else { return };
        let region = &bank.regions[ri];
        // The same key struck again lets the old note go first.
        for v in self.voices.iter_mut().filter(|v| v.active && v.note == note && !v.released) {
            v.released = true;
            v.rel = self.release_coef;
        }
        let slot = self.voices.iter().position(|v| !v.active).unwrap_or_else(|| {
            // Steal the quietest of the oldest.
            let mut best = 0;
            for (i, v) in self.voices.iter().enumerate() {
                let b = &self.voices[best];
                if (v.released, std::cmp::Reverse(v.age)) > (b.released, std::cmp::Reverse(b.age)) {
                    best = i;
                }
            }
            best
        });
        let vel = velocity.clamp(0.0, 1.0) as f32;
        let semis = if bank.one_shot { 0.0 } else { (note as f64 - region.root as f64) / 12.0 };
        self.clock += 1;
        self.voices[slot] = Voice {
            active: true,
            note,
            region: ri as u16,
            pos: 0.0,
            step: region.rate as f64 / self.sample_rate * 2f64.powf(semis),
            gain: (0.2 + 0.8 * vel * vel) * 0.9,
            env: 0.0,
            attack: 0.0,
            released: false,
            held_by_pedal: false,
            rel: self.release_coef,
            age: self.clock,
        };
    }

    fn note_off(&mut self, note: u8) {
        let Some(bank) = self.bank() else { return };
        if bank.one_shot {
            return;
        }
        let sustain = self.sustain;
        for v in self.voices.iter_mut().filter(|v| v.active && v.note == note && !v.released) {
            if sustain {
                v.held_by_pedal = true;
            } else {
                v.released = true;
            }
        }
    }

    fn all_off(&mut self, kill: bool) {
        for v in self.voices.iter_mut().filter(|v| v.active) {
            v.released = true;
            if kill {
                v.rel = self.kill_coef;
            }
        }
    }

    fn midi(&mut self, m: [u8; 3]) {
        match m[0] & 0xF0 {
            0x90 if m[2] > 0 => self.note_on(m[1], m[2] as f64 / 127.0),
            0x90 | 0x80 => self.note_off(m[1]),
            0xB0 => match m[1] {
                64 => {
                    let down = m[2] >= 64;
                    if self.sustain && !down {
                        for v in self.voices.iter_mut().filter(|v| v.held_by_pedal) {
                            v.held_by_pedal = false;
                            v.released = true;
                        }
                    }
                    self.sustain = down;
                }
                120 => self.all_off(true),
                123 => self.all_off(false),
                _ => {}
            },
            _ => {}
        }
    }

    /// Apply this block's commands and mix `out.len() / 2` stereo frames.
    pub fn render_block(&mut self, out: &mut [f32], cmds: &[VoiceCmd]) {
        for &c in cmds {
            match c {
                VoiceCmd::NoteOn(n, v) if v > 0.0 => self.note_on(n.min(127) as u8, v),
                VoiceCmd::NoteOn(n, _) | VoiceCmd::NoteOff(n) => self.note_off(n.min(127) as u8),
                VoiceCmd::Midi(m) => self.midi(m),
                VoiceCmd::Param(..) => {}
            }
        }
        out.fill(0.0);
        let Some(bank) = self.bank() else { return };
        let frames = out.len() / 2;
        for v in self.voices.iter_mut().filter(|v| v.active) {
            let data = &bank.regions[v.region as usize].data;
            for f in 0..frames {
                let i = v.pos as usize;
                if i + 1 >= data.len() {
                    v.active = false;
                    break;
                }
                let frac = (v.pos - i as f64) as f32;
                let s = data[i] + (data[i + 1] - data[i]) * frac;
                if v.attack < 1.0 {
                    v.attack = (v.attack + self.attack_step).min(1.0);
                }
                if v.released {
                    v.env *= v.rel;
                    if v.env < 1e-4 {
                        v.active = false;
                        break;
                    }
                } else {
                    v.env = v.attack;
                }
                let s = s * v.gain * v.env * v.attack.min(1.0);
                out[f * 2] += s;
                out[f * 2 + 1] += s;
                v.pos += v.step;
            }
        }
    }
}

// --- fallback for starter setups ----------------------------------------

static FALLBACK: AtomicBool = AtomicBool::new(false);

/// Starter setups use the factory sounds instead of Surge XT from now on
/// (the app sets this when Surge XT is not installed).
pub fn set_fallback(on: bool) {
    FALLBACK.store(on, Ordering::Release);
}

pub fn fallback_active() -> bool {
    FALLBACK.load(Ordering::Acquire)
}

/// The factory instrument that stands in for the `n`th Surge XT of a
/// starter setup: a piano, then an electric piano.
pub fn stand_in(n: usize) -> (&'static str, &'static str) {
    if n % 2 == 0 { (PIANO_ID, "PIANO") } else { (EPIANO_ID, "E-PIANO") }
}

/// The synth a starter setup puts in its `n`th instrument slot: Surge XT, or
/// the factory instrument that stands in for it while Surge is missing.
pub fn starter_synth(n: usize) -> (&'static str, &'static str) {
    if fallback_active() { stand_in(n) } else { (crate::control::SURGE_PLUGIN_ID, "SURGE") }
}

// --- the self-test --------------------------------------------------------

/// `kabelsalat factory-test`: with no plugin involved, every factory
/// instrument starts in a real engine and its first note peaks audibly.
pub fn run_test() -> anyhow::Result<()> {
    use crate::control::{command_bus, plugin_rig_graph, Command};
    use crate::engine::{GraphEngine, GraphPub, VoiceBack, VoiceMsg, VoiceSlot};
    use crate::matrix::SourceId;
    use std::sync::{Arc, Mutex};
    const SR: f64 = 48000.0;
    println!("=== factory-test (no plugin is loaded) ===");
    println!("  Surge XT installed here: {}", crate::host::surge_available());
    for kind in Kind::ALL {
        let t0 = Instant::now();
        anyhow::ensure!(wait_ready(kind, Duration::from_secs(20)), "{} was not decoded in 20 s", kind.name());
        let decode_ms = t0.elapsed().as_millis();
        let g = plugin_rig_graph(kind.id(), "FACTORY");
        let (mut bus, drain) = command_bus();
        let mut engine = GraphEngine::new(g.clone(), drain, SR, vec![]);
        let gp = Arc::new(Mutex::new(GraphPub { graph: g, ..Default::default() }));
        engine.set_graph_pub(gp);
        let (mut tx, rx) = rtrb::RingBuffer::<VoiceMsg>::new(16);
        let (back_tx, _back) = rtrb::RingBuffer::<VoiceBack>::new(16);
        engine.set_voice_channels(rx, back_tx);
        let (voice, _main) = crate::host::ClapVoice::new_plugin(SR, kind.id())?;
        tx.push(VoiceMsg::Add(VoiceSlot::new(voice))).map_err(|_| anyhow::anyhow!("voice ring full"))?;
        let mut buf = vec![0.0f32; 256 * 2];
        engine.render(&mut buf, 2);
        let note = if kind == Kind::Drums { 36 } else { 60 };
        bus.send(Command::NoteOn { note, velocity: 0.8, source: SourceId::Keys, channel: 0 });
        let mut peak = 0.0f32;
        let mut first_block = None;
        for b in 0..200 {
            engine.render(&mut buf, 2);
            let p = buf.iter().fold(0.0f32, |m, x| m.max(x.abs()));
            if p > 0.01 && first_block.is_none() {
                first_block = Some(b);
            }
            peak = peak.max(p);
        }
        println!("  {:<24} decoded in {decode_ms:>4} ms, note {note}: peak {peak:.3}, first sound in block {first_block:?}", kind.name());
        anyhow::ensure!(peak > 0.05, "{}: the first note peaks at {peak}, not audibly", kind.name());
        anyhow::ensure!(first_block == Some(0), "{}: the note starts late ({first_block:?})", kind.name());
        anyhow::ensure!(peak <= 1.0, "{}: clips ({peak})", kind.name());
    }
    println!("factory-test: ok");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_note(kind: Kind, note: u8, blocks: usize) -> Vec<f32> {
        assert!(wait_ready(kind, Duration::from_secs(60)));
        let mut p = Player::new(kind, 48000.0);
        let mut all = Vec::new();
        let mut buf = vec![0.0f32; 512];
        p.render_block(&mut buf, &[VoiceCmd::NoteOn(note as u16, 0.8)]);
        all.extend_from_slice(&buf);
        for _ in 1..blocks {
            p.render_block(&mut buf, &[]);
            all.extend_from_slice(&buf);
        }
        all
    }

    fn peak(v: &[f32]) -> f32 {
        v.iter().fold(0.0, |m, x| m.max(x.abs()))
    }

    #[test]
    fn ids_name_the_three_instruments() {
        for k in Kind::ALL {
            assert_eq!(kind_of(k.id()), Some(k));
        }
        assert_eq!(kind_of("org.surge-synth-team.surge-xt"), None);
    }

    #[test]
    fn middle_c_sounds_on_every_pitched_instrument_and_in_tune() {
        for kind in [Kind::Piano, Kind::Epiano] {
            let out = render_note(kind, 60, 60);
            assert!(peak(&out) > 0.05 && peak(&out) <= 1.0, "{kind:?}: {}", peak(&out));
        }
        // A one-octave shift doubles the playback speed: the same sample,
        // played at 72, is over in about half the time.
        let len = |note| render_note(Kind::Epiano, note, 1000).chunks(2).rposition(|f| f[0].abs() > 1e-3).unwrap();
        assert!(len(72) < len(60), "{} vs {}", len(72), len(60));
    }

    #[test]
    fn the_kit_plays_the_dock_pads() {
        for pad in crate::keyboard::DRUM_PADS {
            let out = render_note(Kind::Drums, pad.note, 20);
            assert!(peak(&out) > 0.02, "{} (note {}) is silent", pad.label, pad.note);
        }
    }

    #[test]
    fn a_released_key_fades_out_and_the_pedal_holds_it() {
        assert!(wait_ready(Kind::Piano, Duration::from_secs(60)));
        let mut p = Player::new(Kind::Piano, 48000.0);
        let mut buf = vec![0.0f32; 512];
        p.render_block(&mut buf, &[VoiceCmd::Midi([0xB0, 64, 127]), VoiceCmd::NoteOn(60, 0.8)]);
        p.render_block(&mut buf, &[VoiceCmd::NoteOff(60)]);
        for _ in 0..40 { p.render_block(&mut buf, &[]); }
        assert!(p.sounding(), "the pedal holds the note");
        p.render_block(&mut buf, &[VoiceCmd::Midi([0xB0, 64, 0])]);
        for _ in 0..200 { p.render_block(&mut buf, &[]); }
        assert!(!p.sounding(), "the note ends after the pedal is up");
    }

    #[test]
    fn many_notes_never_exceed_the_voice_table() {
        assert!(wait_ready(Kind::Piano, Duration::from_secs(60)));
        let mut p = Player::new(Kind::Piano, 48000.0);
        let mut buf = vec![0.0f32; 512];
        let cmds: Vec<VoiceCmd> = (30..100).map(|n| VoiceCmd::NoteOn(n, 0.9)).collect();
        p.render_block(&mut buf, &cmds);
        assert!(p.voices.iter().filter(|v| v.active).count() <= VOICES);
        assert!(peak(&buf).is_finite());
    }

    #[test]
    fn the_sounds_are_a_source_and_the_keyboard_knows_the_type() {
        use crate::control::{ClapId, NodeKind};
        use crate::keyboard::{catalog_type, KeyboardType, PluginRef};
        let want = [(Kind::Piano, KeyboardType::Piano), (Kind::Epiano, KeyboardType::ElectricPiano), (Kind::Drums, KeyboardType::Drums)];
        for (kind, ty) in want {
            let node = NodeKind::Clap { plugin_id: ClapId::new(kind.id()) };
            let inst = crate::sounds::Instrument::of(&node).expect("a factory unit is an instrument");
            assert_eq!(inst.source(), crate::sounds::Source::FactorySounds);
            let lib = crate::sounds::scan(&inst);
            assert_eq!(lib.sounds.len(), 1);
            assert_eq!(crate::sounds::find(&lib.sounds, "piano drum kit electric").map(|s| s.label()), Some(lib.sounds[0].label()));
            assert_eq!(catalog_type(PluginRef::Clap(kind.id())), Some(ty));
            assert_eq!(crate::keyboard::classify_instrument(&node, None, None, &[]), ty);
        }
    }

    #[test]
    fn stand_ins_alternate_piano_and_electric_piano() {
        assert_eq!([stand_in(0).0, stand_in(1).0, stand_in(2).0], [PIANO_ID, EPIANO_ID, PIANO_ID]);
    }
}
