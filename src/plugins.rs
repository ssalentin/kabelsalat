//! Unified plugin taxonomy and search across built-ins, CLAP, LV2 and the
//! install catalog.
//!
//! Pure data + pure functions (zero I/O, zero lilv/clack calls) so the Plugins
//! stage, the Mixer insert picker, the in-app assistant and MCP all classify,
//! filter and search plugins identically.

use crate::control::DspKind;
use crate::host::ClapPluginInfo;
use crate::lv2::Lv2PluginInfo;

/// Top-level musical role of a plugin.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum PluginType {
    Instrument,
    Effect,
    MidiFx,
    Utility,
}

impl PluginType {
    pub const ALL: [PluginType; 4] = [
        PluginType::Instrument,
        PluginType::Effect,
        PluginType::MidiFx,
        PluginType::Utility,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            PluginType::Instrument => "Instruments",
            PluginType::Effect => "Effects",
            PluginType::MidiFx => "MIDI FX",
            PluginType::Utility => "Utilities",
        }
    }

    pub const fn singular(self) -> &'static str {
        match self {
            PluginType::Instrument => "Instrument",
            PluginType::Effect => "Effect",
            PluginType::MidiFx => "MIDI FX",
            PluginType::Utility => "Utility",
        }
    }

    pub const fn slug(self) -> &'static str {
        match self {
            PluginType::Instrument => "instrument",
            PluginType::Effect => "effect",
            PluginType::MidiFx => "midi_fx",
            PluginType::Utility => "utility",
        }
    }

    /// Parse a user/agent type filter (singular, plural, or common alias).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "instrument" | "instruments" | "inst" | "synth" | "synths" => Some(PluginType::Instrument),
            "effect" | "effects" | "fx" | "audio_effect" | "audio-effect" | "audio_effects" => Some(PluginType::Effect),
            "midi_fx" | "midi-fx" | "midifx" | "midi" | "note_effect" | "note-effect" | "note_effects" => {
                Some(PluginType::MidiFx)
            }
            "utility" | "utilities" | "util" | "utils" | "analyzer" | "analyzers" | "analyser" | "analysers" => {
                Some(PluginType::Utility)
            }
            _ => None,
        }
    }
}

/// Second-level musical subcategory inside a [`PluginType`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum PluginSubtype {
    // Instruments
    Synth,
    Organ,
    Sampler,
    Drum,
    InstrumentOther,

    // Audio effects (ordered by studio workflow)
    DelayReverb,
    EqFilter,
    Dynamics,
    DriveAmp,
    Modulation,
    PitchSpatial,
    MultiFx,
    EffectOther,

    // MIDI FX
    Arpeggiator,
    MidiProcessor,

    // Utilities
    Metronome,
    Analyzer,
    MixerRouting,
    Generator,
    UtilityOther,
}

