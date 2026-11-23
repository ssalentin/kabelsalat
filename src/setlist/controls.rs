//! The show controls as agent tools see them (#277, #437): every action with
//! the control learned for it, and the devices whose program change picks the
//! song. The app and headless MCP answer with the same functions.

use serde_json::{json, Value};

use super::{note_label, parse_note, Setlist, MAX_PROGRAM_DEVICES};
use crate::show::{ShowAction, ShowControl, ShowMsg, MAX_SHOW_CONTROLS};

/// A control's message as the app and tools name it.
pub fn msg_label(msg: ShowMsg) -> String {
    match msg {
        ShowMsg::Cc(n) => format!("CC {n}"),
        ShowMsg::Note(n) => format!("Note {}", note_label(n)),
        ShowMsg::Program(n) => format!("Program {n}"),
        ShowMsg::AnyProgram => "Every program".into(),
    }
}

pub(crate) fn control_json(c: ShowControl, device_name: &dyn Fn(u32) -> String) -> Value {
    let mut v = serde_json::to_value(c.msg).unwrap_or(Value::Null);
    if let Some(m) = v.as_object_mut() {
        m.insert("label".into(), json!(msg_label(c.msg)));
        m.insert("device".into(), json!(c.device));
        m.insert("device_name".into(), json!(c.device.map(device_name)));
        if c.channel.is_some() {
            m.insert("channel".into(), json!(c.channel));
        }
    }
    v
}

fn ids() -> String {
    ShowAction::ALL.iter().map(|a| a.id()).collect::<Vec<_>>().join(", ")
}

/// Parsed `set_show_control` tool request shared between headless `Setlist`
/// and the live GUI app (`config.json` vs session overrides).
pub(crate) enum ParsedShowControl {
    Clear {
        action: ShowAction,
    },
    Bind {
        action: ShowAction,
        msg: ShowMsg,
        device: Option<u32>,
        device_port: Option<String>,
        channel: Option<u8>,
    },
    SongOfProgram {
        device: Option<u32>,
        device_port: Option<String>,
        on: bool,
    },
}

pub(crate) fn parse_show_control_args(args: &Value) -> Result<(ParsedShowControl, bool), String> {
    let id = args
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("action required: one of {}", ids()))?;
    let action = ShowAction::from_id(id)
        .ok_or_else(|| format!("unknown action '{id}' (use one of {})", ids()))?;
    let global_scope = match args.get("scope") {
        None | Some(Value::Null) => true,
        Some(v) => match v.as_str().map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("global") => true,
            Some("session") => false,
            _ => return Err("scope must be 'global' or 'session'".into()),
        },
    };
    let device = match args.get("device") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .filter(|&d| d <= u32::MAX as u64)
                .ok_or("device: a hardware device index from get_show_controls")? as u32,
        ),
    };
    let device_port = match args.get("device_port") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let s = v
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or("device_port: non-empty port name string")?;
            Some(s.to_string())
        }
    };
    if action == ShowAction::SongOfProgram {
        let on = args.get("on").and_then(Value::as_bool).unwrap_or(true);
        return Ok((
            ParsedShowControl::SongOfProgram {
                device,
                device_port,
                on,
            },
            global_scope,
        ));
    }
    if args.get("clear").and_then(Value::as_bool) == Some(true) {
        return Ok((ParsedShowControl::Clear { action }, global_scope));
    }
    let channel = match args.get("channel") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let ch = v
                .as_u64()
                .filter(|&c| c <= 15)
                .ok_or("channel: MIDI channel 0..15")?;
            Some(ch as u8)
        }
    };
    let number = |key: &str| -> Result<Option<u8>, String> {
        match args.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if key == "note" => parse_note(s)
                .map(Some)
                .ok_or_else(|| format!("note '{s}': a MIDI note 0..127 or a name like C2")),
            Some(v) => v
                .as_u64()
                .filter(|&n| n <= 127)
                .map(|n| Some(n as u8))
                .ok_or_else(|| format!("{key}: 0..127")),
        }
    };
    let msgs = [
        (number("cc")?, ShowMsg::Cc as fn(u8) -> ShowMsg),
        (number("note")?, ShowMsg::Note),
        (number("program")?, ShowMsg::Program),
    ];
    let mut given = msgs.into_iter().filter_map(|(n, f)| n.map(f));
    let (Some(msg), None) = (given.next(), given.next()) else {
        return Err("give one of cc, note or program (0..127), or clear: true".into());
    };
    Ok((
        ParsedShowControl::Bind {
            action,
            msg,
            device,
            device_port,
            channel,
        },
        global_scope,
    ))
}

