//! setBfree, the tonewheel organ: its face controls and the MIDI CCs they
//! send. setBfree has no LV2 control ports; everything a player touches
//! (drawbars, Leslie, percussion, overdrive, reverb) is a MIDI controller,
//! with the defaults compiled into the plugin (`midiPrimeControllerMapping`
//! in setBfree's `src/midi.c`; upper manual = MIDI channel 1).
//!
//! Each control is an ordinary rack-knob parameter (`P_BASE + n`), so the
//! session, undo, MIDI learn and the agent's `set_param` treat it like any
//! knob; the engine turns a parameter value into CCs with [`ccs`].

use std::sync::OnceLock;

use crate::control::{NodeKind, ParamId};

/// setBfree's LV2 URI.
pub const B_SYNTH: &str = "http://gareus.org/oss/lv2/b_synth";

/// First organ parameter id. setBfree has no control ports, so no LV2 control
/// index collides; the range is far above any port count.
pub const P_BASE: ParamId = 0x0B00;

/// Number of upper-manual drawbars.
pub const DRAWBARS: usize = 9;

/// Drawbar footages, left (lowest) to right, as printed on the bars.
pub const FOOTAGES: [&str; DRAWBARS] = ["16'", "5⅓'", "8'", "4'", "2⅔'", "2'", "1⅗'", "1⅓'", "1'"];

/// The registration a fresh organ starts with ("Standard B": 88 8000 000).
pub const DEFAULT_DRAWBARS: [u8; DRAWBARS] = [8, 8, 8, 0, 0, 0, 0, 0, 0];

/// Parameter of drawbar `i` (0 = 16').
pub const fn drawbar_param(i: usize) -> ParamId {
    P_BASE + i as ParamId
}

/// Number of organ parameters (`P_BASE..P_BASE + PARAMS`).
pub const PARAMS: usize = 13;

/// Index of an organ parameter, `None` outside the range.
pub fn index(param: ParamId) -> Option<usize> {
    param.checked_sub(P_BASE).map(|i| i as usize).filter(|i| *i < PARAMS)
}

pub const P_LESLIE: ParamId = P_BASE + 9;
pub const P_PERC: ParamId = P_BASE + 10;
pub const P_DRIVE: ParamId = P_BASE + 11;
pub const P_REVERB: ParamId = P_BASE + 12;

/// Leslie speed states sent on CC 1 (`rotary.speed-preset`: the value range
/// is split into stop / slow / fast / stop); the knob steps slow → fast.
const LESLIE: [u8; 3] = [16, 48, 80];
/// Percussion on CC 80 (`percussion.enable`: off / normal / soft / off).
const PERC: [u8; 3] = [0, 48, 80];

/// Is this node setBfree?
pub fn is_organ(kind: &NodeKind) -> bool {
    matches!(kind, NodeKind::Lv2 { uri } if uri.as_str() == B_SYNTH)
}

/// Knob value (0..=1) of a drawbar position (0..=8).
pub fn drawbar_value(position: u8) -> f32 {
    position.min(8) as f32 / 8.0
}

/// Drawbar position (0..=8) of a knob value.
pub fn drawbar_position(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 8.0).round() as u8
}

/// Index of one of `n` steps a stepped knob value selects.
pub fn step(value: f32, n: usize) -> usize {
    ((value.clamp(0.0, 1.0) * (n - 1) as f32).round() as usize).min(n - 1)
}

/// The CC messages (controller, value) one organ parameter at knob value `v`
/// sends; `None` for a parameter that is not an organ control. Allocation-free.
pub fn ccs(param: ParamId, v: f32) -> Option<([(u8, u8); 2], usize)> {
    let v = v.clamp(0.0, 1.0);
    let one = |cc: u8, val: u8| Some(([(cc, val), (0, 0)], 1));
    match param {
        p if p >= P_BASE && p < P_BASE + DRAWBARS as ParamId => {
            // setBfree inverts drawbar CCs (0 = pulled out, like a fader
            // pulled down) and quantizes to the nine positions.
            let pos = drawbar_position(v) as u32;
            one(70 + (p - P_BASE) as u8, (127 - pos * 127 / 8) as u8)
        }
        P_LESLIE => one(1, LESLIE[step(v, LESLIE.len())]),
        P_PERC => one(80, PERC[step(v, PERC.len())]),
        // DRIVE 0 = overdrive off (CC 65); above it, on with that character.
        P_DRIVE if v <= 0.0 => one(65, 0),
        P_DRIVE => Some(([(65, 127), (93, (v * 127.0).round() as u8)], 2)),
        P_REVERB => one(91, (v * 127.0).round() as u8),
        _ => None,
    }
}

/// Text a stepped organ knob shows for its value (LESLIE / PERC).
pub fn step_name(param: ParamId, v: f32) -> Option<&'static str> {
    match param {
        P_LESLIE => Some(["STOP", "SLOW", "FAST"][step(v, 3)]),
        P_PERC => Some(["OFF", "ON", "SOFT"][step(v, 3)]),
        _ => None,
    }
}

