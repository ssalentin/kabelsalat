//! Unsampled Piano (`fr.mgth.unsampled-piano`) CPU & room profiles.
//!
//! Unsampled Piano embeds a modal soundboard, sympathetic string resonance,
//! longitudinal phantom partials, cast-iron frame, rim, cross-polarisation,
//! and a concert-hall room FDN on a single audio thread (`nice-plug` lacks
//! CLAP `thread-pool`). Its plugin editor exposes presets and physics controls,
//! while kabelsalat's Sound library offers one-click CPU/room profiles that
//! mutate only the relevant fields inside its CLAP state (`nice-plug`
//! length-prefixed JSON) without disturbing tuning, hammer/damper tweaks,
//! MIDI learn mappings, or host parameter values.

use serde_json::{json, Value};

pub const PLUGIN_ID: &str = "fr.mgth.unsampled-piano";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileKind {
    Full,
    Balanced,
    Light,
    Dry,
    LightDry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Profile {
    pub category: &'static str,
    pub name: &'static str,
    pub summary: &'static str,
    pub preferred: bool,
    pub kind: ProfileKind,
}

pub static PROFILES: &[Profile] = &[
    Profile {
        category: "CPU",
        name: "Full",
        summary: "Full physics and built-in concert hall room (highest CPU)",
        preferred: true,
        kind: ProfileKind::Full,
    },
    Profile {
        category: "CPU",
        name: "Balanced",
        summary: "Sympathetic resonance <= 2 kHz, phantom partials <= C3, 2 close mics",
        preferred: false,
        kind: ProfileKind::Balanced,
    },
    Profile {
        category: "CPU",
        name: "Light",
        summary: "No sympathy, phantom partials, frame/rim or cross-polarisation; 2 close mics",
        preferred: false,
        kind: ProfileKind::Light,
    },
    Profile {
        category: "CPU",
        name: "Dry",
        summary: "Full physics with built-in room off (~1% idle; pair with kabelsalat Reverb)",
        preferred: false,
        kind: ProfileKind::Dry,
    },
    Profile {
        category: "CPU",
        name: "Light Dry",
        summary: "Light physics, 2 close mics and built-in room off (lowest CPU, ~0.4% idle)",
        preferred: false,
        kind: ProfileKind::LightDry,
    },
];

/// Parse a `nice-plug` CLAP state blob (`u64` little-endian length prefix + UTF-8 JSON).
pub fn parse_state(raw: &[u8]) -> Result<Value, String> {
    if raw.len() < 8 {
        return Err(format!("Unsampled Piano state too short ({} bytes)", raw.len()));
    }
    let len = u64::from_le_bytes(raw[..8].try_into().unwrap()) as usize;
    if raw.len() != 8 + len {
        return Err(format!("Unsampled Piano state length mismatch (header {len}, body {})", raw.len() - 8));
    }
    let root: Value = serde_json::from_slice(&raw[8..])
        .map_err(|e| format!("invalid Unsampled Piano state JSON: {e}"))?;
    if !root.is_object() {
        return Err("Unsampled Piano state root is not a JSON object".into());
    }
    Ok(root)
}

/// Encode a `nice-plug` CLAP state blob (`u64` little-endian length prefix + UTF-8 JSON).
pub fn encode_state(root: &Value) -> Result<Vec<u8>, String> {
    let body = serde_json::to_vec(root)
        .map_err(|e| format!("serializing Unsampled Piano state: {e}"))?;
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&(body.len() as u64).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

fn ensure_obj<'a>(parent: &'a mut serde_json::Map<String, Value>, key: &str) -> Result<&'a mut serde_json::Map<String, Value>, String> {
    if !parent.contains_key(key) {
        parent.insert(key.into(), Value::Object(Default::default()));
    }
    parent
        .get_mut(key)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| format!("Unsampled Piano model field '{key}' is not an object"))
}

fn ab_mics_json() -> [Value; 2] {
    [
        json!({
            "at": [0.323529, -1.230769, 2.0],
            "upright": [-0.2, 0.7, 2.0],
            "pattern": 1.0,
            "gain_db": 7.8,
            "pan": -1.0,
        }),
        json!({
            "at": [0.735294, -1.230769, 2.0],
            "upright": [-0.2, 0.3, 2.0],
            "pattern": 1.0,
            "gain_db": 7.8,
            "pan": 1.0,
        }),
    ]
}

