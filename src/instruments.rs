//! Instrument profiles: what each MIDI controller of a hardware instrument
//! means, so a whole instrument maps onto the rack at once instead of one
//! MIDI learn per knob.
//!
//! A profile is a CSV file in the format of the MIDI Guide dataset
//! (<https://github.com/pencilresearch/midi>, one file per device: section,
//! parameter name, CC number, …) with one more column, `role`, naming what a
//! control does in a vocabulary every target shares (`drawbar.upper.3`,
//! `rotary.speed`, …). A MIDI Guide file without that column still loads; it
//! names its controllers but binds nothing. Built-in profiles live in
//! `instruments/` and are compiled in; more are read from
//! `~/.config/kabelsalat/instruments/*.csv`.
//!
//! Which rack parameter a role drives is the target's business
//! ([`target`]); the profile never names a parameter.

use crate::control::{NodeKind, ParamId};
use crate::matrix::{CcScale, CcTarget};
use crate::organ;

const BUILTIN: &[(&str, &str)] = &[("nord-electro-2", include_str!("../instruments/nord-electro-2.csv"))];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Manual {
    Upper,
    Lower,
}

/// What a hardware control means, independent of instrument and target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Drawbar `bar` (0 = 16', left to right) of a manual, 9 positions.
    Drawbar { manual: Manual, bar: usize },
    /// Rotary speaker slow/fast switch.
    RotarySpeed,
    /// Percussion on/off.
    Percussion,
    /// Overdrive amount.
    Drive,
    /// Reverb amount.
    Reverb,
}

