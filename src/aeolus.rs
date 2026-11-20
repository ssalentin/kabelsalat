//! Aeolus (Archie3d/aeolus_plugin), a pipe organ. Its sounds are
//! registrations: which stops of which division are drawn. The plugin has no
//! presets, keeps its stops out of its parameters and starts with every stop
//! off, so a fresh instance is silent. Its CLAP state is JSON
//! (`Engine::getPersistentState`): kabelsalat writes a whole state with one
//! registration drawn, for a fresh voice ([`initial_state`]) and for
//! `load_sound`.

use serde_json::{json, Value};

pub const AEOLUS_ID: &str = "com.ArthurBenilov.Aeolus";

/// A division of the organ the plugin builds when the user has no
/// `Documents/Aeolus/organ_config.json` (its `Resources/configs/default_organ.json`).
pub struct Division {
    pub name: &'static str,
    /// Stop names exactly as the state names them (a line break before the
    /// footage, as on the stop button).
    pub stops: &'static [&'static str],
    /// Divisions it can be coupled to.
    pub links: &'static [&'static str],
}

/// The divisions in the plugin's order: its state must list all of them, or
/// it ignores the whole state.
pub static DIVISIONS: [Division; 4] = [
    Division {
        name: "Manual III",
        stops: &[
            "Open Diapason\n8", "Stopt Diapason\n8", "Claribel\n8", "Suabile\n8", "Celeste\n8", "Violin\n8",
            "Principal\n4", "Flute Dolce\n4", "Fifteenth\n2", "Mixture\nV", "Bassoon\n16", "Cornopean\n8",
            "Oboe\n8", "Vox Humana\n8", "Trumpet\n4",
        ],
        links: &[],
    },
    Division {
        name: "Manual II",
        stops: &[
            "Open Diapason\n8", "Quintadena\n8", "Gemshorn\n8", "Chimney Flute\n8", "Principal\n4", "Rohr Flute\n4",
            "Nazard\n2 2/3", "Fifteenth\n2", "Spitz Flute\n2", "Tierce\n1 3/5", "Larigot\n1 1/3", "Sifflet\n1",
            "Cymbel\nVII", "Cromorne\n8",
        ],
        links: &["Manual III"],
    },
    Division {
        name: "Manual I",
        stops: &[
            "Double Open Diapason\n16", "Open Diapason\n8", "Harmonic\nFlute 8", "Principal\n4", "Flute\n4",
            "Twelfth\n2 2/3", "Fifteenth\n2", "Sesquialtera\nII", "Mixture\nIII", "Fourniture\nIV", "Cornet\nV",
            "Trumpet\n8", "Trumpet\n4",
        ],
        links: &["Manual II", "Manual III"],
    },
    Division {
        name: "Pedal",
        stops: &[
            "Open Diapason\n16", "Bourdon\n16", "Principal\n8", "Violoncell\n8", "Flute\n8", "Twelfth\n5 1/3",
            "Fifteenth\n4", "Mixture\nIV", "Bombarde\n32", "Trombone\n16", "Bassoon\n16", "Trumpet\n8",
        ],
        links: &["Manual I", "Manual II", "Manual III"],
    },
];

/// One registration: the stops drawn, by division name.
#[derive(Debug, PartialEq)]
pub struct Registration {
    pub category: &'static str,
    pub name: &'static str,
    pub stops: &'static [(&'static str, &'static str)],
    /// The tremulant of every division that has drawn stops.
    pub tremulant: bool,
}