fn is_close_mics_pair(mics: &[Value]) -> bool {
    if mics.len() != 2 {
        return false;
    }
    let (Some(a), Some(b)) = (mics[0].as_object(), mics[1].as_object()) else {
        return false;
    };
    let z_close = |o: &serde_json::Map<String, Value>| {
        o.get("at")
            .and_then(Value::as_array)
            .and_then(|arr| arr.get(2))
            .and_then(Value::as_f64)
            .is_some_and(|z| (z - 0.4).abs() < 1e-3)
    };
    a.get("pan").and_then(Value::as_f64) == Some(-1.0)
        && b.get("pan").and_then(Value::as_f64) == Some(1.0)
        && a.get("pattern").and_then(Value::as_f64) == Some(0.5)
        && b.get("pattern").and_then(Value::as_f64) == Some(0.5)
        && z_close(a)
        && z_close(b)
}

fn trim_to_close_mics(sb: &mut serde_json::Map<String, Value>) {
    if let Some(mics) = sb.get_mut("mics").and_then(Value::as_array_mut) {
        if mics.len() > 2 && mics.first().is_some_and(Value::is_object) {
            mics.truncate(2);
        }
    }
}

/// Apply `profile` to an existing Unsampled Piano CLAP state blob, keeping all
/// unrelated model fields, live parameters and MIDI mappings intact.
pub fn apply_profile(raw: &[u8], profile: &Profile) -> Result<Vec<u8>, String> {
    let mut root = parse_state(raw)?;
    let model_str = root
        .get("fields")
        .and_then(|f| f.get("model"))
        .and_then(Value::as_str)
        .ok_or_else(|| "Unsampled Piano state has no fields.model string".to_string())?;
    let mut model: Value = serde_json::from_str(model_str)
        .map_err(|e| format!("invalid Unsampled Piano model JSON: {e}"))?;
    let m = model
        .as_object_mut()
        .ok_or_else(|| "Unsampled Piano model is not a JSON object".to_string())?;

    let (sym_on, sym_max_freq, nl_on, nl_max_note, struct_on, room_on, trim_mics) =
        match profile.kind {
            ProfileKind::Full => (true, 5000.0, true, 64u64, true, true, false),
            ProfileKind::Dry => (true, 5000.0, true, 64u64, true, false, false),
            ProfileKind::Balanced => (true, 2000.0, true, 48u64, true, true, true),
            ProfileKind::Light => (false, 5000.0, false, 64u64, false, true, true),
            ProfileKind::LightDry => (false, 5000.0, false, 64u64, false, false, true),
        };

    {
        let sym = ensure_obj(m, "sympathetic")?;
        sym.insert("enabled".into(), json!(sym_on));
        sym.insert("max_freq".into(), json!(sym_max_freq));
    }
    {
        let nl = ensure_obj(m, "nonlinear")?;
        nl.insert("enabled".into(), json!(nl_on));
        nl.insert("max_note".into(), json!(nl_max_note));
    }
    for key in ["frame", "rim", "polarization"] {
        ensure_obj(m, key)?.insert("enabled".into(), json!(struct_on));
    }
    ensure_obj(m, "room")?.insert("enabled".into(), json!(room_on));

    if let Some(sb) = m.get_mut("soundboard").and_then(Value::as_object_mut) {
        if trim_mics {
            trim_to_close_mics(sb);
        } else if let Some(mics) = sb.get_mut("mics").and_then(Value::as_array_mut) {
            if is_close_mics_pair(mics) {
                mics.extend(ab_mics_json());
            }
        }
    }

    let encoded_model = serde_json::to_string(&model)
        .map_err(|e| format!("serializing Unsampled Piano model: {e}"))?;
    let fields = root
        .get_mut("fields")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| "Unsampled Piano state fields is not an object".to_string())?;
    fields.insert("model".into(), Value::String(encoded_model));
    encode_state(&root)
}