impl Setlist {
    /// The `get_show_controls` answer. `devices` are the hardware devices
    /// to list for song selection (the chosen ones are always listed).
    pub fn show_controls_json(&self, devices: &[u32], device_name: &dyn Fn(u32) -> String) -> Value {
        let actions: Vec<Value> = ShowAction::LEARNED
            .iter()
            .map(|&a| json!({"action": a.id(), "label": a.label(), "control": self.control_of(a).map(|c| control_json(c, device_name))}))
            .collect();
        let mut listed: Vec<u32> = devices.iter().copied().chain(self.program_devices()).collect();
        listed.sort_unstable();
        listed.dedup();
        let programs: Vec<Value> = listed
            .into_iter()
            .map(|d| json!({"device": d, "name": device_name(d), "on": self.program_selects_song(d)}))
            .collect();
        json!({
            "actions": actions,
            "song_of_program": programs,
            "slots": {"used": self.controls.len(), "max": MAX_SHOW_CONTROLS},
        })
    }

    /// The `set_show_control` tool: bind or clear one action's control, or
    /// choose whether a device's program change picks the song. Returns
    /// what changed, in words.
    pub fn set_show_control(&mut self, args: &Value) -> Result<String, String> {
        let (parsed, _) = parse_show_control_args(args)?;
        match parsed {
            ParsedShowControl::SongOfProgram { device, on, .. } => {
                let d = device.ok_or("song_of_program needs device (a hardware device index from get_show_controls)")?;
                if on && !self.program_selects_song(d) && self.program_devices().count() >= MAX_PROGRAM_DEVICES {
                    return Err(format!("at most {MAX_PROGRAM_DEVICES} devices pick songs: turn one off first"));
                }
                self.set_program_selects_song(d, on);
                Ok(format!("device {d}'s program change {} the song", if on { "now picks" } else { "no longer picks" }))
            }
            ParsedShowControl::Clear { action } => {
                self.forget(action);
                Ok(format!("{} has no control", action.label()))
            }
            ParsedShowControl::Bind { action, msg, device, channel, .. } => {
                let taken = self.learn_with_channel(action, device, channel, msg);
                let from = device.map_or("any device".to_string(), |d| format!("device {d}"));
                let ch = channel.map_or(String::new(), |c| format!(" ch {}", c + 1));
                Ok(match taken {
                    Some(other) => format!("{} from {from}{ch} now does {} (it no longer does {})", msg_label(msg), action.label(), other.label()),
                    None => format!("{} from {from}{ch} now does {}", msg_label(msg), action.label()),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tool_binds_clears_and_chooses_program_devices() {
        let mut sl = Setlist::default();
        let name = |d: u32| format!("hw:{d}");
        assert!(sl.set_show_control(&json!({"action": "panic", "note": "C2", "device": 1})).unwrap().contains("Note C2"));
        assert_eq!(sl.control_of(ShowAction::Panic).map(|c| (c.msg, c.device)), Some((ShowMsg::Note(36), Some(1))));
        let moved = sl.set_show_control(&json!({"action": "tuner", "note": 36, "device": 1})).unwrap();
        assert!(moved.contains("no longer does Panic"), "{moved}");
        sl.set_show_control(&json!({"action": "next_part", "cc": 80, "channel": 2})).unwrap();
        assert_eq!(sl.control_of(ShowAction::NextPart).and_then(|c| c.channel), Some(2));
        sl.set_show_control(&json!({"action": "show_mode", "cc": 82})).unwrap();
        assert_eq!(sl.control_of(ShowAction::ShowMode).map(|c| c.msg), Some(ShowMsg::Cc(82)));
        sl.set_show_control(&json!({"action": "song_of_program", "device": 2})).unwrap();
        let v = sl.show_controls_json(&[0, 2], &name);
        let row = |a: &str| v["actions"].as_array().unwrap().iter().find(|r| r["action"] == a).cloned().unwrap();
        assert_eq!(row("tuner")["control"], json!({"note": 36, "label": "Note C2", "device": 1, "device_name": "hw:1"}));
        assert_eq!(row("next_part")["control"]["cc"], 80);
        assert_eq!(row("next_part")["control"]["channel"], 2);
        assert_eq!(row("panic")["control"], Value::Null);
        assert_eq!(v["actions"].as_array().unwrap().len(), ShowAction::LEARNED.len(), "every action is listed, bound or not");
        assert_eq!(v["song_of_program"], json!([{"device": 0, "name": "hw:0", "on": false}, {"device": 2, "name": "hw:2", "on": true}]));
        sl.set_show_control(&json!({"action": "tuner", "clear": true})).unwrap();
        sl.set_show_control(&json!({"action": "show_mode", "clear": true})).unwrap();
        sl.set_show_control(&json!({"action": "song_of_program", "device": 2, "on": false})).unwrap();
        assert_eq!(sl.controls.len(), 1);
        for bad in [json!({}), json!({"action": "jump"}), json!({"action": "panic"}), json!({"action": "panic", "cc": 1, "note": 2}), json!({"action": "panic", "cc": 200}), json!({"action": "panic", "cc": 1, "channel": 16}), json!({"action": "panic", "cc": 1, "scope": "other"}), json!({"action": "song_of_program"})] {
            assert!(sl.set_show_control(&bad).is_err(), "{bad}");
        }
        assert_eq!(sl.controls.len(), 1, "a refused call changes nothing");
    }
}