/// The upper drawbars a MIDI programme (0-based, as sent) sets, from the
/// programme file setBfree loads at start (display number = programme + 1).
/// `None` when the programme sets no drawbars or the file is missing.
pub fn program_drawbars(program: u8) -> Option<[u8; DRAWBARS]> {
    static TABLE: OnceLock<Vec<(u8, [u8; DRAWBARS])>> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        std::fs::read_to_string("/usr/share/setBfree/pgm/default.pgm")
            .map(|t| parse_pgm(&t))
            .unwrap_or_default()
    });
    table.iter().find(|(n, _)| *n as u16 == program as u16 + 1).map(|(_, d)| *d)
}

/// `(display number, upper drawbars)` of every programme in a setBfree
/// programme file that sets `drawbars="…"`. Comments start with `#`.
pub fn parse_pgm(text: &str) -> Vec<(u8, [u8; DRAWBARS])> {
    let mut out = Vec::new();
    let mut current: Option<u8> = None;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("");
        let trimmed = line.trim_start();
        // A programme starts with its number: `5   { name="Standard B", …`.
        let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() && trimmed[digits.len()..].trim_start().starts_with('{') {
            current = digits.parse().ok();
        }
        let Some(n) = current else { continue };
        if let Some(bars) = upper_drawbars(line) {
            out.push((n, bars));
        }
        if line.contains('}') { current = None; }
    }
    out
}

/// The upper-manual registration on one programme line: the value of
/// `drawbars` or `drawbarsupper` (any case, quoted or not), read the way
/// setBfree's `parseDrawbarRegistration` reads it: the first nine digits
/// 0..=8, skipping blanks, `-` and `_`; anything after the ninth is ignored.
fn upper_drawbars(line: &str) -> Option<[u8; DRAWBARS]> {
    let lower = line.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find("drawbars") {
        let at = from + i + "drawbars".len();
        from = at;
        let rest = lower[at..].strip_prefix("upper").unwrap_or(&lower[at..]);
        let Some(value) = rest.trim_start().strip_prefix('=') else { continue };
        let value = value.trim_start().trim_start_matches('"');
        let mut bars = [0u8; DRAWBARS];
        let mut n = 0;
        for c in value.chars() {
            if n == DRAWBARS { break; }
            match c {
                '0'..='8' => { bars[n] = c as u8 - b'0'; n += 1; }
                ' ' | '\t' | '-' | '_' => {}
                _ => break,
            }
        }
        if n == DRAWBARS { return Some(bars); }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drawbars_send_setbfrees_inverted_ccs() {
        // Fully out (8) = CC 0, fully in (0) = CC 127, on CC 70..78.
        assert_eq!(ccs(drawbar_param(0), 1.0), Some(([(70, 0), (0, 0)], 1)));
        assert_eq!(ccs(drawbar_param(8), 0.0), Some(([(78, 127), (0, 0)], 1)));
        // Every position lands on setBfree's own quantization (rint(v*8/127)).
        for pos in 0..=8u8 {
            let ([(_, cc), _], _) = ccs(drawbar_param(3), drawbar_value(pos)).unwrap();
            let back = ((127 - cc) as f64 * 8.0 / 127.0).round() as u8;
            assert_eq!(back, pos, "position {pos} sent as CC {cc}");
        }
    }

    #[test]
    fn switches_and_steps_send_their_controller_ranges() {
        assert_eq!(ccs(P_LESLIE, 0.5), Some(([(1, 48), (0, 0)], 1)), "slow");
        assert_eq!(ccs(P_LESLIE, 1.0), Some(([(1, 80), (0, 0)], 1)), "fast");
        assert_eq!(ccs(P_PERC, 0.0), Some(([(80, 0), (0, 0)], 1)), "off");
        assert_eq!(ccs(P_DRIVE, 0.0), Some(([(65, 0), (0, 0)], 1)), "overdrive off");
        assert_eq!(ccs(P_DRIVE, 0.5), Some(([(65, 127), (93, 64)], 2)), "on, with character");
        assert_eq!(ccs(P_REVERB, 1.0), Some(([(91, 127), (0, 0)], 1)));
        assert_eq!(ccs(P_BASE + 40, 1.0), None);
        assert_eq!(step_name(P_LESLIE, 0.0), Some("STOP"));
        assert_eq!(step_name(P_LESLIE, 1.0), Some("FAST"));
    }

    /// Lines pasted from the installed /usr/share/setBfree/pgm/default.pgm
    /// (rule 63: parsers are tested on the tool's real output).
    #[test]
    fn the_programme_file_gives_each_programme_its_drawbars() {
        let real = r#"# The default programme (this file) contains instrument controls and some
1 {name="Jazz 1 all",
   drawbars="888 0000 000",
   perc=on,
   percvol=soft,
}
2   { name="Fredriks tfn 1",         drawbars="75 8757 200" }
5   { name="Standard B",             drawbars="88 8000 000" }
"#;
        assert_eq!(upper_drawbars(r#"  drawbarslower="00 8800 000","#), None, "the lower manual is not the upper");
        assert_eq!(upper_drawbars(r#"  DrawbarsUpper = 88-8000-000"#), Some([8, 8, 8, 0, 0, 0, 0, 0, 0]));
        let t = parse_pgm(real);
        assert_eq!(t, vec![(1, [8, 8, 8, 0, 0, 0, 0, 0, 0]), (2, [7, 5, 8, 7, 5, 7, 2, 0, 0]), (5, [8, 8, 8, 0, 0, 0, 0, 0, 0])]);
    }
}