/// Identify which built-in profile an Unsampled Piano CLAP state matches.
pub fn detect_profile(raw: &[u8]) -> Option<&'static Profile> {
    let root = parse_state(raw).ok()?;
    let model_str = root.get("fields")?.get("model")?.as_str()?;
    let model: Value = serde_json::from_str(model_str).ok()?;
    let room_on = model.pointer("/room/enabled").and_then(Value::as_bool).unwrap_or(true);
    let sym_on = model.pointer("/sympathetic/enabled").and_then(Value::as_bool).unwrap_or(true);
    let sym_max_freq = model.pointer("/sympathetic/max_freq").and_then(Value::as_f64).unwrap_or(5000.0);
    let nl_on = model.pointer("/nonlinear/enabled").and_then(Value::as_bool).unwrap_or(true);
    let nl_max_note = model.pointer("/nonlinear/max_note").and_then(Value::as_u64).unwrap_or(64);
    let frame_on = model.pointer("/frame/enabled").and_then(Value::as_bool).unwrap_or(true);
    let rim_on = model.pointer("/rim/enabled").and_then(Value::as_bool).unwrap_or(true);
    let pol_on = model.pointer("/polarization/enabled").and_then(Value::as_bool).unwrap_or(true);

    let find = |kind: ProfileKind| PROFILES.iter().find(|p| p.kind == kind);

    if !sym_on && !nl_on && !frame_on && !rim_on && !pol_on {
        return find(if room_on { ProfileKind::Light } else { ProfileKind::LightDry });
    }
    if sym_on && sym_max_freq <= 2000.0 + 1e-6 && nl_on && nl_max_note <= 48 && frame_on && rim_on && pol_on && room_on {
        return find(ProfileKind::Balanced);
    }
    if sym_on && nl_on && frame_on && rim_on && pol_on {
        return find(if room_on { ProfileKind::Full } else { ProfileKind::Dry });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(kind: ProfileKind) -> &'static Profile {
        PROFILES.iter().find(|p| p.kind == kind).unwrap()
    }

    fn main_schema_fixture() -> Vec<u8> {
        let model = json!({
            "a4": 442.0,
            "polarization": { "enabled": true, "h_coupling": 0.03 },
            "soundboard": {
                "f_max": 20000.0,
                "exact_below": 6000.0,
                "radiation_delays": true,
                "mics": [
                    { "at": [0.294118, 0.346154, 0.4], "upright": [0.705882, 0.653846, 0.4], "pattern": 0.5, "aim": [0.294118, 0.346154, 0.0], "aim_upright": [0.705882, 0.653846, 0.0], "gain_db": 0.0, "pan": -1.0 },
                    { "at": [0.705882, 0.653846, 0.4], "upright": [0.294118, 0.346154, 0.4], "pattern": 0.5, "aim": [0.705882, 0.653846, 0.0], "aim_upright": [0.294118, 0.346154, 0.0], "gain_db": 0.0, "pan": 1.0 },
                    { "at": [0.323529, -1.230769, 2.0], "upright": [-0.2, 0.7, 2.0], "pattern": 1.0, "gain_db": 7.8, "pan": -1.0 },
                    { "at": [0.735294, -1.230769, 2.0], "upright": [-0.2, 0.3, 2.0], "pattern": 1.0, "gain_db": 7.8, "pan": 1.0 }
                ]
            },
            "sympathetic": { "enabled": true, "max_freq": 5000.0 },
            "nonlinear": { "enabled": true, "max_note": 64 },
            "room": { "enabled": true, "order": 3 },
            "frame": { "enabled": true },
            "rim": { "enabled": true }
        });
        let root = json!({
            "version": "0.2.1",
            "params": { "output_gain_db": -3.5 },
            "fields": {
                "model": serde_json::to_string(&model).unwrap(),
                "midi": "{\"mappings\":[{\"cc\":11,\"path\":\"output_gain_db\"}]}"
            }
        });
        encode_state(&root).unwrap()
    }

    fn v021_schema_fixture() -> Vec<u8> {
        let model = json!({
            "a4": 440.0,
            "polarization": { "enabled": true },
            "soundboard": {
                "f_max": 20000.0,
                "mics": [[0.45, 0.35, 1.5], [1.15, 0.85, 1.5]],
                "mics_upright": [[0.7, 1.0, 1.5], [0.3, 0.45, 1.5]]
            },
            "sympathetic": { "enabled": true, "max_freq": 5000.0 },
            "nonlinear": { "enabled": true, "max_note": 64 },
            "room": { "enabled": true },
            "frame": { "enabled": true },
            "rim": { "enabled": true }
        });
        let root = json!({
            "version": "0.2.1",
            "params": { "output_gain_db": 0.0 },
            "fields": {
                "model": serde_json::to_string(&model).unwrap(),
                "midi": "{}"
            }
        });
        encode_state(&root).unwrap()
    }

    fn inner_model(raw: &[u8]) -> Value {
        let root = parse_state(raw).unwrap();
        serde_json::from_str(root["fields"]["model"].as_str().unwrap()).unwrap()
    }

    #[test]
    fn framing_rejects_short_or_mismatched_state_and_round_trips_valid() {
        assert!(parse_state(&[1, 2, 3]).is_err());
        let valid = main_schema_fixture();
        let mut truncated = valid.clone();
        truncated.pop();
        assert!(parse_state(&truncated).is_err());
        let mut bad_json = (3u64).to_le_bytes().to_vec();
        bad_json.extend_from_slice(b"{{{");
        assert!(parse_state(&bad_json).is_err());
        let root = parse_state(&valid).unwrap();
        assert_eq!(parse_state(&encode_state(&root).unwrap()).unwrap(), root);
    }

    #[test]
    fn all_profiles_apply_detect_and_preserve_unrelated_state() {
        let start = main_schema_fixture();
        assert_eq!(detect_profile(&start), Some(profile(ProfileKind::Full)));

        for &p in PROFILES {
            let next = apply_profile(&start, &p).unwrap();
            assert_eq!(detect_profile(&next), Some(profile(p.kind)), "detect {}", p.name);
            let root = parse_state(&next).unwrap();
            assert_eq!(root["params"]["output_gain_db"], json!(-3.5));
            assert_eq!(root["fields"]["midi"], json!("{\"mappings\":[{\"cc\":11,\"path\":\"output_gain_db\"}]}"));
            let m = inner_model(&next);
            assert_eq!(m["a4"], json!(442.0));
            assert_eq!(m["polarization"]["h_coupling"], json!(0.03));
            assert_eq!(m["room"]["order"], json!(3));
        }

        // Lossless round-trip: Full -> Light Dry -> Balanced -> Dry -> Full restores all 4 mics
        // while leaving unrelated soundboard fields intact.
        let light_dry = apply_profile(&start, profile(ProfileKind::LightDry)).unwrap();
        let ld_m = inner_model(&light_dry);
        assert_eq!(ld_m["soundboard"]["mics"].as_array().unwrap().len(), 2);
        assert_eq!(ld_m["soundboard"]["exact_below"], json!(6000.0));
        assert_eq!(ld_m["soundboard"]["radiation_delays"], json!(true));
        assert_eq!(ld_m["room"]["enabled"], json!(false));

        let balanced = apply_profile(&light_dry, profile(ProfileKind::Balanced)).unwrap();
        let bal_m = inner_model(&balanced);
        assert_eq!(bal_m["sympathetic"]["max_freq"], json!(2000.0));
        assert_eq!(bal_m["nonlinear"]["max_note"], json!(48));
        assert_eq!(bal_m["soundboard"]["mics"].as_array().unwrap().len(), 2);

        let dry = apply_profile(&balanced, profile(ProfileKind::Dry)).unwrap();
        let dry_m = inner_model(&dry);
        assert_eq!(dry_m["room"]["enabled"], json!(false));
        assert_eq!(dry_m["soundboard"]["mics"].as_array().unwrap().len(), 4);

        let back = apply_profile(&dry, profile(ProfileKind::Full)).unwrap();
        let back_m = inner_model(&back);
        assert_eq!(back_m["room"]["enabled"], json!(true));
        assert_eq!(back_m["sympathetic"]["max_freq"], json!(5000.0));
        assert_eq!(back_m["nonlinear"]["max_note"], json!(64));
        assert_eq!(back_m["soundboard"]["exact_below"], json!(6000.0));
        assert_eq!(back_m["soundboard"]["radiation_delays"], json!(true));
        assert_eq!(back_m["soundboard"]["mics"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn v021_tuple_mics_schema_applies_without_inserting_newer_keys() {
        let start = v021_schema_fixture();
        assert_eq!(detect_profile(&start), Some(profile(ProfileKind::Full)));
        for &p in PROFILES {
            let next = apply_profile(&start, &p).unwrap();
            assert_eq!(detect_profile(&next), Some(profile(p.kind)));
            let m = inner_model(&next);
            assert_eq!(m["soundboard"]["mics"], json!([[0.45, 0.35, 1.5], [1.15, 0.85, 1.5]]));
            assert!(m["soundboard"].get("exact_below").is_none());
            assert!(m["soundboard"].get("radiation_delays").is_none());
        }
    }
}