impl PluginSubtype {
    pub const INSTRUMENT_ALL: &'static [PluginSubtype] = &[
        PluginSubtype::Synth,
        PluginSubtype::Organ,
        PluginSubtype::Sampler,
        PluginSubtype::Drum,
        PluginSubtype::InstrumentOther,
    ];

    pub const EFFECT_ALL: &'static [PluginSubtype] = &[
        PluginSubtype::DelayReverb,
        PluginSubtype::EqFilter,
        PluginSubtype::Dynamics,
        PluginSubtype::DriveAmp,
        PluginSubtype::Modulation,
        PluginSubtype::PitchSpatial,
        PluginSubtype::MultiFx,
        PluginSubtype::EffectOther,
    ];

    pub const MIDIFX_ALL: &'static [PluginSubtype] = &[
        PluginSubtype::Arpeggiator,
        PluginSubtype::MidiProcessor,
    ];

    pub const UTILITY_ALL: &'static [PluginSubtype] = &[
        PluginSubtype::Metronome,
        PluginSubtype::Analyzer,
        PluginSubtype::MixerRouting,
        PluginSubtype::Generator,
        PluginSubtype::UtilityOther,
    ];

    pub const fn subtypes_for(ty: PluginType) -> &'static [PluginSubtype] {
        match ty {
            PluginType::Instrument => Self::INSTRUMENT_ALL,
            PluginType::Effect => Self::EFFECT_ALL,
            PluginType::MidiFx => Self::MIDIFX_ALL,
            PluginType::Utility => Self::UTILITY_ALL,
        }
    }

    pub const fn plugin_type(self) -> PluginType {
        match self {
            Self::Synth | Self::Organ | Self::Sampler | Self::Drum | Self::InstrumentOther => {
                PluginType::Instrument
            }
            Self::DelayReverb
            | Self::EqFilter
            | Self::Dynamics
            | Self::DriveAmp
            | Self::Modulation
            | Self::PitchSpatial
            | Self::MultiFx
            | Self::EffectOther => PluginType::Effect,
            Self::Arpeggiator | Self::MidiProcessor => PluginType::MidiFx,
            Self::Metronome | Self::Analyzer | Self::MixerRouting | Self::Generator | Self::UtilityOther => {
                PluginType::Utility
            }
        }
    }

    /// Section / chip label.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Synth => "Synths",
            Self::Organ => "Organs",
            Self::Sampler => "Samplers",
            Self::Drum => "Drums",
            Self::InstrumentOther => "Other instruments",
            Self::DelayReverb => "Delay & Reverb",
            Self::EqFilter => "EQ & Filter",
            Self::Dynamics => "Dynamics",
            Self::DriveAmp => "Drive & Amp",
            Self::Modulation => "Modulation",
            Self::PitchSpatial => "Pitch & Spatial",
            Self::MultiFx => "Multi-FX",
            Self::EffectOther => "Other FX",
            Self::Arpeggiator => "Arp & Chord",
            Self::MidiProcessor => "MIDI FX",
            Self::Metronome => "Rhythm",
            Self::Analyzer => "Analyzers",
            Self::MixerRouting => "Mix & Gain",
            Self::Generator => "Generators",
            Self::UtilityOther => "Utilities",
        }
    }

    /// Compact uppercase badge for cards.
    pub const fn badge(self) -> &'static str {
        match self {
            Self::Synth => "SYNTH",
            Self::Organ => "ORGAN",
            Self::Sampler => "SAMPLER",
            Self::Drum => "DRUM",
            Self::InstrumentOther => "INST",
            Self::DelayReverb => "DELAY / VERB",
            Self::EqFilter => "EQ / FILTER",
            Self::Dynamics => "DYNAMICS",
            Self::DriveAmp => "DRIVE / AMP",
            Self::Modulation => "MOD",
            Self::PitchSpatial => "PITCH / SPACE",
            Self::MultiFx => "MULTI-FX",
            Self::EffectOther => "FX",
            Self::Arpeggiator => "ARP",
            Self::MidiProcessor => "MIDI FX",
            Self::Metronome => "RHYTHM",
            Self::Analyzer => "ANALYZER",
            Self::MixerRouting => "MIX / GAIN",
            Self::Generator => "GEN",
            Self::UtilityOther => "UTILITY",
        }
    }

    pub const fn slug(self) -> &'static str {
        match self {
            Self::Synth => "synth",
            Self::Organ => "organ",
            Self::Sampler => "sampler",
            Self::Drum => "drum",
            Self::InstrumentOther => "instrument",
            Self::DelayReverb => "delay_reverb",
            Self::EqFilter => "eq_filter",
            Self::Dynamics => "dynamics",
            Self::DriveAmp => "drive_amp",
            Self::Modulation => "modulation",
            Self::PitchSpatial => "pitch_spatial",
            Self::MultiFx => "multi_fx",
            Self::EffectOther => "effect_other",
            Self::Arpeggiator => "arpeggiator",
            Self::MidiProcessor => "midi_processor",
            Self::Metronome => "metronome",
            Self::Analyzer => "analyzer",
            Self::MixerRouting => "mixer_routing",
            Self::Generator => "generator",
            Self::UtilityOther => "utility_other",
        }
    }

    /// Parse a user/agent category/subcategory name or common musical alias.
    pub fn parse(s: &str) -> Option<Self> {
        let words: Vec<String> = s
            .to_ascii_lowercase()
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect();
        match words.join("_").as_str() {
            "synth" | "synths" | "synthesizer" | "wavetable" | "fm" => Some(Self::Synth),
            "organ" | "organs" => Some(Self::Organ),
            "sampler" | "samplers" => Some(Self::Sampler),
            "drum" | "drums" | "drum_machine" | "percussion" => Some(Self::Drum),
            "instrument" | "instrument_other" | "other_instruments" => Some(Self::InstrumentOther),
            "delay_reverb" | "delay" | "reverb" | "echo" | "room" | "space" | "time" => {
                Some(Self::DelayReverb)
            }
            "eq_filter" | "eq" | "equalizer" | "filter" | "filters" | "wah" => Some(Self::EqFilter),
            "dynamics" | "dynamic" | "compressor" | "compressors" | "comp" | "limiter" | "gate" | "expander"
            | "envelope" | "de_esser" | "transient" => Some(Self::Dynamics),
            "drive_amp" | "drive" | "amp" | "amps" | "amplifier" | "cabinet" | "cab" | "distortion"
            | "saturation" | "fuzz" | "overdrive" | "waveshaper" | "simulator" | "guitar" => Some(Self::DriveAmp),
            "modulation" | "mod" | "chorus" | "flanger" | "phaser" | "tremolo" | "vibrato" | "rotary" => {
                Some(Self::Modulation)
            }
            "pitch_spatial" | "pitch" | "pitch_shifter" | "detune" | "spatial" | "spectral"
            | "stereo" | "pan" => Some(Self::PitchSpatial),
            "multi_fx" | "multifx" | "multi_effects" => Some(Self::MultiFx),
            "effect_other" | "other_fx" => Some(Self::EffectOther),
            "arpeggiator" | "arp" | "arp_chord" | "chord" => Some(Self::Arpeggiator),
            "midi_processor" | "midi_fx" | "transpose" => Some(Self::MidiProcessor),
            "metronome" | "rhythm" | "click" => Some(Self::Metronome),
            "analyzer" | "analyzers" | "analyser" | "analysers" | "meter" | "metering" | "tuner" | "scope"
            | "spectrum" => Some(Self::Analyzer),
            "mixer_routing" | "mix_gain" | "mixer" | "gain" | "routing" | "converter" => {
                Some(Self::MixerRouting)
            }
            "generator" | "generators" | "oscillator" | "test" => Some(Self::Generator),
            "utility_other" | "other_utility" => Some(Self::UtilityOther),
            _ => None,
        }
    }
}

/// Plugin format (orthogonal to [`PluginType`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum PluginFormat {
    BuiltIn,
    Clap,
    Lv2,
}

impl PluginFormat {
    pub const ALL: [PluginFormat; 3] = [PluginFormat::BuiltIn, PluginFormat::Clap, PluginFormat::Lv2];

    pub const fn label(self) -> &'static str {
        match self {
            PluginFormat::BuiltIn => "Built-in",
            PluginFormat::Clap => "CLAP",
            PluginFormat::Lv2 => "LV2",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "builtin" | "built-in" | "built_in" | "dsp" => Some(PluginFormat::BuiltIn),
            "clap" => Some(PluginFormat::Clap),
            "lv2" => Some(PluginFormat::Lv2),
            _ => None,
        }
    }
}

/// How a built-in entry is instantiated onto the rack.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum BuiltinAction {
    Dsp(DspKind),
    Arp,
    MidiTranspose,
}

/// One built-in rack unit shown in the Plugins browser and agent listings.
#[derive(Clone, Debug)]
pub struct BuiltinPlugin {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub subtype: PluginSubtype,
    pub action: BuiltinAction,
}

const fn builtin(
    id: &'static str,
    name: &'static str,
    description: &'static str,
    subtype: PluginSubtype,
    action: BuiltinAction,
) -> BuiltinPlugin {
    BuiltinPlugin { id, name, description, subtype, action }
}