/// Every division listens on every channel, so one keyboard plays all of
/// them at once: a registration draws stops in one manual (plus the pedal
/// for the full organ) to sound like one.
pub static REGISTRATIONS: &[Registration] = &[
    Registration { category: "Principals", name: "Diapason 8", tremulant: false, stops: &[
        ("Manual I", "Open Diapason\n8"),
    ] },
    Registration { category: "Principals", name: "Principal Chorus", tremulant: false, stops: &[
        ("Manual I", "Open Diapason\n8"), ("Manual I", "Principal\n4"), ("Manual I", "Fifteenth\n2"),
    ] },
    Registration { category: "Principals", name: "Plenum", tremulant: false, stops: &[
        ("Manual I", "Double Open Diapason\n16"), ("Manual I", "Open Diapason\n8"), ("Manual I", "Principal\n4"),
        ("Manual I", "Fifteenth\n2"), ("Manual I", "Mixture\nIII"), ("Manual I", "Fourniture\nIV"),
    ] },
    Registration { category: "Flutes", name: "Soft Flute 8", tremulant: false, stops: &[
        ("Manual III", "Stopt Diapason\n8"),
    ] },
    Registration { category: "Flutes", name: "Flutes 8 4", tremulant: false, stops: &[
        ("Manual II", "Chimney Flute\n8"), ("Manual II", "Rohr Flute\n4"),
    ] },
    Registration { category: "Flutes", name: "Cornet", tremulant: false, stops: &[
        ("Manual II", "Chimney Flute\n8"), ("Manual II", "Rohr Flute\n4"), ("Manual II", "Nazard\n2 2/3"),
        ("Manual II", "Spitz Flute\n2"), ("Manual II", "Tierce\n1 3/5"),
    ] },
    Registration { category: "Strings", name: "Celeste", tremulant: false, stops: &[
        ("Manual III", "Violin\n8"), ("Manual III", "Celeste\n8"),
    ] },
    Registration { category: "Reeds", name: "Oboe", tremulant: false, stops: &[
        ("Manual III", "Stopt Diapason\n8"), ("Manual III", "Oboe\n8"),
    ] },
    Registration { category: "Reeds", name: "Vox Humana", tremulant: true, stops: &[
        ("Manual III", "Stopt Diapason\n8"), ("Manual III", "Vox Humana\n8"),
    ] },
    Registration { category: "Reeds", name: "Cromorne", tremulant: false, stops: &[
        ("Manual II", "Chimney Flute\n8"), ("Manual II", "Cromorne\n8"),
    ] },
    Registration { category: "Reeds", name: "Trumpet", tremulant: false, stops: &[
        ("Manual I", "Open Diapason\n8"), ("Manual I", "Trumpet\n8"),
    ] },
    Registration { category: "Full", name: "Full Organ", tremulant: false, stops: &[
        ("Manual I", "Double Open Diapason\n16"), ("Manual I", "Open Diapason\n8"), ("Manual I", "Principal\n4"),
        ("Manual I", "Fifteenth\n2"), ("Manual I", "Mixture\nIII"), ("Manual I", "Fourniture\nIV"),
        ("Manual I", "Trumpet\n8"), ("Manual I", "Trumpet\n4"),
        ("Pedal", "Open Diapason\n16"), ("Pedal", "Bourdon\n16"), ("Pedal", "Trombone\n16"),
    ] },
];

/// The registration a fresh Aeolus starts with.
pub fn default_registration() -> &'static Registration {
    &REGISTRATIONS[1]
}

/// The plugin's whole state with `reg` drawn: every stop of every division
/// listed (stops left out keep what they were), couplers off, all MIDI
/// channels on (a missing mask means none). Plugin parameters (volume,
/// reverb) are left out, so they keep their values.
pub fn state(reg: &Registration) -> Vec<u8> {
    let all_channels = (1 << 16) - 1;
    let divisions: Vec<Value> = DIVISIONS.iter().map(|d| {
        let drawn = |stop: &str| reg.stops.iter().any(|&(div, s)| div == d.name && s == stop);
        let used = reg.stops.iter().any(|&(div, _)| div == d.name);
        json!({
            "midi_channels_mask": all_channels,
            "tremulant_enabled": reg.tremulant && used,
            "stops": d.stops.iter().map(|s| json!({ "name": s, "enabled": drawn(s) })).collect::<Vec<_>>(),
            "links": d.links.iter().map(|l| json!({ "division": l, "enabled": false })).collect::<Vec<_>>(),
        })
    }).collect();
    json!({
        "midi_ctrl_channels_mask": all_channels,
        "midi_swell_channels_mask": all_channels,
        "ir": 0,
        "divisions": divisions,
    }).to_string().into_bytes()
}

/// The state a freshly instantiated plugin gets so it plays at once, for the
/// plugins that start silent.
pub fn initial_state(plugin_id: &str) -> Option<Vec<u8>> {
    (plugin_id == AEOLUS_ID).then(|| state(default_registration()))
}

/// Short name of a division for a face tab ("III", "II", "I", "P").
pub fn division_tab(division: usize) -> &'static str {
    ["III", "II", "I", "P"].get(division).copied().unwrap_or("?")
}