impl Role {
    pub fn parse(s: &str) -> Option<Role> {
        let s = s.trim().to_ascii_lowercase();
        let drawbar = |manual, n: &str| {
            let n: usize = n.parse().ok()?;
            (1..=organ::DRAWBARS).contains(&n).then_some(Role::Drawbar { manual, bar: n - 1 })
        };
        match s.as_str() {
            "rotary.speed" => Some(Role::RotarySpeed),
            "percussion" => Some(Role::Percussion),
            "drive" => Some(Role::Drive),
            "reverb" => Some(Role::Reverb),
            _ => {
                if let Some(n) = s.strip_prefix("drawbar.upper.") {
                    drawbar(Manual::Upper, n)
                } else if let Some(n) = s.strip_prefix("drawbar.lower.") {
                    drawbar(Manual::Lower, n)
                } else {
                    None
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Control {
    pub cc: u8,
    pub section: String,
    pub name: String,
    pub role: Option<Role>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    /// File stem: what the config stores per device.
    pub id: String,
    pub manufacturer: String,
    pub device: String,
    pub controls: Vec<Control>,
}

/// One CC binding a profile gives a rack node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binding {
    pub cc: u8,
    pub param: ParamId,
    pub target: CcTarget,
    pub scale: CcScale,
}

impl Profile {
    pub fn title(&self) -> String {
        format!("{} {}", self.manufacturer, self.device)
    }

    /// Does an ALSA port name belong to this instrument? Both the maker and
    /// the model must appear ("Nord Stage 3 MIDI 1"); an interface between
    /// instrument and computer (a 5-pin DIN cable into a USB adapter) hides
    /// the instrument, and the user picks the profile instead.
    pub fn matches_port(&self, port: &str) -> bool {
        let port = port.to_lowercase();
        !self.device.is_empty()
            && port.contains(&self.manufacturer.to_lowercase())
            && port.contains(&self.device.to_lowercase())
    }

    /// Every binding this profile gives a node of `kind`, one per CC.
    pub fn bindings(&self, kind: &NodeKind) -> Vec<Binding> {
        self.controls
            .iter()
            .filter_map(|c| {
                let (param, target, scale) = target(kind, c.role?)?;
                Some(Binding { cc: c.cc, param, target, scale })
            })
            .collect()
    }

    pub fn control(&self, cc: u8) -> Option<&Control> {
        self.controls.iter().find(|c| c.cc == cc)
    }
}

/// The parameter a role drives on a node of `kind`, and how the instrument's
/// control lands on it. None = this node has nothing for the role.
fn target(kind: &NodeKind, role: Role) -> Option<(ParamId, CcTarget, CcScale)> {
    if !organ::is_organ(kind) {
        return None;
    }
    let knob = |param, scale| Some((param, CcTarget::Knob, scale));
    match role {
        // The organ face plays the upper manual; it has no lower drawbars.
        Role::Drawbar { manual: Manual::Upper, bar } => knob(organ::drawbar_param(bar), CcScale::stepped(9, 0, 127)),
        Role::Drawbar { manual: Manual::Lower, .. } => None,
        // LESLIE steps STOP / SLOW / FAST: the switch lands on SLOW or FAST.
        Role::RotarySpeed => knob(organ::P_LESLIE, CcScale::stepped(2, 64, 127)),
        // PERC steps OFF / ON / SOFT: the switch lands on OFF or ON.
        Role::Percussion => knob(organ::P_PERC, CcScale::stepped(2, 0, 64)),
        Role::Drive => knob(organ::P_DRIVE, CcScale::IDENTITY),
        Role::Reverb => knob(organ::P_REVERB, CcScale::IDENTITY),
    }
}

/// Parse one profile. Unknown roles are an error, so a typo in a user file
/// is reported instead of silently binding nothing.
pub fn parse(id: &str, text: &str) -> Result<Profile, String> {
    let mut rows = text.lines().filter(|l| !l.trim().is_empty()).map(split_csv_line);
    let header = rows.next().ok_or("empty file")?;
    let col = |name: &str| header.iter().position(|h| h.trim().eq_ignore_ascii_case(name));
    let (Some(maker), Some(device), Some(cc_col), Some(name_col)) =
        (col("manufacturer"), col("device"), col("cc_msb"), col("parameter_name"))
    else {
        return Err("needs the MIDI Guide columns manufacturer, device, parameter_name, cc_msb".into());
    };
    let (section, role_col) = (col("section"), col("role"));
    let mut p = Profile { id: id.to_string(), manufacturer: String::new(), device: String::new(), controls: Vec::new() };
    for (line, row) in rows.enumerate() {
        let field = |i: Option<usize>| i.and_then(|i| row.get(i)).map(|s| s.trim()).unwrap_or("");
        if p.device.is_empty() {
            p.manufacturer = field(Some(maker)).to_string();
            p.device = field(Some(device)).to_string();
        }
        let Ok(cc) = field(Some(cc_col)).parse::<u8>() else { continue };
        if cc > 127 {
            return Err(format!("row {}: CC {cc} is not a controller", line + 2));
        }
        let role = match field(role_col) {
            "" => None,
            r => Some(Role::parse(r).ok_or_else(|| format!("row {}: unknown role '{r}'", line + 2))?),
        };
        if p.control(cc).is_some() {
            return Err(format!("row {}: CC {cc} is listed twice", line + 2));
        }
        p.controls.push(Control { cc, section: field(section).to_string(), name: field(Some(name_col)).to_string(), role });
    }
    if p.device.is_empty() {
        return Err("no device rows".into());
    }
    Ok(p)
}

/// One CSV record: commas split fields, double quotes enclose a field with
/// commas, and `""` inside quotes is a quote.
fn split_csv_line(line: &str) -> Vec<String> {
    let mut fields = vec![String::new()];
    let mut quoted = false;
    let mut chars = line.trim_end_matches('\r').chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                chars.next();
                fields.last_mut().unwrap().push('"');
            }
            ('"', _) => quoted = !quoted,
            (',', false) => fields.push(String::new()),
            _ => fields.last_mut().unwrap().push(c),
        }
    }
    fields
}

pub fn builtin() -> Vec<Profile> {
    BUILTIN.iter().map(|(id, text)| parse(id, text).expect("built-in profile parses")).collect()
}

fn user_dir() -> std::path::PathBuf {
    crate::assistant::config_path().with_file_name("instruments")
}

/// Built-in profiles, then the user's (a user file with a built-in's id
/// replaces it), with an error line per file that did not load. Tests read
/// only the built-ins (rule 37).
pub fn load_all() -> (Vec<Profile>, Vec<String>) {
    let mut profiles = builtin();
    let mut errors = Vec::new();
    if cfg!(test) {
        return (profiles, errors);
    }
    let Ok(dir) = std::fs::read_dir(user_dir()) else { return (profiles, errors) };
    let mut files: Vec<_> = dir.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "csv")).collect();
    files.sort();
    for path in files {
        let id = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        match std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|t| parse(&id, &t)) {
            Ok(p) => {
                profiles.retain(|q| q.id != p.id);
                profiles.push(p);
            }
            Err(e) => errors.push(format!("{}: {e}", path.display())),
        }
    }
    (profiles, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn electro() -> Profile {
        builtin().into_iter().find(|p| p.id == "nord-electro-2").unwrap()
    }

    fn organ_kind() -> NodeKind {
        NodeKind::Lv2 { uri: crate::control::Lv2Uri::new(organ::B_SYNTH) }
    }

    #[test]
    fn the_nord_electro_2_chart_is_the_manuals() {
        let p = electro();
        assert_eq!(p.title(), "Nord Electro 2");
        assert_eq!(p.controls.len(), 59, "every controller on manual pages 45-47 plus the two pedals");
        assert_eq!(p.control(18).unwrap().name, "8' upper drawbar");
        assert_eq!(p.control(12).unwrap().name, "Piano type", "a quoted note with a comma stays one field");
        assert_eq!(p.control(82).unwrap().role, Some(Role::RotarySpeed));
        assert_eq!(p.control(70).unwrap().role, Some(Role::Drawbar { manual: Manual::Lower, bar: 0 }));
    }

    #[test]
    fn the_electro_drives_every_organ_face_control_it_has() {
        let b = electro().bindings(&organ_kind());
        let param_of = |cc| b.iter().find(|x| x.cc == cc).map(|x| x.param);
        for bar in 0..organ::DRAWBARS {
            assert_eq!(param_of(16 + bar as u8), Some(organ::drawbar_param(bar)), "upper drawbar {bar}");
            assert_eq!(param_of(70 + bar as u8), None, "the face has no lower manual");
        }
        assert_eq!(param_of(82), Some(organ::P_LESLIE));
        assert_eq!(param_of(87), Some(organ::P_PERC));
        assert_eq!(param_of(111), Some(organ::P_DRIVE));
        assert_eq!(b.len(), 12);
        assert!(b.iter().all(|x| x.target == CcTarget::Knob));
        assert!(electro().bindings(&NodeKind::Clap { plugin_id: crate::control::ClapId::new("org.surge-synth-team.surge-xt") }).is_empty());
    }

    /// The values a control sends land on the organ's own steps, wherever in
    /// its section the instrument puts them.
    #[test]
    fn switches_and_drawbars_land_on_the_organs_steps() {
        let b = electro().bindings(&organ_kind());
        let at = |cc: u8, v: u8| b.iter().find(|x| x.cc == cc).unwrap().scale.norm(v) as f32;
        for pos in 0..=8u32 {
            let section = ((pos * 128).div_ceil(9), ((pos + 1) * 128).div_ceil(9) - 1);
            for v in [section.0, section.1, pos * 127 / 8] {
                assert_eq!(organ::drawbar_position(at(16, v as u8)), pos as u8, "drawbar value {v}");
            }
        }
        assert_eq!(organ::step_name(organ::P_LESLIE, at(82, 0)), Some("SLOW"));
        assert_eq!(organ::step_name(organ::P_LESLIE, at(82, 63)), Some("SLOW"));
        assert_eq!(organ::step_name(organ::P_LESLIE, at(82, 64)), Some("FAST"));
        assert_eq!(organ::step_name(organ::P_PERC, at(87, 0)), Some("OFF"));
        assert_eq!(organ::step_name(organ::P_PERC, at(87, 127)), Some("ON"));
        assert_eq!(at(111, 127), 1.0);
    }

    #[test]
    fn ports_match_by_maker_and_model() {
        let stage = Profile { id: "s3".into(), manufacturer: "Nord".into(), device: "Stage 3".into(), controls: vec![] };
        assert!(stage.matches_port("Nord Stage 3:Nord Stage 3 MIDI 1 24:0"));
        assert!(!stage.matches_port("Nord Electro 6D:Nord Electro 6D MIDI 1 24:0"));
        assert!(!electro().matches_port("ESI MIDIMATE eX:ESI MIDIMATE eX MIDI 1 28:0"));
    }

    #[test]
    fn a_midi_guide_file_without_roles_loads_and_bad_roles_are_reported() {
        let guide = "manufacturer,device,section,parameter_name,parameter_description,cc_msb\n\
                     Nord,Stage 3,Organ: Drawbars,Drawbar 1,,16\n";
        let p = parse("stage-3", guide).unwrap();
        assert_eq!((p.device.as_str(), p.controls.len(), p.controls[0].role), ("Stage 3", 1, None));
        let typo = "manufacturer,device,parameter_name,cc_msb,role\nNord,X,Bar,16,drawbar.upper.10\n";
        assert!(parse("x", typo).unwrap_err().contains("unknown role"));
        let twice = "manufacturer,device,parameter_name,cc_msb\nNord,X,A,16\nNord,X,B,16\n";
        assert!(parse("x", twice).unwrap_err().contains("twice"));
    }
}