/// Every built-in rack unit users can add from the Plugins browser.
pub static BUILTIN_PLUGINS: &[BuiltinPlugin] = &[
    builtin("builtin.delay", "Delay", "stereo echo · time, feedback, mix", PluginSubtype::DelayReverb, BuiltinAction::Dsp(DspKind::Delay)),
    builtin("builtin.reverb", "Reverb", "stereo hall reverb · size, damp, mix", PluginSubtype::DelayReverb, BuiltinAction::Dsp(DspKind::Reverb)),
    builtin("builtin.filter", "Filter", "resonant low-pass filter · cutoff, reso", PluginSubtype::EqFilter, BuiltinAction::Dsp(DspKind::Filter)),
    builtin("builtin.eq3", "EQ 3", "3-band equalizer · low, mid, high", PluginSubtype::EqFilter, BuiltinAction::Dsp(DspKind::Eq3)),
    builtin("builtin.saturation", "Saturation", "warm soft-clip drive · drive, mix", PluginSubtype::DriveAmp, BuiltinAction::Dsp(DspKind::Saturation)),
    builtin("builtin.limiter", "Limiter", "peak brickwall limiter · ceiling", PluginSubtype::Dynamics, BuiltinAction::Dsp(DspKind::Limiter)),
    builtin("builtin.arp", "Arpeggiator", "arpeggiator & chord generator · between keys and synth", PluginSubtype::Arpeggiator, BuiltinAction::Arp),
    builtin("builtin.midi_transpose", "MIDI transpose", "shifts every note · between keys and synth", PluginSubtype::MidiProcessor, BuiltinAction::MidiTranspose),
    builtin("builtin.metronome", "Metronome", "tempo · tap · count-in", PluginSubtype::Metronome, BuiltinAction::Dsp(DspKind::Metronome)),
    builtin("builtin.gain", "Gain", "clean level trim · gain", PluginSubtype::MixerRouting, BuiltinAction::Dsp(DspKind::Gain)),
    builtin("builtin.pan", "Pan", "stereo balance · left to right", PluginSubtype::MixerRouting, BuiltinAction::Dsp(DspKind::Pan)),
];

/// The subcategory of a built-in DSP kind (Plugins browser and insert picker).
pub fn classify_dsp(kind: &DspKind) -> PluginSubtype {
    match kind {
        DspKind::Delay | DspKind::Reverb => PluginSubtype::DelayReverb,
        DspKind::Filter | DspKind::Eq3 => PluginSubtype::EqFilter,
        DspKind::Limiter => PluginSubtype::Dynamics,
        DspKind::Saturation => PluginSubtype::DriveAmp,
        DspKind::Pan | DspKind::Gain | DspKind::Bus => PluginSubtype::MixerRouting,
        DspKind::Metronome => PluginSubtype::Metronome,
    }
}

/// The subcategory of a scanned CLAP plugin, from its declared CLAP features.
pub fn classify_clap(p: &ClapPluginInfo) -> PluginSubtype {
    let feats: Vec<String> = p.features.iter().map(|f| f.to_ascii_lowercase()).collect();
    let has = |needle: &str| feats.iter().any(|f| f == needle);
    let name_low = p.name.to_ascii_lowercase();
    let id_low = p.id.to_ascii_lowercase();

    let is_inst = has("instrument")
        || ((has("synthesizer") || has("sampler") || has("drum") || has("drum-machine"))
            && !has("audio-effect")
            && !has("note-effect"));
    if is_inst {
        return if has("organ") || name_low.contains("organ") || id_low.contains("aeolus") {
            PluginSubtype::Organ
        } else if has("sampler") || name_low.contains("sampler") || id_low.contains("floe") {
            PluginSubtype::Sampler
        } else if has("drum") || has("drum-machine") || name_low.contains("drum") {
            PluginSubtype::Drum
        } else {
            PluginSubtype::Synth
        };
    }
    if has("note-effect") {
        return if name_low.contains("arp") || name_low.contains("chord") || has("arpeggiator") {
            PluginSubtype::Arpeggiator
        } else {
            PluginSubtype::MidiProcessor
        };
    }
    if has("audio-effect") {
        let any = |tags: &[&str]| tags.iter().any(|t| has(t));
        return if has("multi-effects") {
            PluginSubtype::MultiFx
        } else if any(&["reverb", "delay"]) {
            PluginSubtype::DelayReverb
        } else if any(&["equalizer", "filter"]) {
            PluginSubtype::EqFilter
        } else if any(&["compressor", "limiter", "gate", "expander", "transient-shaper", "de-esser"]) {
            PluginSubtype::Dynamics
        } else if has("distortion") {
            PluginSubtype::DriveAmp
        } else if any(&["chorus", "flanger", "phaser", "tremolo"]) {
            PluginSubtype::Modulation
        } else if any(&[
            "pitch-shifter",
            "pitch-correction",
            "frequency-shifter",
            "granular",
            "phase-vocoder",
            "surround",
            "ambisonic",
        ]) {
            PluginSubtype::PitchSpatial
        } else {
            PluginSubtype::EffectOther
        };
    }
    if has("analyzer") || has("note-detector") {
        return PluginSubtype::Analyzer;
    }
    if has("utility") || has("mixing") || has("mastering") {
        return PluginSubtype::UtilityOther;
    }
    // A plugin that declares no CLAP feature at all is hosted as an
    // instrument, as before this taxonomy existed.
    PluginSubtype::Synth
}

/// Is this CLAP plugin an instrument (synth, organ, sampler, drum machine)?
pub fn clap_is_instrument(p: &ClapPluginInfo) -> bool {
    classify_clap(p).plugin_type() == PluginType::Instrument
}

/// Can this CLAP plugin go into an audio insert slot (channel, bus or master)?
/// Audio effects and audio utilities can; instruments and MIDI-only units
/// cannot.
pub fn clap_is_audio_insert(p: &ClapPluginInfo) -> bool {
    matches!(classify_clap(p).plugin_type(), PluginType::Effect | PluginType::Utility)
}

/// Can kabelsalat host this CLAP plugin? CLAP instruments and audio effects
/// (including audio utilities/analyzers) run in `ClapVoice`; CLAP note
/// effects are listed but not hosted yet.
pub fn clap_hostable(p: &ClapPluginInfo) -> bool {
    clap_is_instrument(p) || clap_is_audio_insert(p)
}

/// Is `id` a CLAP audio effect (or utility) rather than an instrument?
/// Uses the scanned descriptor when available, and recognizes the known
/// effect plugin IDs before a scan has run.
pub fn clap_id_is_effect(id: &str, scanned: &[ClapPluginInfo]) -> bool {
    if let Some(p) = scanned.iter().find(|p| p.id == id) {
        return clap_is_audio_insert(p);
    }
    let low = id.to_ascii_lowercase();
    low == "org.surge-synth-team.surge-xt-fx"
        || low.starts_with("michaelwillis.dragonfly.")
        || low == "com.airwindows.consolidated"
        || low.starts_with("com.chowdsp.")
        || low.ends_with("#fx")
        || (low.starts_with("in.lsp-plug.") && !low.contains("multisampler"))
}