/// Which stops a state draws: one bit per stop of [`DIVISIONS`], per
/// division. `None` when the state is not the default organ (a user's own
/// `organ_config.json` has other divisions or stops), so a face never shows
/// stops the plugin does not have.
pub fn drawn(state: &[u8]) -> Option<[u32; 4]> {
    let v: Value = serde_json::from_slice(state).ok()?;
    let divisions = v.get("divisions")?.as_array()?;
    if divisions.len() != DIVISIONS.len() { return None; }
    let mut out = [0u32; 4];
    for ((d, div), bits) in divisions.iter().zip(&DIVISIONS).zip(&mut out) {
        let stops = d.get("stops")?.as_array()?;
        if stops.len() != div.stops.len() { return None; }
        for s in stops {
            let i = div.stops.iter().position(|n| s.get("name").and_then(Value::as_str) == Some(*n))?;
            if s.get("enabled").and_then(Value::as_bool) == Some(true) { *bits |= 1 << i; }
        }
    }
    Some(out)
}

/// `state` with one stop drawn or pushed in, everything else (couplers,
/// tremulant, channels, sequencer, volume, reverb) as it was. `None` when the
/// state does not have that stop.
pub fn with_stop(state: &[u8], division: usize, stop: usize, on: bool) -> Option<Vec<u8>> {
    let name = *DIVISIONS.get(division)?.stops.get(stop)?;
    let mut v: Value = serde_json::from_slice(state).ok()?;
    let s = v.get_mut("divisions")?.get_mut(division)?.get_mut("stops")?.as_array_mut()?
        .iter_mut().find(|s| s.get("name").and_then(Value::as_str) == Some(name))?;
    s["enabled"] = Value::Bool(on);
    Some(v.to_string().into_bytes())
}

/// Whether `division`'s tremulant is enabled in `state`.
pub fn tremulant(state: &[u8], division: usize) -> Option<bool> {
    let v: Value = serde_json::from_slice(state).ok()?;
    v.get("divisions")?.get(division)?.get("tremulant_enabled")?.as_bool()
}

/// `state` with `division`'s tremulant set to `on`.
pub fn with_tremulant(state: &[u8], division: usize, on: bool) -> Option<Vec<u8>> {
    if division >= DIVISIONS.len() { return None; }
    let mut v: Value = serde_json::from_slice(state).ok()?;
    let div = v.get_mut("divisions")?.get_mut(division)?;
    div["tremulant_enabled"] = Value::Bool(on);
    Some(v.to_string().into_bytes())
}

/// Bitmask of enabled couplers per division (bit `j` = `DIVISIONS[d].links[j]`).
pub fn couplers(state: &[u8]) -> Option<[u8; 4]> {
    let v: Value = serde_json::from_slice(state).ok()?;
    let divisions = v.get("divisions")?.as_array()?;
    if divisions.len() != DIVISIONS.len() { return None; }
    let mut out = [0u8; 4];
    for ((d, div), bits) in divisions.iter().zip(&DIVISIONS).zip(&mut out) {
        let Some(links) = d.get("links").and_then(Value::as_array) else { continue };
        for l in links {
            let Some(name) = l.get("division").and_then(Value::as_str) else { continue };
            if let Some(i) = div.links.iter().position(|n| *n == name) {
                if l.get("enabled").and_then(Value::as_bool) == Some(true) {
                    *bits |= 1 << i;
                }
            }
        }
    }
    Some(out)
}

