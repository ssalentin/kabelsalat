//! Main controls: the few parameters of a hosted instrument that its designer
//! marked as most important. One list per plugin instance feeds the rack face
//! (knobs), the agent (`list_params`) and knob-name resolution, so "Macro 1"
//! means the same parameter everywhere.

/// Controls kept per instrument: one CLAP remote-controls page.
pub const MAX_MAIN_CONTROLS: usize = 8;
/// Knobs a face shows from the list (beside its built-in knobs).
pub const FACE_MAIN_CONTROLS: usize = 4;

/// One main control. `id` is what the host sends to move it: the CLAP param
/// id, or the LV2 control-port ordinal (the engine's `SetParam` id).
#[derive(Clone, Debug, PartialEq)]
pub struct MainControl {
    pub id: u32,
    pub name: String,
    /// The remote-controls page that lists it, else the parameter's own group.
    pub page: String,
    pub min: f64,
    pub max: f64,
    pub default: f64,
}

impl MainControl {
    pub fn normalized(&self, value: f64) -> f32 {
        (((value - self.min) / (self.max - self.min).max(1e-9)) as f32).clamp(0.0, 1.0)
    }

    pub fn plain(&self, norm: f32) -> f64 {
        self.min + norm.clamp(0.0, 1.0) as f64 * (self.max - self.min)
    }
}

/// Which rule found the controls (see [`resolve`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MainSource {
    RemoteControls,
    Designation,
    #[default]
    Fallback,
}

impl MainSource {
    pub fn as_str(self) -> &'static str {
        match self {
            MainSource::RemoteControls => "remote controls",
            MainSource::Designation => "designation",
            MainSource::Fallback => "fallback",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MainControls {
    pub source: MainSource,
    pub controls: Vec<MainControl>,
}

impl MainControls {
    /// The controls a face shows as knobs.
    pub fn face(&self) -> &[MainControl] {
        &self.controls[..self.controls.len().min(FACE_MAIN_CONTROLS)]
    }

    /// A control by name (any case), as agents and CC bindings name it.
    pub fn find(&self, name: &str) -> Option<&MainControl> {
        let q = name.trim();
        self.controls.iter().find(|c| c.name.eq_ignore_ascii_case(q))
    }
}

/// A parameter as its plugin describes it, reduced to what the rule reads.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub id: u32,
    pub name: String,
    pub group: String,
    pub min: f64,
    pub max: f64,
    pub default: f64,
    /// Automatable, shown to users and writable.
    pub usable: bool,
    /// Switch or list rather than a continuous value.
    pub stepped: bool,
    /// Carries a parameter designation (LV2 `param:gain`, `param:cutoffFrequency`, …).
    pub designated: bool,
}

/// A CLAP remote-controls page: up to 8 slots, each a param id or empty.
#[derive(Clone, Debug)]
pub struct RemotePage {
    pub name: String,
    pub slots: [Option<u32>; 8],
}

/// The rule, first match wins, at most [`MAX_MAIN_CONTROLS`], no control twice:
/// 1. remote controls: the filled slots of the plugin's remote-controls pages
///    in page order, as far as they name a usable parameter the plugin gave
///    a name (Surge XT calls an unassigned macro "M1: -": it moves nothing);
/// 2. designation: the usable parameters that carry a designation, in declared
///    order, filled up from rule 3;
/// 3. fallback: the usable continuous parameters, those named "Macro…" first
///    (a macro is the designer's own main control), then in declared order.
///
/// A parameter with an empty range is never a main control.
pub fn resolve(pages: &[RemotePage], params: &[Candidate]) -> MainControls {
    let ok = |c: &&Candidate| c.usable && c.max > c.min;
    let mut controls: Vec<MainControl> = Vec::new();
    let push = |controls: &mut Vec<MainControl>, c: &Candidate, page: &str| {
        if controls.len() < MAX_MAIN_CONTROLS && !controls.iter().any(|m| m.id == c.id) {
            controls.push(MainControl {
                id: c.id,
                name: c.name.clone(),
                page: page.to_string(),
                min: c.min,
                max: c.max,
                default: c.default,
            });
        }
    };

    for page in pages {
        for id in page.slots.iter().flatten() {
            if let Some(c) = params.iter().filter(ok).find(|c| c.id == *id && !is_placeholder(&c.name)) {
                push(&mut controls, c, &page.name);
            }
        }
    }
    if !controls.is_empty() {
        return MainControls { source: MainSource::RemoteControls, controls };
    }

    for c in params.iter().filter(ok).filter(|c| c.designated) {
        push(&mut controls, c, &c.group);
    }
    let source = if controls.is_empty() { MainSource::Fallback } else { MainSource::Designation };

    let continuous = || params.iter().filter(ok).filter(|c| !c.stepped);
    let is_macro = |c: &&Candidate| c.name.trim_start().to_ascii_lowercase().starts_with("macro");
    for c in continuous().filter(is_macro).chain(continuous().filter(|c| !is_macro(c))) {
        push(&mut controls, c, &c.group);
    }
    MainControls { source, controls }
}

/// What a name says after its index tag: the text after the last colon
/// ("M1: Cutoff" → "Cutoff").
fn untagged(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name).trim()
}