/// The subcategory of a scanned LV2 plugin: the scan's `is_instrument` /
/// `is_midi_fx` flags decide the type (rule 72), the declared lv2core class
/// the subcategory, and the plugin's name only where the class says nothing.
pub fn classify_lv2(p: &Lv2PluginInfo) -> PluginSubtype {
    let name = p.name.to_ascii_lowercase();
    let words = NameWords::new(&name);
    let uri = p.uri.to_ascii_lowercase();

    if p.is_instrument {
        return if words.any(&["organ", "tonewheel"]) || uri.contains("b_synth") {
            PluginSubtype::Organ
        } else if words.any(&["sampler", "sfizz", "fluidsynth", "multisampler"]) {
            PluginSubtype::Sampler
        } else if words.any(&["drum", "drums", "drumkit"]) {
            PluginSubtype::Drum
        } else {
            PluginSubtype::Synth
        };
    }
    if p.is_midi_fx {
        return if words.any(&["arp", "arpeggiator", "chord", "chords"]) {
            PluginSubtype::Arpeggiator
        } else {
            PluginSubtype::MidiProcessor
        };
    }

    match p.lv2_class.rsplit('#').next().unwrap_or(&p.lv2_class) {
        "ReverbPlugin" | "DelayPlugin" => PluginSubtype::DelayReverb,
        "EQPlugin" | "ParaEQPlugin" | "MultiEQPlugin" | "FilterPlugin" | "LowpassPlugin" | "HighpassPlugin"
        | "BandpassPlugin" | "CombPlugin" | "AllpassPlugin" => PluginSubtype::EqFilter,
        "DynamicsPlugin" | "CompressorPlugin" | "LimiterPlugin" | "GatePlugin" | "ExpanderPlugin"
        | "EnvelopePlugin" => PluginSubtype::Dynamics,
        "DistortionPlugin" | "WaveshaperPlugin" | "AmplifierPlugin" => PluginSubtype::DriveAmp,
        "SimulatorPlugin" => {
            if words.any(&["whirl", "rotary", "leslie"]) {
                PluginSubtype::Modulation
            } else if words.any(&["room", "reverb"]) {
                PluginSubtype::DelayReverb
            } else {
                PluginSubtype::DriveAmp
            }
        }
        "ModulatorPlugin" | "ChorusPlugin" | "FlangerPlugin" | "PhaserPlugin" => PluginSubtype::Modulation,
        "PitchPlugin" | "SpectralPlugin" | "SpatialPlugin" => PluginSubtype::PitchSpatial,
        "AnalyserPlugin" => PluginSubtype::Analyzer,
        "MixerPlugin" | "ConverterPlugin" => PluginSubtype::MixerRouting,
        "OscillatorPlugin" | "GeneratorPlugin" | "ConstantPlugin" => PluginSubtype::Generator,
        "UtilityPlugin" | "FunctionPlugin" => {
            if words.any(&["meter", "meters", "tuner", "scope", "oscilloscope", "analyzer", "analyser", "spectrum"]) {
                PluginSubtype::Analyzer
            } else if words.any(&["lfo", "oscillator", "osc", "noise"]) {
                PluginSubtype::Generator
            } else if words.any(&["gain", "mixer", "patchbay", "boost", "booster", "pan", "balance", "volume", "split"]) {
                PluginSubtype::MixerRouting
            } else {
                PluginSubtype::UtilityOther
            }
        }
        _ => classify_lv2_by_name(&name, &words),
    }
}

/// An LV2 plugin that declares no specific lv2core class: its name decides,
/// matched by whole words (a substring "amp" would match "sampler").
fn classify_lv2_by_name(name: &str, words: &NameWords) -> PluginSubtype {
    let stem = |stems: &[&str]| stems.iter().any(|s| name.contains(s));
    if stem(&["reverb", "delay", "echo"]) || words.any(&["verb", "room", "hall", "plate"]) {
        PluginSubtype::DelayReverb
    } else if stem(&["equaliz", "filter"]) || words.any(&["eq", "wah", "lowpass", "highpass"]) {
        PluginSubtype::EqFilter
    } else if stem(&["compress", "limiter", "expander", "dynamics"]) || words.any(&["gate", "comp"]) {
        PluginSubtype::Dynamics
    } else if stem(&["distort", "overdrive", "saturat", "fuzz", "cabinet"]) || words.any(&["amp", "drive", "cab"]) {
        PluginSubtype::DriveAmp
    } else if stem(&["chorus", "flanger", "phaser", "tremolo", "vibrato", "rotary"]) {
        PluginSubtype::Modulation
    } else if stem(&["pitch", "detune", "spatial", "stereo"]) {
        PluginSubtype::PitchSpatial
    } else if stem(&["tuner", "meter", "scope", "analy"]) {
        PluginSubtype::Analyzer
    } else if stem(&["patchbay", "mixer"]) || words.any(&["gain", "pan", "split", "matrix"]) {
        PluginSubtype::MixerRouting
    } else {
        PluginSubtype::EffectOther
    }
}

/// The lower-case words of a plugin name ("GxAmp-Stereo" → "gxamp", "stereo").
struct NameWords<'a>(Vec<&'a str>);

impl<'a> NameWords<'a> {
    fn new(lower_name: &'a str) -> Self {
        Self(lower_name.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).collect())
    }

    fn any(&self, words: &[&str]) -> bool {
        self.0.iter().any(|w| words.contains(w))
    }
}

/// Can this LV2 plugin go into an audio insert slot (channel or master)?
/// Audio effects and audio utilities can; instruments and MIDI-only units
/// cannot.
pub fn lv2_is_audio_insert(p: &Lv2PluginInfo) -> bool {
    !p.is_instrument && !p.is_midi_fx
}

