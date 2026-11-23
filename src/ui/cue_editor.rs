//! The cue editor of a song or a part in the setlist (#401): one line of
//! text per message, applied as soon as every line reads.

use super::*;
use crate::cues::Cue;

/// The editor's text between frames, with the cues it was made from: when
/// a tool changes the cues, the text starts over from them.
#[derive(Clone, Default)]
struct Buffer {
    from: String,
    text: String,
    open: bool,
}

fn canonical(cues: &[Cue]) -> String {
    cues.iter().map(Cue::text).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
thread_local! {
    /// Screenshots open every editor (nothing clicks in a headless frame).
    static OPEN_ALL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn load(ui: &egui::Ui, id: egui::Id, cues: &[Cue]) -> Buffer {
    let mut buf: Buffer = ui.data_mut(|d| d.get_temp(id)).unwrap_or_default();
    #[cfg(test)]
    if OPEN_ALL.with(|o| o.get()) {
        buf.open = true;
    }
    let now = canonical(cues);
    if buf.from != now {
        buf.from = now.clone();
        buf.text = now;
    }
    buf
}

fn id_of(ui: &egui::Ui, id: impl std::hash::Hash + std::fmt::Debug) -> egui::Id {
    ui.make_persistent_id(("cue_editor", id))
}

/// The "Cues" chip of a song or a part, inline in a wrapping row: how many
/// messages it sends on entry, and a toggle for the editor under the row
/// ([`cue_body`]). It adds no row of its own, so a card keeps its height.
pub(super) fn cue_chip(ui: &mut egui::Ui, id: impl std::hash::Hash + std::fmt::Debug, who: &str, cues: &[Cue]) {
    let id = id_of(ui, id);
    let mut buf = load(ui, id, cues);
    let n = cues.len();
    let lbl = match (buf.open, n) {
        (true, _) => "Hide cues ▴".to_string(),
        (false, 0) => "+ Cues".to_string(),
        (false, n) => format!("Cues ({n}) ▾"),
    };
    if theme::wrapped_chip_button(ui, &lbl, buf.open || n > 0)
        .on_hover_text(format!("MIDI and OSC messages sent when the setlist enters {who}"))
        .clicked()
    {
        buf.open = !buf.open;
    }
    ui.data_mut(|d| d.insert_temp(id, buf));
}

/// The editor of a song's or part's cues, shown while its chip is open:
/// one line per message, applied as soon as every line reads.
pub(super) fn cue_body(ui: &mut egui::Ui, id: impl std::hash::Hash + std::fmt::Debug, cues: &mut Vec<Cue>) {
    let id = id_of(ui, id);
    let mut buf = load(ui, id, cues);
    if buf.open {
        egui::Frame::new()
            .fill(theme::PAPER_SINK)
            .stroke(Stroke::new(1.0, theme::HAIRLINE))
            .corner_radius(6.0)
            .inner_margin(egui::Margin::same(6))
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut buf.text)
                        .id_salt(id.with("text"))
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .font(egui::FontId::monospace(12.0))
                        .text_color(theme::CREAM)
                        .frame(egui::Frame::NONE)
                        .hint_text("pc 1 5   ·   cc 1 20 127 @desk   ·   note 10 C3   ·   osc 192.168.1.50:8000 /cue/go 12"),
                );
                match crate::cues::parse_lines(buf.text.lines()) {
                    Ok(parsed) => {
                        if parsed != *cues {
                            *cues = parsed;
                            buf.from = canonical(cues);
                        }
                        if theme::wrapped_chip_button(ui, "Test now", false)
                            .on_hover_text("send these messages right now")
                            .clicked()
                        {
                            crate::cues::global().fire(cues.clone());
                        }
                    }
                    Err(e) => {
                        ui.add(egui::Label::new(egui::RichText::new(e).size(11.5).color(theme::CLIP_BRIGHT)).wrap());
                    }
                }
            });
    }
    ui.data_mut(|d| d.insert_temp(id, buf));
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::OPEN_ALL;
    use crate::cues::Sent;

    fn tool(app: &mut DemoApp, name: &str, args: serde_json::Value) -> serde_json::Value {
        let out = app.exec_tool(name, &args);
        serde_json::from_str(&out).unwrap_or_else(|_| serde_json::json!({"error": out}))
    }

    fn two_songs(app: &mut DemoApp) {
        for n in ["Opener", "Closer"] {
            tool(app, "set_setlist_song", serde_json::json!({
                "name": n,
                "parts": [{"name": "Verse", "rigs": [{"rig": 0}]}, {"name": "Chorus", "rigs": [{"rig": 0}]}],
            }));
        }
    }

    fn midi(app: &DemoApp) -> Vec<Vec<u8>> {
        app.cues.stats().log.iter().filter_map(|s| if let Sent::Midi { bytes, .. } = s { Some(bytes.clone()) } else { None }).collect()
    }

    #[test]
    fn the_cue_tools_are_in_every_table() {
        let tools = crate::assistant::tools();
        let names: Vec<&str> = tools.as_array().unwrap().iter().filter_map(|t| t["function"]["name"].as_str()).collect();
        for name in ["get_cues", "set_cues"] {
            assert!(names.contains(&name), "assistant tools miss {name}");
            assert!(UI_BRIDGE_TOOLS.contains(&name), "UI_BRIDGE_TOOLS miss {name}");
            assert!(include_str!("../mcp.rs").contains(&format!("#[tool(name = \"{name}\"")), "MCP misses {name}");
        }
    }

    /// A part switch emits the configured cues: set through the tool,
    /// stepped by the tool, raw bytes read back; a session load is silent.
    #[test]
    fn a_part_switch_emits_the_configured_cues_and_a_load_emits_none() {
        let mut app = DemoApp::new_with_sink(None);
        two_songs(&mut app);
        tool(&mut app, "set_cues", serde_json::json!({"song": 0, "cues": ["pc 1 5"]}));
        tool(&mut app, "set_cues", serde_json::json!({"song": 0, "part": 1, "cues": ["cc 1 20 127", "note 10 C3 100"]}));
        tool(&mut app, "set_cues", serde_json::json!({"song": 1, "cues": ["pc 2 9 @desk"]}));
        let v = tool(&mut app, "set_cues", serde_json::json!({"song": 9, "cues": []}));
        assert!(v["error"].as_str().unwrap().contains("no song at index 9"), "{v}");
        let got = tool(&mut app, "get_cues", serde_json::json!({}));
        assert_eq!(got["songs"][0]["parts"][1]["cues"], serde_json::json!(["cc 1 20 127", "note 10 C3 100"]), "{got}");
        assert!(midi(&app).is_empty(), "editing sends nothing");
        app.select_setlist(0, 0).unwrap();
        assert_eq!(midi(&app), [vec![0xC0, 5]], "entering the song sends the song's cue");
        app.select_setlist(0, 1).unwrap();
        assert_eq!(midi(&app)[1..], [vec![0xB0, 20, 127], vec![0x99, 48, 100], vec![0x89, 48, 0]], "a part of the same song sends only its own");
        app.select_setlist(1, 0).unwrap();
        assert_eq!(app.cues.stats().log.last(), Some(&Sent::Midi { bytes: vec![0xC1, 9], port: Some("desk".into()) }));
        let n = midi(&app).len();
        // Leaving the setlist and editing again sends nothing.
        app.step_setlist("clear").unwrap();
        tool(&mut app, "set_cues", serde_json::json!({"song": 1, "cues": []}));
        assert_eq!(midi(&app).len(), n);
        // test: true sends now.
        tool(&mut app, "set_cues", serde_json::json!({"song": 0, "cues": ["pc 3 3"], "test": true}));
        assert_eq!(midi(&app).len(), n + 1);
    }

    #[test]
    fn cues_are_saved_with_the_session_and_rewriting_parts_keeps_them() {
        let mut app = DemoApp::new_with_sink(None);
        two_songs(&mut app);
        tool(&mut app, "set_cues", serde_json::json!({"song": 0, "part": 0, "cues": ["osc 127.0.0.1:9000 /a 1"]}));
        tool(&mut app, "set_setlist_song", serde_json::json!({"song": 0, "parts": [{"name": "Verse", "rigs": [{"rig": 0}]}]}));
        assert_eq!(app.setlist.songs[0].parts[0].cues.len(), 1, "a same-named part keeps its cues");
        let json = serde_json::to_string(&app.setlist).unwrap();
        let back: crate::setlist::Setlist = serde_json::from_str(&json).unwrap();
        assert_eq!(back.songs[0].parts[0].cues, app.setlist.songs[0].parts[0].cues);
    }

    #[test]
    fn following_a_clock_is_a_tool_never_both_ways_and_named_inputs_resolve() {
        let mut app = DemoApp::new_with_sink(None);
        app.clock_in.stats.lock().unwrap().inputs = vec!["Roland SPD:SPD MIDI 1".into(), "Korg:Volca".into()];
        let v = tool(&mut app, "set_midi_clock", serde_json::json!({"follow": true, "input": "volca"}));
        assert_eq!((&v["follow"]["follow"], &v["follow"]["input"]), (&serde_json::json!(true), &serde_json::json!("Korg:Volca")), "{v}");
        let v = tool(&mut app, "set_midi_clock", serde_json::json!({"enabled": true}));
        assert_eq!(v["enabled"], true, "send can be turned on: it turns following off");
        let v = tool(&mut app, "set_midi_clock", serde_json::json!({"follow": true}));
        assert!(v["error"].as_str().unwrap().contains("both"), "{v}");
        assert!(tool(&mut app, "set_midi_clock", serde_json::json!({"input": "nothing"}))["error"].as_str().unwrap().contains("no MIDI in port"));
    }

    #[test]
    #[ignore = "writes PNGs: KABELSALAT_SHOTS=<dir> cargo test --bin kabelsalat cue_shots -- --ignored"]
    fn cue_shots() {
        let dir = std::path::PathBuf::from(std::env::var("KABELSALAT_SHOTS").expect("KABELSALAT_SHOTS=<dir>"));
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = DemoApp::new_with_sink(None);
        tool(&mut app, "set_setlist_song", serde_json::json!({"name": "Opening Night", "bpm": 118.0, "parts": [
            {"name": "Verse", "rigs": [{"rig": 0}], "cues": ["pc 1 5", "osc 192.168.1.50:8000 /lights/scene 3"]},
            {"name": "Chorus", "rigs": [{"rig": 0}], "cues": ["cc 1 20 127 @desk", "note 10 C3 100"]}]}));
        tool(&mut app, "set_cues", serde_json::json!({"song": 0, "cues": ["pc 1 12", "osc 192.168.1.50:8000 /lyrics/song \"Opening Night\""]}));
        tool(&mut app, "set_setlist_song", serde_json::json!({"name": "Midnight Waltz", "parts": [{"name": "Theme", "rigs": [{"rig": 0}]}]}));
        OPEN_ALL.with(|o| o.set(true));
        app.stage_view = StageView::Setlist;
        super::offline_shots::shot(&mut app, Vec2::new(1280.0, 800.0), &dir.join("cues.png"));
        OPEN_ALL.with(|o| o.set(false));
        app.clock_in.stats.lock().unwrap().inputs = vec!["Roland SPD:SPD MIDI 1".into(), "Korg:volca beats".into()];
        app.clock_in.set(Some(true), Some(Some("Korg:volca beats".into())));
        {
            let mut s = app.clock_in.stats.lock().unwrap();
            s.locked = true;
            s.bpm_in = 124.0;
            s.error_ms = 0.3;
        }
        app.stage_view = StageView::Matrix;
        super::offline_shots::shot(&mut app, Vec2::new(1280.0, 800.0), &dir.join("clock-follow.png"));
    }
}