/// A name that names nothing: no letter or digit after its tag ("M1: -", "-", "").
fn is_placeholder(name: &str) -> bool {
    !untagged(name).chars().any(char::is_alphanumeric)
}

/// Long words a knob label shortens, as hardware panels print them.
const ABBREVIATIONS: &[(&str, &str)] = &[
    ("VOLUME", "VOL"),
    ("RESONANCE", "RESO"),
    ("FREQUENCY", "FREQ"),
    ("OSCILLATOR", "OSC"),
    ("MODULATION", "MOD"),
    ("FEEDBACK", "FDBK"),
    ("BALANCE", "BAL"),
    ("WAVESHAPER", "SHAPER"),
];

/// A knob label for a control name: upper case, without its index tag, at
/// most `max_chars`. Long names keep their LAST words ("A Filter 1 Cutoff" →
/// "1 CUTOFF"), which say what the control does; a single long word is cut.
pub fn knob_label(name: &str, max_chars: usize) -> String {
    let words: Vec<String> = untagged(name)
        .to_uppercase()
        .split_whitespace()
        .map(|w| ABBREVIATIONS.iter().find(|(long, _)| *long == w).map_or(w.to_string(), |(_, short)| short.to_string()))
        .collect();
    let mut label = String::new();
    for word in words.iter().rev() {
        let joined = if label.is_empty() { word.clone() } else { format!("{word} {label}") };
        if joined.chars().count() > max_chars {
            break;
        }
        label = joined;
    }
    if label.is_empty() {
        words.last().map(|w| w.chars().take(max_chars).collect()).unwrap_or_default()
    } else {
        label
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn param(id: u32, name: &str) -> Candidate {
        Candidate {
            id,
            name: name.into(),
            group: "Osc".into(),
            min: 0.0,
            max: 1.0,
            default: 0.5,
            usable: true,
            stepped: false,
            designated: false,
        }
    }

    fn page(name: &str, ids: &[Option<u32>]) -> RemotePage {
        let mut slots = [None; 8];
        slots[..ids.len()].copy_from_slice(ids);
        RemotePage { name: name.into(), slots }
    }

    fn names(m: &MainControls) -> Vec<&str> {
        m.controls.iter().map(|c| c.name.as_str()).collect()
    }

    #[test]
    fn remote_pages_win_and_keep_slot_order_without_empty_slots() {
        let params = vec![param(10, "Pitch"), param(20, "Cutoff"), param(30, "Resonance"), param(40, "Drive")];
        let pages = vec![
            page("Main", &[Some(30), None, Some(20)]),
            page("More", &[Some(40), Some(20)]),
        ];
        let m = resolve(&pages, &params);
        assert_eq!(m.source, MainSource::RemoteControls);
        assert_eq!(names(&m), ["Resonance", "Cutoff", "Drive"], "slot order, empty slot skipped, no repeat");
        assert_eq!(m.controls[0].page, "Main");
        assert_eq!(m.controls[2].page, "More", "a short first page continues on the next");
    }

    #[test]
    fn a_slot_naming_an_unknown_hidden_or_rangeless_param_is_skipped() {
        let mut hidden = param(2, "Hidden");
        hidden.usable = false;
        let mut flat = param(3, "Flat");
        flat.max = flat.min;
        let params = vec![param(1, "Level"), hidden, flat];
        let m = resolve(&[page("Main", &[Some(99), Some(2), Some(3), Some(1)])], &params);
        assert_eq!(names(&m), ["Level"]);
    }

    #[test]
    fn a_slot_the_plugin_left_unnamed_gives_way_to_the_next_page() {
        let params = vec![param(1, "M1: -"), param(2, "M2: Cutoff"), param(3, "-"), param(4, "A Osc 1 Volume")];
        let pages = vec![page("Macros", &[Some(1), Some(2), Some(3)]), page("Scene A Mixer", &[Some(4)])];
        let m = resolve(&pages, &params);
        assert_eq!(m.source, MainSource::RemoteControls);
        assert_eq!(names(&m), ["M2: Cutoff", "A Osc 1 Volume"]);
    }

    #[test]
    fn pages_naming_nothing_usable_fall_through_to_the_next_rule() {
        let params = vec![param(1, "Level")];
        let m = resolve(&[page("Empty", &[None, Some(99)])], &params);
        assert_eq!(m.source, MainSource::Fallback);
        assert_eq!(names(&m), ["Level"]);
    }

    #[test]
    fn designated_params_come_first_then_the_declared_order_fills_up() {
        let mut gain = param(3, "Gain");
        gain.designated = true;
        let mut cutoff = param(5, "Cutoff");
        cutoff.designated = true;
        let params = vec![param(1, "Attack"), param(2, "Decay"), gain, param(4, "Sustain"), cutoff];
        let m = resolve(&[], &params);
        assert_eq!(m.source, MainSource::Designation);
        assert_eq!(names(&m), ["Gain", "Cutoff", "Attack", "Decay", "Sustain"]);
    }

    #[test]
    fn fallback_takes_macros_first_then_the_first_usable_continuous_params() {
        let mut bypass = param(1, "Bypass");
        bypass.stepped = true;
        let mut internal = param(2, "Internal");
        internal.usable = false;
        let mut params = vec![bypass, internal, param(3, "Attack"), param(4, "Macro 2"), param(5, "Macro 1")];
        params.extend((6..20).map(|i| param(i, &format!("P{i}"))));
        let m = resolve(&[], &params);
        assert_eq!(m.source, MainSource::Fallback);
        assert_eq!(m.controls.len(), MAX_MAIN_CONTROLS);
        assert_eq!(&names(&m)[..4], ["Macro 2", "Macro 1", "Attack", "P6"], "macros in declared order, switch and hidden skipped");
        assert_eq!(m.face().len(), FACE_MAIN_CONTROLS);
    }

    #[test]
    fn a_plugin_without_parameters_has_no_main_controls() {
        let m = resolve(&[], &[]);
        assert!(m.controls.is_empty() && m.face().is_empty());
    }

    #[test]
    fn controls_map_between_plain_and_normalized_and_resolve_by_name() {
        let mut cutoff = param(7, "Filter Cutoff");
        cutoff.min = 20.0;
        cutoff.max = 20020.0;
        let m = resolve(&[], &[cutoff]);
        let c = m.find(" filter cutoff ").expect("any case, trimmed");
        assert_eq!(c.id, 7);
        assert!((c.normalized(10020.0) - 0.5).abs() < 1e-6);
        assert!((c.plain(0.5) - 10020.0).abs() < 1e-6);
        assert_eq!(c.normalized(1e9), 1.0, "clamped");
        assert!(m.find("Resonance").is_none());
    }

    #[test]
    fn knob_labels_keep_the_words_that_say_what_the_control_does() {
        assert_eq!(knob_label("Cutoff", 9), "CUTOFF");
        assert_eq!(knob_label("A Filter 1 Cutoff", 9), "1 CUTOFF");
        assert_eq!(knob_label("A Osc 1 Volume", 9), "OSC 1 VOL");
        assert_eq!(knob_label("M1: Cutoff", 9), "CUTOFF", "the macro index is not the name");
        assert_eq!(knob_label("M2: Resonance", 9), "RESO");
        assert_eq!(knob_label("M5: Vibrato Rate", 9), "RATE");
        assert_eq!(knob_label("Macro 1", 9), "MACRO 1");
        assert_eq!(knob_label("Resonance", 9), "RESO");
        assert_eq!(knob_label("Polyphony Limit Mode", 9), "MODE");
        assert_eq!(knob_label("Supercalifragilistic", 9), "SUPERCALI");
        assert_eq!(knob_label("  ", 9), "");
    }
}