/// Human-readable label for an lv2core class local name (`"ParaEQPlugin"`
/// → `"Parametric EQ"`). Empty when generic or unknown.
pub fn lv2_class_label(lv2_class: &str) -> &'static str {
    match lv2_class.rsplit('#').next().unwrap_or(lv2_class) {
        "InstrumentPlugin" => "Instrument",
        "OscillatorPlugin" => "Oscillator",
        "GeneratorPlugin" => "Generator",
        "ConstantPlugin" => "Constant",
        "ReverbPlugin" => "Reverb",
        "DelayPlugin" => "Delay",
        "EQPlugin" => "EQ",
        "ParaEQPlugin" => "Parametric EQ",
        "MultiEQPlugin" => "Multiband EQ",
        "FilterPlugin" => "Filter",
        "LowpassPlugin" => "Lowpass Filter",
        "HighpassPlugin" => "Highpass Filter",
        "BandpassPlugin" => "Bandpass Filter",
        "CombPlugin" => "Comb Filter",
        "AllpassPlugin" => "Allpass Filter",
        "DynamicsPlugin" => "Dynamics",
        "CompressorPlugin" => "Compressor",
        "LimiterPlugin" => "Limiter",
        "GatePlugin" => "Gate",
        "ExpanderPlugin" => "Expander",
        "EnvelopePlugin" => "Envelope",
        "DistortionPlugin" => "Distortion",
        "WaveshaperPlugin" => "Waveshaper",
        "AmplifierPlugin" => "Amplifier",
        "SimulatorPlugin" => "Simulator",
        "ModulatorPlugin" => "Modulator",
        "ChorusPlugin" => "Chorus",
        "FlangerPlugin" => "Flanger",
        "PhaserPlugin" => "Phaser",
        "PitchPlugin" => "Pitch Shifter",
        "SpectralPlugin" => "Spectral",
        "SpatialPlugin" => "Spatial",
        "AnalyserPlugin" => "Analyzer",
        "MixerPlugin" => "Mixer",
        "ConverterPlugin" => "Converter",
        "UtilityPlugin" => "Utility",
        "FunctionPlugin" => "Function",
        _ => "",
    }
}

/// Card subtitle for an LV2 plugin: author and class, never a raw URI unless
/// the plugin declares neither.
pub fn lv2_subtitle(p: &Lv2PluginInfo) -> String {
    let cls = lv2_class_label(&p.lv2_class);
    match (p.author.trim(), cls) {
        ("", "") => p.uri.clone(),
        ("", cls) => format!("LV2 · {cls}"),
        (author, "") => author.to_string(),
        (author, cls) => format!("{author} · {cls}"),
    }
}

/// Card subtitle for a CLAP plugin: vendor and its most telling feature tag.
pub fn clap_subtitle(p: &ClapPluginInfo) -> String {
    let vendor = if p.vendor == "?" { "" } else { p.vendor.trim() };
    let detail = p
        .features
        .iter()
        .map(String::as_str)
        .find(|f| {
            !matches!(
                f.to_ascii_lowercase().as_str(),
                "instrument" | "audio-effect" | "note-effect" | "stereo" | "mono" | "free and open source"
            )
        })
        .unwrap_or_else(|| classify_clap(p).label());
    if vendor.is_empty() {
        detail.to_string()
    } else {
        format!("{vendor} · {detail}")
    }
}

/// Multi-word, case-insensitive search: every word of `query` must appear in
/// at least one of `fields`.
pub fn matches_query(fields: &[&str], query: &str) -> bool {
    let lowers: Vec<String> = fields.iter().map(|f| f.to_lowercase()).collect();
    query.split_whitespace().all(|word| {
        let w = word.to_lowercase();
        lowers.iter().any(|f| f.contains(&w))
    })
}

/// Where a browsable plugin comes from.
#[derive(Clone, Copy, Debug)]
pub enum Source<'a> {
    Builtin(&'static BuiltinPlugin),
    Clap(&'a ClapPluginInfo),
    Lv2(&'a Lv2PluginInfo),
}

/// One plugin of any format, classified once: what the Plugins browser, the
/// assistant's `list_plugins` and MCP's `list_plugins` all filter.
#[derive(Clone, Copy, Debug)]
pub struct Entry<'a> {
    pub source: Source<'a>,
    pub sub: PluginSubtype,
}

impl Entry<'_> {
    pub fn ty(&self) -> PluginType {
        self.sub.plugin_type()
    }

    pub fn format(&self) -> PluginFormat {
        match self.source {
            Source::Builtin(_) => PluginFormat::BuiltIn,
            Source::Clap(_) => PluginFormat::Clap,
            Source::Lv2(_) => PluginFormat::Lv2,
        }
    }

    pub fn name(&self) -> &str {
        match self.source {
            Source::Builtin(b) => b.name,
            Source::Clap(p) => &p.name,
            Source::Lv2(p) => &p.name,
        }
    }

    /// Whether kabelsalat can put this plugin on the rack today.
    pub fn hostable(&self) -> bool {
        match self.source {
            Source::Clap(p) => clap_hostable(p),
            Source::Builtin(_) | Source::Lv2(_) => true,
        }
    }

    fn matches_query(&self, query: &str) -> bool {
        let (ty, sub) = (self.ty().label(), self.sub.label());
        match self.source {
            Source::Builtin(b) => matches_query(&[b.name, b.description, ty, sub, "built-in builtin"], query),
            Source::Clap(p) => {
                matches_query(&[&p.name, &p.vendor, &p.id, &p.features.join(" "), ty, sub, "clap"], query)
            }
            Source::Lv2(p) => matches_query(
                &[&p.name, &p.author, &p.uri, lv2_class_label(&p.lv2_class), ty, sub, "lv2"],
                query,
            ),
        }
    }
}

/// Every built-in unit, then the CLAP and LV2 plugins in scan order (the LV2
/// scan lists instruments first, rule 154), each classified.
pub fn entries<'a>(clap: &'a [ClapPluginInfo], lv2: &'a [Lv2PluginInfo]) -> Vec<Entry<'a>> {
    let builtins = BUILTIN_PLUGINS.iter().map(|b| Entry { source: Source::Builtin(b), sub: b.subtype });
    let claps = clap.iter().map(|p| Entry { source: Source::Clap(p), sub: classify_clap(p) });
    let lv2s = lv2.iter().map(|p| Entry { source: Source::Lv2(p), sub: classify_lv2(p) });
    builtins.chain(claps).chain(lv2s).collect()
}