/// `state` with one coupler (`DIVISIONS[division].links[link]`) toggled.
pub fn with_coupler(state: &[u8], division: usize, link: usize, on: bool) -> Option<Vec<u8>> {
    let target = *DIVISIONS.get(division)?.links.get(link)?;
    let mut v: Value = serde_json::from_slice(state).ok()?;
    let links = v.get_mut("divisions")?.get_mut(division)?.get_mut("links")?.as_array_mut()?;
    let entry = links.iter_mut().find(|l| l.get("division").and_then(Value::as_str) == Some(target))?;
    entry["enabled"] = Value::Bool(on);
    Some(v.to_string().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registration_draws_stops_the_organ_has() {
        for r in REGISTRATIONS {
            assert!(!r.stops.is_empty(), "{} draws a stop", r.name);
            for (div, stop) in r.stops {
                let d = DIVISIONS.iter().find(|d| d.name == *div).unwrap_or_else(|| panic!("{}: no division {div}", r.name));
                assert!(d.stops.contains(stop), "{}: {div} has no stop {stop:?}", r.name);
            }
        }
    }

    #[test]
    fn a_state_lists_every_stop_and_draws_only_the_registration() {
        let reg = REGISTRATIONS.iter().find(|r| r.name == "Vox Humana").unwrap();
        let v: Value = serde_json::from_slice(&state(reg)).unwrap();
        let divisions = v["divisions"].as_array().unwrap();
        assert_eq!(divisions.len(), DIVISIONS.len());
        let drawn: Vec<(usize, &str)> = divisions.iter().enumerate()
            .flat_map(|(i, d)| d["stops"].as_array().unwrap().iter()
                .filter(|s| s["enabled"] == true)
                .map(move |s| (i, s["name"].as_str().unwrap())))
            .collect();
        assert_eq!(drawn, [(0, "Stopt Diapason\n8"), (0, "Vox Humana\n8")]);
        for (d, div) in divisions.iter().zip(&DIVISIONS) {
            assert_eq!(d["stops"].as_array().unwrap().len(), div.stops.len(), "{} lists every stop", div.name);
            assert_eq!(d["midi_channels_mask"], 65535, "{} hears every channel", div.name);
        }
        assert_eq!(divisions[0]["tremulant_enabled"], true, "the swell box tremulant is on for Vox Humana");
        assert_eq!(divisions[2]["tremulant_enabled"], false);
        assert!(v.get("parameters").is_none(), "volume and reverb stay as they are");
    }

    #[test]
    fn a_stop_toggles_alone_and_reads_back() {
        let start = state(default_registration());
        assert_eq!(drawn(&start), Some([0, 0, 0b100_1010, 0]), "Principal Chorus: Manual I 8, 4 and 2");
        // Draw the Pedal Bourdon 16, then push Manual I's Principal 4 in.
        let s = with_stop(&start, 3, 1, true).unwrap();
        let s = with_stop(&s, 2, 3, false).unwrap();
        assert_eq!(drawn(&s), Some([0, 0, 0b100_0010, 0b10]));
        // Everything else is kept, also keys kabelsalat never writes.
        let mut v: Value = serde_json::from_slice(&start).unwrap();
        v["sequencer"] = json!({ "steps": [1, 2] });
        let kept: Value = serde_json::from_slice(&with_stop(v.to_string().as_bytes(), 0, 0, true).unwrap()).unwrap();
        assert_eq!(kept["sequencer"], json!({ "steps": [1, 2] }));
        assert_eq!(kept["divisions"][1]["links"], v["divisions"][1]["links"]);
        assert_eq!(with_stop(&start, 0, 99, true), None, "no such stop");
    }

    #[test]
    fn another_organ_shows_no_stops() {
        let mut v: Value = serde_json::from_slice(&state(default_registration())).unwrap();
        v["divisions"].as_array_mut().unwrap().pop();
        assert_eq!(drawn(v.to_string().as_bytes()), None, "three divisions are another organ");
        let mut v: Value = serde_json::from_slice(&state(default_registration())).unwrap();
        v["divisions"][0]["stops"][0]["name"] = json!("Gamba\n8");
        assert_eq!(drawn(v.to_string().as_bytes()), None, "a stop the default organ lacks");
    }

    #[test]
    fn only_aeolus_starts_with_a_registration() {
        assert!(initial_state(AEOLUS_ID).is_some());
        assert!(initial_state("org.surge-synth-team.surge-xt").is_none());
    }

    #[test]
    fn tremulant_and_couplers_toggle_and_read_back() {
        let start = state(default_registration());
        assert_eq!(tremulant(&start, 2), Some(false));
        let s = with_tremulant(&start, 2, true).unwrap();
        assert_eq!(tremulant(&s, 2), Some(true));
        assert_eq!(tremulant(&s, 0), Some(false));
        assert_eq!(couplers(&s), Some([0, 0, 0, 0]));
        // Manual I (div 2) links[0] = Manual II, links[1] = Manual III.
        let s = with_coupler(&s, 2, 0, true).unwrap();
        let s = with_coupler(&s, 3, 2, true).unwrap();
        assert_eq!(couplers(&s), Some([0, 0, 0b01, 0b100]));
        assert_eq!(with_coupler(&s, 0, 0, true), None, "Manual III has no couplers");
    }
}