/// Browsing order: by subcategory (which orders the types too), what can be
/// hosted first, then format and name.
pub fn sort_for_browsing(entries: &mut [Entry<'_>]) {
    entries.sort_by_cached_key(|e| (e.sub, !e.hostable(), e.format(), e.name().to_lowercase()));
}

/// The narrowing a `list_plugins` call or the Plugins browser asks for.
#[derive(Clone, Debug, Default)]
pub struct PluginListFilter {
    pub plugin_type: Option<PluginType>,
    pub subtype: Option<PluginSubtype>,
    pub format: Option<PluginFormat>,
    pub query: String,
    pub limit: Option<usize>,
}

impl PluginListFilter {
    /// Parse a tool call's arguments; an unknown value is an error naming the
    /// accepted ones, so the model can correct the call (rule 77).
    pub fn from_args(args: &serde_json::Value) -> Result<Self, String> {
        let arg = |keys: &[&str]| {
            keys.iter()
                .find_map(|k| args.get(*k).and_then(|v| v.as_str()))
                .map(str::trim)
                .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("all"))
        };
        let plugin_type = arg(&["type", "plugin_type"])
            .map(|raw| {
                PluginType::parse(raw)
                    .ok_or_else(|| format!("unknown plugin type '{raw}' (use instrument | effect | midi_fx | utility | all)"))
            })
            .transpose()?;
        let subtype = arg(&["category", "subcategory"])
            .map(|raw| {
                PluginSubtype::parse(raw).ok_or_else(|| {
                    format!(
                        "unknown category '{raw}' (e.g. synth, organ, sampler, delay_reverb, eq_filter, dynamics, drive_amp, modulation, pitch_spatial, multi_fx, arpeggiator, analyzer, mixer_routing)"
                    )
                })
            })
            .transpose()?;
        let format = arg(&["format"])
            .map(|raw| PluginFormat::parse(raw).ok_or_else(|| format!("unknown format '{raw}' (use builtin | clap | lv2 | all)")))
            .transpose()?;
        Ok(Self {
            plugin_type,
            subtype,
            format,
            query: args.get("query").and_then(|v| v.as_str()).unwrap_or("").trim().to_string(),
            limit: args.get("limit").and_then(|v| v.as_u64()).map(|n| (n as usize).clamp(1, 300)),
        })
    }

    fn is_filtered(&self) -> bool {
        self.plugin_type.is_some() || self.subtype.is_some() || self.format.is_some() || !self.query.is_empty()
    }

    pub fn matches(&self, e: &Entry<'_>) -> bool {
        self.plugin_type.is_none_or(|t| t == e.ty())
            && self.subtype.is_none_or(|s| s == e.sub)
            && self.format.is_none_or(|f| f == e.format())
            && e.matches_query(&self.query)
    }

    fn apply<'a>(&self, clap: &'a [ClapPluginInfo], lv2: &'a [Lv2PluginInfo]) -> Vec<Entry<'a>> {
        entries(clap, lv2).into_iter().filter(|e| self.matches(e)).collect()
    }
}

/// The in-app assistant's `list_plugins` answer: plugins grouped by type,
/// each section capped, with how to narrow what was cut.
pub fn format_list_plugins(clap: &[ClapPluginInfo], lv2: &[Lv2PluginInfo], filter: &PluginListFilter) -> String {
    let hits = filter.apply(clap, lv2);
    let count = |ty: PluginType| hits.iter().filter(|e| e.ty() == ty).count();
    let mut out = format!(
        "Plugins by type — Instruments: {}, Effects: {}, MIDI FX: {}, Utilities: {} (filter with type/category/format/query)\n",
        count(PluginType::Instrument),
        count(PluginType::Effect),
        count(PluginType::MidiFx),
        count(PluginType::Utility),
    );
    let cap = filter.limit.unwrap_or(if filter.is_filtered() { 60 } else { 24 });
    for ty in PluginType::ALL {
        let section: Vec<&Entry> = hits.iter().filter(|e| e.ty() == ty).collect();
        if section.is_empty() {
            continue;
        }
        out.push_str(&format!("\n{} ({}):\n", ty.label(), section.len()));
        if section.len() > cap {
            let subs: Vec<String> = PluginSubtype::subtypes_for(ty)
                .iter()
                .filter_map(|&sub| {
                    let n = section.iter().filter(|e| e.sub == sub).count();
                    (n > 0).then(|| format!("{}={n}", sub.slug()))
                })
                .collect();
            out.push_str(&format!("  categories: {}\n", subs.join(", ")));
        }
        for e in section.iter().take(cap) {
            let sub = e.sub.label();
            let line = match e.source {
                Source::Builtin(b) => format!("[built-in · {sub}] {} ({}) — {}", b.name, b.id, b.description),
                Source::Clap(p) => {
                    let note = if clap_hostable(p) { "" } else { " (not hostable yet)" };
                    format!("[CLAP · {sub}] {} [{}]{note}", p.name, p.id)
                }
                Source::Lv2(p) if p.is_instrument => format!("{} [instrument] — {} (LV2 · {sub})", p.name, p.uri),
                Source::Lv2(p) if p.is_midi_fx => format!("{} [midi_fx] — {} (LV2 · {sub})", p.name, p.uri),
                Source::Lv2(p) => format!("{} [{sub}] — {} (LV2)", p.name, p.uri),
            };
            out.push_str(&format!("  {line}\n"));
        }
        if section.len() > cap {
            out.push_str(&format!(
                "  … {} more {} (pass type='{}', category='…', or query='…' to narrow)\n",
                section.len() - cap,
                ty.label().to_ascii_lowercase(),
                ty.slug()
            ));
        }
    }
    out
}

/// MCP's `list_plugins` answer. Keeps the earlier `clap`, `lv2` and
/// `lv2_count` fields and adds `builtin`, `counts`, `effect_categories` and a
/// `type`/`category` on every item.
pub fn list_plugins_json(clap: &[ClapPluginInfo], lv2: &[Lv2PluginInfo], filter: &PluginListFilter) -> serde_json::Value {
    let hits = filter.apply(clap, lv2);
    let limit = filter.limit.unwrap_or(60);
    let counts: serde_json::Map<_, _> = PluginType::ALL
        .iter()
        .map(|&ty| (ty.slug().to_string(), serde_json::json!(hits.iter().filter(|e| e.ty() == ty).count())))
        .collect();
    let effect_categories: serde_json::Map<_, _> = PluginSubtype::EFFECT_ALL
        .iter()
        .filter_map(|&sub| {
            let n = hits.iter().filter(|e| e.sub == sub).count();
            (n > 0).then(|| (sub.slug().to_string(), serde_json::json!(n)))
        })
        .collect();
    let tagged = |e: &Entry, mut item: serde_json::Value| {
        item["type"] = serde_json::json!(e.ty().slug());
        item["category"] = serde_json::json!(e.sub.slug());
        item["category_label"] = serde_json::json!(e.sub.label());
        item
    };
    let mut builtin = Vec::new();
    let mut clap_json = Vec::new();
    let mut lv2_json = Vec::new();
    let mut lv2_count = 0;
    for e in &hits {
        match e.source {
            Source::Builtin(b) => {
                builtin.push(tagged(e, serde_json::json!({"id": b.id, "name": b.name, "description": b.description})))
            }
            Source::Clap(p) if clap_json.len() < limit => clap_json.push(tagged(
                e,
                serde_json::json!({"id": p.id, "name": p.name, "vendor": p.vendor, "features": p.features, "hostable": clap_hostable(p)}),
            )),
            Source::Clap(_) => {}
            Source::Lv2(p) => {
                lv2_count += 1;
                if lv2_json.len() < limit {
                    lv2_json.push(tagged(
                        e,
                        serde_json::json!({
                            "uri": p.uri,
                            "name": p.name,
                            "author": p.author,
                            "lv2_class": p.lv2_class,
                            "num_ports": p.num_ports,
                            "is_instrument": p.is_instrument,
                            "is_midi_fx": p.is_midi_fx,
                        }),
                    ));
                }
            }
        }
    }
    serde_json::json!({
        "counts": counts,
        "effect_categories": effect_categories,
        "builtin": builtin,
        "clap": clap_json,
        "lv2_count": lv2_count,
        "lv2": lv2_json,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lv2(uri: &str, name: &str, lv2_class: &str, is_instrument: bool, is_midi_fx: bool) -> Lv2PluginInfo {
        Lv2PluginInfo {
            uri: uri.into(),
            name: name.into(),
            author: String::new(),
            lv2_class: lv2_class.into(),
            num_ports: 4,
            is_instrument,
            is_midi_fx,
        }
    }

    fn clap(id: &str, name: &str, features: &[&str]) -> ClapPluginInfo {
        ClapPluginInfo {
            id: id.into(),
            name: name.into(),
            vendor: "Vendor".into(),
            features: features.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn clap_plugins_classify_into_instruments_effects_and_midi_fx() {
        let surge = clap("org.surge-synth-team.surge-xt", "Surge XT", &["instrument", "synthesizer", "stereo"]);
        assert_eq!(classify_clap(&surge), PluginSubtype::Synth);
        assert!(clap_hostable(&surge));

        let aeolus = clap("com.ArthurBenilov.Aeolus", "Aeolus", &["instrument", "virtual analog"]);
        assert_eq!(classify_clap(&aeolus), PluginSubtype::Organ);
        assert!(clap_hostable(&aeolus));

        let floe = clap("com.floe-audio.floe", "Floe", &["instrument", "sampler"]);
        assert_eq!(classify_clap(&floe), PluginSubtype::Sampler);

        let surge_fx = clap(
            "org.surge-synth-team.surge-xt-fx",
            "Surge XT Effects",
            &["audio-effect", "filter", "delay", "reverb", "multi-effects"],
        );
        assert_eq!(classify_clap(&surge_fx), PluginSubtype::MultiFx);
        assert!(clap_hostable(&surge_fx));
        assert!(!clap_is_instrument(&surge_fx));
        assert!(clap_is_audio_insert(&surge_fx));

        let arp = clap("org.example.arp", "BlueArp", &["note-effect"]);
        assert_eq!(classify_clap(&arp), PluginSubtype::Arpeggiator);
        assert!(!clap_hostable(&arp));
        assert!(!clap_is_audio_insert(&arp));

        let scope = clap("org.example.scope", "Oscilloscope", &["analyzer"]);
        assert_eq!(classify_clap(&scope), PluginSubtype::Analyzer);
        assert!(clap_hostable(&scope));
        assert!(clap_is_audio_insert(&scope));
    }

    #[test]
    fn lv2_plugins_classify_across_every_major_core_class() {
        assert_eq!(
            classify_lv2(&lv2("http://gareus.org/oss/lv2/b_synth", "setBfree DSP Tonewheel Organ", "InstrumentPlugin", true, false)),
            PluginSubtype::Organ
        );
        assert_eq!(
            classify_lv2(&lv2("urn:distrho:vitalium", "Vitalium", "InstrumentPlugin", true, false)),
            PluginSubtype::Synth
        );
        assert_eq!(
            classify_lv2(&lv2("http://lsp-plug.in/plugins/lv2/multisampler_x12", "LSP Multisampler x12", "InstrumentPlugin", true, false)),
            PluginSubtype::Sampler
        );
        assert_eq!(
            classify_lv2(&lv2("http://kxstudio.sf.net/carla/plugins/miditranspose", "MIDI Transpose", "UtilityPlugin", false, true)),
            PluginSubtype::MidiProcessor
        );
        assert_eq!(
            classify_lv2(&lv2("http://guitarix.sourceforge.net/plugins/gx_zita_rev1_stereo#_zita_rev1_stereo", "GxZita_rev1-Stereo", "ReverbPlugin", false, false)),
            PluginSubtype::DelayReverb
        );
        assert_eq!(
            classify_lv2(&lv2("http://lsp-plug.in/plugins/lv2/para_equalizer_x16_lr", "LSP Parametric Equalizer x16", "ParaEQPlugin", false, false)),
            PluginSubtype::EqFilter
        );
        assert_eq!(
            classify_lv2(&lv2("http://lsp-plug.in/plugins/lv2/compressor_lr", "LSP Compressor", "CompressorPlugin", false, false)),
            PluginSubtype::Dynamics
        );
        assert_eq!(
            classify_lv2(&lv2("http://guitarix.sourceforge.net/plugins/gx_amp#GUITARIX", "GxAmplifier-X", "SimulatorPlugin", false, false)),
            PluginSubtype::DriveAmp
        );
        assert_eq!(
            classify_lv2(&lv2("http://gareus.org/oss/lv2/b_whirl#extended", "x42 Whirl - Rotary Speaker", "SimulatorPlugin", false, false)),
            PluginSubtype::Modulation
        );
        assert_eq!(
            classify_lv2(&lv2("http://guitarix.sourceforge.net/plugins/gx_chorus_stereo#_chorus_stereo", "GxChorus-Stereo", "ChorusPlugin", false, false)),
            PluginSubtype::Modulation
        );
        assert_eq!(
            classify_lv2(&lv2("http://guitarix.sourceforge.net/plugins/gx_oc_2_#_oc_2_", "GxOC-2", "PitchPlugin", false, false)),
            PluginSubtype::PitchSpatial
        );
        assert_eq!(
            classify_lv2(&lv2("http://guitarix.sourceforge.net/plugins/gxtuner#tuner", "GxTuner", "AnalyserPlugin", false, false)),
            PluginSubtype::Analyzer
        );
        assert_eq!(
            classify_lv2(&lv2("http://kxstudio.sf.net/carla/plugins/audiogain", "Audio Gain (Mono)", "UtilityPlugin", false, false)),
            PluginSubtype::MixerRouting
        );
        assert_eq!(
            classify_lv2(&lv2("http://lsp-plug.in/plugins/lv2/oscillator_mono", "LSP Oscillator Mono", "OscillatorPlugin", false, false)),
            PluginSubtype::Generator
        );
    }

    #[test]
    fn lv2_midi_fx_are_excluded_from_audio_inserts() {
        let midi = lv2("http://kxstudio.sf.net/carla/plugins/miditranspose", "MIDI Transpose", "UtilityPlugin", false, true);
        let inst = lv2("urn:distrho:vitalium", "Vitalium", "InstrumentPlugin", true, false);
        let fx = lv2("http://lsp-plug.in/plugins/lv2/compressor_lr", "LSP Compressor", "CompressorPlugin", false, false);
        let util = lv2("http://guitarix.sourceforge.net/plugins/gxtuner#tuner", "GxTuner", "AnalyserPlugin", false, false);
        assert!(!lv2_is_audio_insert(&midi));
        assert!(!lv2_is_audio_insert(&inst));
        assert!(lv2_is_audio_insert(&fx));
        assert!(lv2_is_audio_insert(&util));
    }

    #[test]
    fn list_plugins_filters_by_type_category_format_and_query() {
        let claps = vec![
            clap("org.surge-synth-team.surge-xt", "Surge XT", &["instrument", "synthesizer"]),
            clap("org.surge-synth-team.surge-xt-fx", "Surge XT Effects", &["audio-effect", "multi-effects"]),
        ];
        let lv2s = vec![
            lv2("urn:distrho:vitalium", "Vitalium", "InstrumentPlugin", true, false),
            lv2("http://example.org/rev", "Lush Verb", "ReverbPlugin", false, false),
            lv2("http://example.org/comp", "Glue Comp", "CompressorPlugin", false, false),
            lv2("http://example.org/tuner", "Stage Tuner", "AnalyserPlugin", false, false),
        ];

        let f_inst = PluginListFilter::from_args(&serde_json::json!({"type": "instruments"})).unwrap();
        let txt = format_list_plugins(&claps, &lv2s, &f_inst);
        assert!(txt.contains("Surge XT"), "{txt}");
        assert!(txt.contains("Vitalium"), "{txt}");
        assert!(!txt.contains("Lush Verb"), "{txt}");
        assert!(!txt.contains("Surge XT Effects"), "{txt}");

        let f_dyn = PluginListFilter::from_args(&serde_json::json!({"category": "dynamics", "format": "lv2"})).unwrap();
        let json = list_plugins_json(&claps, &lv2s, &f_dyn);
        assert_eq!(json["lv2_count"], 1, "{json}");
        assert_eq!(json["lv2"][0]["name"], "Glue Comp");
        assert_eq!(json["lv2"][0]["type"], "effect");
        assert_eq!(json["lv2"][0]["category"], "dynamics");
        assert_eq!(json["builtin"].as_array().unwrap().len(), 0);
        assert_eq!(json["clap"].as_array().unwrap().len(), 0);

        let f_q = PluginListFilter::from_args(&serde_json::json!({"query": "stage analyzer"})).unwrap();
        let json_q = list_plugins_json(&claps, &lv2s, &f_q);
        assert_eq!(json_q["lv2_count"], 1, "{json_q}");
        assert_eq!(json_q["lv2"][0]["name"], "Stage Tuner");
    }

    #[test]
    fn a_builtin_effect_has_one_category_in_the_browser_and_the_insert_picker() {
        for b in BUILTIN_PLUGINS {
            if let BuiltinAction::Dsp(kind) = &b.action {
                assert_eq!(classify_dsp(kind), b.subtype, "{}", b.id);
            }
        }
    }

    #[test]
    fn an_unclassed_lv2_plugin_is_sorted_by_whole_words_of_its_name_never_its_uri() {
        let plain = |uri: &str, name: &str| lv2(uri, name, "", false, false);
        assert_eq!(classify_lv2(&plain("http://example.org/a", "Bass Amp")), PluginSubtype::DriveAmp);
        assert_eq!(classify_lv2(&plain("http://example.org/amp", "Thing")), PluginSubtype::EffectOther, "the uri is no evidence");
        assert_eq!(classify_lv2(&plain("http://x.org/b", "Champion")), PluginSubtype::EffectOther, "'amp' inside a word");
        assert_eq!(classify_lv2(&plain("http://x.org/c", "Navigate")), PluginSubtype::EffectOther, "'gate' inside a word");
        assert_eq!(classify_lv2(&plain("http://x.org/d", "Noise Gate")), PluginSubtype::Dynamics);
        assert_eq!(classify_lv2(&plain("http://x.org/e", "Tape Echo")), PluginSubtype::DelayReverb);
    }

    #[test]
    fn a_category_parses_from_its_label_slug_or_alias() {
        for sub in PluginSubtype::EFFECT_ALL.iter().chain(PluginSubtype::UTILITY_ALL).chain(PluginSubtype::INSTRUMENT_ALL) {
            assert_eq!(PluginSubtype::parse(sub.slug()), Some(*sub), "slug {}", sub.slug());
        }
        assert_eq!(PluginSubtype::parse("Delay & Reverb"), Some(PluginSubtype::DelayReverb));
        assert_eq!(PluginSubtype::parse("drive-amp"), Some(PluginSubtype::DriveAmp));
        assert_eq!(PluginSubtype::parse("Mix & Gain"), Some(PluginSubtype::MixerRouting));
        assert_eq!(PluginType::parse("midi-fx"), Some(PluginType::MidiFx));
    }
}
