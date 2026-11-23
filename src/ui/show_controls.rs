//! Show controls in the app (#277, #437): learning or editing a pedal, key
//! or button for a show action, persisting bindings by stable MIDI port name in
//! `config.json`, keeping hardware show messages off instruments/knobs, handling
//! app-side actions (tuner, recording, Show mode), and rendering the Show
//! settings page and Setlist show-controls list.

use super::*;
use crate::show::{ShowAction, ShowControl, ShowMsg};

/// One row of the action list, and the width of its action's name.
const ROW_H: f32 = 28.0;
const ACTION_W: f32 = 128.0;
/// The tuner panel's size and the cents its needle spans either way.
const TUNER_SIZE: Vec2 = Vec2::new(340.0, 168.0);
const TUNER_SPAN_CENTS: f32 = 50.0;
/// In tune: the needle and the note turn teal within this many cents.
const IN_TUNE_CENTS: f32 = 3.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MsgKind {
    Cc,
    Note,
    Program,
}

impl MsgKind {
    const ALL: [MsgKind; 3] = [MsgKind::Cc, MsgKind::Note, MsgKind::Program];

    fn label(self) -> &'static str {
        match self {
            MsgKind::Cc => "CC",
            MsgKind::Note => "Note",
            MsgKind::Program => "Program",
        }
    }
}

pub(super) struct ShowControlEdit {
    pub(super) action: ShowAction,
    pub(super) kind: MsgKind,
    pub(super) number_text: String,
    pub(super) channel: Option<u8>,
    pub(super) device_port: Option<String>,
    pub(super) device_index: Option<u32>,
    pub(super) error: Option<String>,
    pub(super) focus: bool,
}

pub(super) enum ShowControlAction {
    ToggleLearn(ShowAction),
    OpenEdit(ShowAction),
    CancelEdit,
    SaveEdit,
    Clear(ShowAction),
    ToggleProgramPort {
        device: Option<u32>,
        port: Option<String>,
        on: bool,
    },
    PromoteSessionOverrides,
    ClearSessionOverrides,
}

/// What the tuner shows.
enum TunerReading {
    /// No Audio In unit in the rack.
    NoInput,
    /// An input, and the pitch it plays (none while it is quiet).
    Input { name: String, pitch: Option<crate::audio_in::Pitch> },
}

fn format_control_label(
    msg: ShowMsg,
    channel: Option<u8>,
    device_label: Option<&str>,
    offline: bool,
    session_override: bool,
) -> String {
    let mut parts = vec![crate::setlist::msg_label(msg)];
    if let Some(ch) = channel {
        parts.push(format!("Ch {}", ch + 1));
    }
    if let Some(dev) = device_label {
        parts.push(dev.to_string());
    }
    let mut out = parts.join(" · ");
    if offline {
        out.push_str(" (offline)");
    }
    if session_override {
        out.push_str(" (session)");
    }
    out
}

fn parse_edit_msg(kind: MsgKind, raw: &str) -> Result<ShowMsg, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("Enter a value (0–127)".into());
    }
    match kind {
        MsgKind::Cc => {
            let n: u8 = s
                .parse::<u16>()
                .ok()
                .filter(|&v| v <= 127)
                .map(|v| v as u8)
                .ok_or_else(|| "CC must be 0–127".to_string())?;
            Ok(ShowMsg::Cc(n))
        }
        MsgKind::Note => {
            let n = crate::setlist::parse_note(s)
                .ok_or_else(|| "Note must be 0–127 or a note like C2".to_string())?;
            Ok(ShowMsg::Note(n))
        }
        MsgKind::Program => {
            let n: u8 = s
                .parse::<u16>()
                .ok()
                .filter(|&v| v <= 127)
                .map(|v| v as u8)
                .ok_or_else(|| "Program must be 0–127".to_string())?;
            Ok(ShowMsg::Program(n))
        }
    }
}

impl DemoApp {
    /// Effective engine show controls: permanent `config.json` bindings merged
    /// with any per-session overrides on `self.setlist.controls`.
    pub(super) fn effective_show_controls(&self) -> crate::show::ShowControls {
        self.config_show_controls.merge_with_session(
            &self.setlist.controls,
            &self.hw_devices,
            midi_port_key,
        )
    }

    pub(super) fn effective_show_control(&self, action: ShowAction) -> Option<ShowControl> {
        self.effective_show_controls()
            .into_iter()
            .flatten()
            .find(|c| c.action == action && c.action != ShowAction::SongOfProgram)
    }

    pub(super) fn effective_program_selects_song(&self, d: u32) -> bool {
        self.effective_show_controls()
            .into_iter()
            .flatten()
            .any(|c| c.action == ShowAction::SongOfProgram && c.device == Some(d))
    }

    pub(super) fn has_session_show_overrides(&self) -> bool {
        !self.setlist.controls.is_empty()
    }

    fn connected_port_index(&self, port_key: &str) -> Option<u32> {
        self.hw_devices
            .iter()
            .position(|raw| midi_port_key(raw) == port_key)
            .map(|i| i as u32)
    }

    /// Persist a learned or edited Show control to `config.json` (keyed by
    /// `midi_port_key` when a device is known), clearing conflicting session
    /// overrides so the permanent choice takes effect immediately.
    pub(super) fn persist_show_control(
        &mut self,
        action: ShowAction,
        device: Option<u32>,
        device_port: Option<String>,
        channel: Option<u8>,
        msg: ShowMsg,
    ) -> Option<ShowAction> {
        let resolved_port = device_port
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                device.and_then(|d| {
                    self.hw_devices
                        .get(d as usize)
                        .map(|raw| midi_port_key(raw).to_string())
                })
            });
        let device_index = if resolved_port.is_none() { device } else { None };
        let resolved_idx = resolved_port
            .as_deref()
            .and_then(|p| self.connected_port_index(p))
            .or(device_index);
        let channel = channel.map(|c| c & 0x0F);

        // Clear any matching/conflicting per-session override so permanent config wins live.
        self.setlist.controls.retain(|c| {
            if c.action == ShowAction::SongOfProgram {
                return true;
            }
            let same_action = c.action == action;
            let overlaps = c.msg == msg
                && (c.device.is_none() || resolved_idx.is_none() || c.device == resolved_idx)
                && (c.channel.is_none() || channel.is_none() || c.channel == channel);
            !same_action && !overlaps
        });

        let displaced = self.config_show_controls.set_binding(
            action,
            msg,
            resolved_port,
            device_index,
            channel,
        );
        let _ = crate::config::save_show_controls(&self.config_show_controls);
        displaced
    }

    pub(super) fn clear_permanent_show_control(&mut self, action: ShowAction) {
        self.config_show_controls.clear_binding(action);
        self.setlist.forget(action);
        let _ = crate::config::save_show_controls(&self.config_show_controls);
    }

    pub(super) fn set_permanent_program_device(
        &mut self,
        d: Option<u32>,
        port_key: Option<String>,
        on: bool,
    ) -> Result<(), String> {
        let resolved_port = port_key
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                d.and_then(|idx| {
                    self.hw_devices
                        .get(idx as usize)
                        .map(|raw| midi_port_key(raw).to_string())
                })
            });
        if on {
            let already = match (&resolved_port, d) {
                (Some(p), _) => self.config_show_controls.program_selects_port(p),
                (None, Some(idx)) => self.config_show_controls.program_selects_index(idx),
                (None, None) => false,
            };
            let used = self.config_show_controls.program_devices.len()
                + self.config_show_controls.program_device_indices.len();
            if !already && used >= crate::setlist::MAX_PROGRAM_DEVICES {
                return Err(format!(
                    "at most {} devices pick songs: turn one off first",
                    crate::setlist::MAX_PROGRAM_DEVICES
                ));
            }
        }
        if let Some(port) = resolved_port {
            let conn_idx = d.or_else(|| self.connected_port_index(&port));
            self.config_show_controls.set_program_port(port, on);
            if let Some(idx) = conn_idx {
                self.setlist.set_program_selects_song(idx, false);
            }
        } else if let Some(idx) = d {
            self.config_show_controls.set_program_index(idx, on);
            self.setlist.set_program_selects_song(idx, false);
        } else {
            return Err("song_of_program needs device or device_port".into());
        }
        let _ = crate::config::save_show_controls(&self.config_show_controls);
        Ok(())
    }

    pub(super) fn promote_session_show_controls_to_config(&mut self) {
        let old = std::mem::take(&mut self.setlist.controls);
        for c in old {
            if c.action == ShowAction::SongOfProgram {
                if let Some(d) = c.device {
                    let _ = self.set_permanent_program_device(Some(d), None, true);
                }
            } else {
                self.persist_show_control(c.action, c.device, None, c.channel, c.msg);
            }
        }
    }

    pub(super) fn clear_session_show_overrides(&mut self) {
        self.setlist.controls.clear();
    }

    pub(super) fn apply_show_control_action(&mut self, act: ShowControlAction) {
        match act {
            ShowControlAction::ToggleLearn(action) => {
                self.show_learn = if self.show_learn == Some(action) {
                    None
                } else {
                    if self.show_control_edit.as_ref().is_some_and(|e| e.action == action) {
                        self.show_control_edit = None;
                    }
                    Some(action)
                };
            }
            ShowControlAction::OpenEdit(action) => {
                if self.show_control_edit.as_ref().is_some_and(|e| e.action == action) {
                    self.show_control_edit = None;
                    return;
                }
                self.show_learn = None;
                let edit = self.seed_show_control_edit(action);
                self.show_control_edit = Some(edit);
            }
            ShowControlAction::CancelEdit => {
                self.show_control_edit = None;
            }
            ShowControlAction::SaveEdit => {
                let Some(mut edit) = self.show_control_edit.take() else { return };
                match parse_edit_msg(edit.kind, &edit.number_text) {
                    Ok(msg) => {
                        let displaced = self.persist_show_control(
                            edit.action,
                            edit.device_index,
                            edit.device_port.clone(),
                            edit.channel,
                            msg,
                        );
                        let what = crate::setlist::msg_label(msg);
                        let note = match displaced {
                            Some(other) => format!(
                                "Saved {what} for {} (no longer {})",
                                edit.action.label(),
                                other.label()
                            ),
                            None => format!("Saved {what} for {}", edit.action.label()),
                        };
                        self.settings_msg = note.clone();
                        self.session_msg = note;
                    }
                    Err(e) => {
                        edit.error = Some(format!("{e}."));
                        edit.focus = true;
                        self.show_control_edit = Some(edit);
                    }
                }
            }
            ShowControlAction::Clear(action) => {
                self.clear_permanent_show_control(action);
                if self.show_control_edit.as_ref().is_some_and(|e| e.action == action) {
                    self.show_control_edit = None;
                }
                let msg = format!("Cleared {} control", action.label());
                self.settings_msg = msg.clone();
                self.session_msg = msg;
            }
            ShowControlAction::ToggleProgramPort { device, port, on } => {
                match self.set_permanent_program_device(device, port.clone(), on) {
                    Ok(()) => {
                        let name = port
                            .or_else(|| device.map(|d| self.hw_device_label(d)))
                            .unwrap_or_else(|| "Device".into());
                        self.settings_msg = if on {
                            format!("{name}: program change now picks the song")
                        } else {
                            format!("{name}: program change no longer picks the song")
                        };
                    }
                    Err(e) => {
                        self.settings_msg = format!("Show control failed: {e}");
                    }
                }
            }
            ShowControlAction::PromoteSessionOverrides => {
                self.promote_session_show_controls_to_config();
                self.settings_msg = "Saved session show control overrides to config.json".into();
            }
            ShowControlAction::ClearSessionOverrides => {
                self.clear_session_show_overrides();
                self.settings_msg = "Cleared per-session show control overrides".into();
            }
        }
    }

    fn seed_show_control_edit(&self, action: ShowAction) -> ShowControlEdit {
        let existing_saved = self.config_show_controls.binding_of(action);
        let existing_eff = self.effective_show_control(action);
        let (kind, number_text, channel, device_port, device_index) = if let Some(eff) = existing_eff {
            let (kind, text) = match eff.msg {
                ShowMsg::Cc(n) => (MsgKind::Cc, n.to_string()),
                ShowMsg::Note(n) => (MsgKind::Note, crate::setlist::note_label(n)),
                ShowMsg::Program(n) => (MsgKind::Program, n.to_string()),
                ShowMsg::AnyProgram => (MsgKind::Cc, "80".into()),
            };
            let port = eff.device.and_then(|d| {
                self.hw_devices
                    .get(d as usize)
                    .map(|raw| midi_port_key(raw).to_string())
            });
            let idx = if port.is_none() { eff.device } else { None };
            (kind, text, eff.channel, port, idx)
        } else if let Some(saved) = existing_saved {
            let (kind, text) = match saved.msg {
                ShowMsg::Cc(n) => (MsgKind::Cc, n.to_string()),
                ShowMsg::Note(n) => (MsgKind::Note, crate::setlist::note_label(n)),
                ShowMsg::Program(n) => (MsgKind::Program, n.to_string()),
                ShowMsg::AnyProgram => (MsgKind::Cc, "80".into()),
            };
            (kind, text, saved.channel, saved.device.clone(), saved.device_index)
        } else {
            (MsgKind::Cc, "80".into(), None, None, None)
        };
        ShowControlEdit {
            action,
            kind,
            number_text,
            channel,
            device_port,
            device_index,
            error: None,
            focus: true,
        }
    }

    /// A hardware message the app sees after the engine took it: learned
    /// for the armed action on a press, or one a show control takes. True
    /// when the message is a show control's, so no key, knob or knob learn
    /// moves on it (the engine gave it to no instrument).
    pub(super) fn show_control_msg(
        &mut self,
        source: SourceId,
        channel: u8,
        msg: ShowMsg,
        press: bool,
    ) -> bool {
        let SourceId::Hardware(device) = source else {
            return false;
        };
        if let Some(action) = self.show_learn.filter(|_| press) {
            if let ShowMsg::Cc(cc) = msg {
                if self.cc_chatter.is_chattering(source, cc, self.ui_time) {
                    return false;
                }
            }
            self.show_learn = None;
            let what = crate::setlist::msg_label(msg);
            let note = match self.persist_show_control(action, Some(device), None, None, msg) {
                Some(other) => format!("Learned {what} for {} (no longer {})", action.label(), other.label()),
                None => format!("Learned {what} for {}", action.label()),
            };
            self.session_msg = note.clone();
            self.settings_msg = note.clone();
            self.notify_patch(&note);
            return true;
        }
        self.effective_show_controls()
            .iter()
            .flatten()
            .any(|c| c.takes(device, channel, msg))
    }

    /// Do what the engine counted presses of and only the app can do: the
    /// tuner, recording, and Show mode; a Panic or a tap also shows here.
    pub(super) fn follow_show_presses(&mut self) {
        let Some(p) = &self.graph_pub else { return };
        let Ok(g) = p.lock() else { return };
        let now = g.show_presses;
        drop(g);
        let seen = std::mem::replace(&mut self.show_presses_seen, now);
        let moved = |a: ShowAction| now[a as usize].wrapping_sub(seen[a as usize]);
        if moved(ShowAction::Tuner) % 2 == 1 {
            self.tuner_on = !self.tuner_on;
        }
        if moved(ShowAction::Record) % 2 == 1 {
            self.toggle_recording();
        }
        if moved(ShowAction::ShowMode) % 2 == 1 {
            let next = self.show_mode.is_none();
            self.set_show_mode(next);
        }
        if moved(ShowAction::Panic) > 0 {
            self.panicked();
        }
        if moved(ShowAction::MasterMute) > 0 {
            self.ui_master_mute_grace = -1.0;
        }
        if moved(ShowAction::TapTempo) > 0 {
            self.tap_flash_t = 1.0;
            self.ui_transport_grace = self.ui_time + 0.6;
        }
    }

    /// Describe what a Show action currently maps to for UI rows.
    fn describe_action_binding(
        &self,
        action: ShowAction,
        effective: &crate::show::ShowControls,
    ) -> (bool, String) {
        let session_override = self.setlist.control_of(action).is_some();
        if let Some(c) = effective
            .iter()
            .flatten()
            .find(|c| c.action == action && c.action != ShowAction::SongOfProgram)
        {
            let dev_name = c.device.map(|d| self.hw_device_label(d));
            return (
                true,
                format_control_label(c.msg, c.channel, dev_name.as_deref(), false, session_override),
            );
        }
        if let Some(saved) = self.config_show_controls.binding_of(action) {
            return (
                true,
                format_control_label(
                    saved.msg,
                    saved.channel,
                    saved.device.as_deref(),
                    saved.device.is_some(),
                    false,
                ),
            );
        }
        (false, "Unbound".to_string())
    }

    /// The Setlist view's show controls: every action with its control and
    /// Learn / Clear, then each device whose program change can pick the song.
    pub(super) fn show_controls_rows(
        ui: &mut egui::Ui,
        rows: &[(ShowAction, bool, String)],
        learn: Option<ShowAction>,
        devices: &[(Option<u32>, Option<String>, String, bool, bool)],
        actions: &mut Vec<ShowControlAction>,
        open_settings: &mut bool,
    ) {
        let narrow = ui.available_width() < 300.0;
        ui.horizontal(|ui| {
            theme::section_label(ui, "Show controls");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::chip_button(ui, "Settings →", false)
                    .on_hover_text("Configure Show mode options and edit permanent MIDI controls")
                    .clicked()
                {
                    *open_settings = true;
                }
            });
        });
        ui.add_space(6.0);
        for (action, has_binding, bound_text) in rows {
            let action = *action;
            let has_binding = *has_binding;
            let learning = learn == Some(action);
            let (text, color) = if learning {
                ("Press a control…", theme::ACCENT_BRIGHT)
            } else if has_binding {
                (bound_text.as_str(), theme::INK)
            } else {
                (bound_text.as_str(), theme::INK_2)
            };
            let draw_row = |ui: &mut egui::Ui, actions: &mut Vec<ShowControlAction>| {
                ui.horizontal(|ui| {
                    ui.set_min_height(ROW_H - 4.0);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        // Learn keeps its column whether or not there is a Clear; while
                        // learning, Cancel replaces Learn and uses the room for the prompt.
                        if has_binding && !learning {
                            if theme::chip_button(ui, "Clear", false).clicked() {
                                actions.push(ShowControlAction::Clear(action));
                            }
                        } else if !learning {
                            ui.add_space(theme::chip_button_width(ui, "Clear") + 4.0);
                        }
                        if theme::chip_button(ui, if learning { "Cancel" } else { "Learn" }, learning).clicked() {
                            actions.push(ShowControlAction::ToggleLearn(action));
                        }
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            if narrow {
                                ui.add(egui::Label::new(egui::RichText::new(action.label()).size(12.0).color(theme::INK)).truncate());
                            } else {
                                ui.allocate_ui_with_layout(
                                    Vec2::new(ACTION_W, ROW_H - 4.0),
                                    egui::Layout::left_to_right(egui::Align::Center),
                                    |ui| {
                                        ui.set_min_width(ACTION_W);
                                        ui.add(egui::Label::new(egui::RichText::new(action.label()).size(12.0).color(theme::INK)).truncate());
                                    },
                                );
                                ui.add(egui::Label::new(egui::RichText::new(text).size(11.5).color(color)).truncate())
                                    .on_hover_text(text);
                            }
                        });
                    });
                });
            };
            if narrow {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    draw_row(ui, actions);
                    ui.horizontal(|ui| {
                        ui.add_space(8.0);
                        ui.add(egui::Label::new(egui::RichText::new(text).size(11.0).color(color)).truncate())
                            .on_hover_text(text);
                    });
                    ui.add_space(2.0);
                });
            } else {
                draw_row(ui, actions);
            }
        }
        ui.add_space(6.0);
        ui.label(egui::RichText::new(ShowAction::SongOfProgram.label()).size(12.0).color(theme::INK));
        if devices.is_empty() {
            ui.label(
                egui::RichText::new("Plug in a MIDI device to pick songs with its program buttons.")
                    .size(11.5)
                    .color(theme::INK_2),
            );
        }
        for (d, port, name, live, on) in devices {
            let active_count = devices.iter().filter(|r| r.4).count();
            let full = !*on && active_count >= crate::setlist::MAX_PROGRAM_DEVICES;
            ui.horizontal(|ui| {
                ui.set_min_height(ROW_H - 4.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let hint = if full {
                        format!("At most {} devices pick songs", crate::setlist::MAX_PROGRAM_DEVICES)
                    } else {
                        "Program N selects song N+1; this device's program changes no longer reach its instruments".to_string()
                    };
                    let chip = ui.add_enabled_ui(!full, |ui| theme::chip_button(ui, if *on { "On" } else { "Off" }, *on)).inner;
                    if chip.on_hover_text(hint).clicked() {
                        actions.push(ShowControlAction::ToggleProgramPort {
                            device: *d,
                            port: port.clone(),
                            on: !*on,
                        });
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        let color = if *live { theme::INK } else { theme::INK_2 };
                        ui.add(egui::Label::new(egui::RichText::new(name).size(12.0).color(color)).truncate())
                            .on_hover_text(name.as_str());
                    });
                });
            });
        }
    }

    pub(super) fn show_control_rows_data(&self) -> Vec<(ShowAction, bool, String)> {
        let effective = self.effective_show_controls();
        ShowAction::LEARNED
            .iter()
            .map(|&a| {
                let (has, label) = self.describe_action_binding(a, &effective);
                (a, has, label)
            })
            .collect()
    }

    pub(super) fn show_program_rows_data(&self) -> Vec<(Option<u32>, Option<String>, String, bool, bool)> {
        let mut rows = Vec::new();
        let mut seen_ports: Vec<String> = Vec::new();
        for (idx, raw) in self.hw_devices.iter().enumerate() {
            let d = idx as u32;
            let port = midi_port_key(raw).to_string();
            seen_ports.push(port.clone());
            let live = self.hw_live.get(idx).copied().unwrap_or(true);
            let on = self.effective_program_selects_song(d);
            rows.push((Some(d), Some(port), self.hw_device_label(d), live, on));
        }
        for port in &self.config_show_controls.program_devices {
            if !seen_ports.iter().any(|p| p == port) {
                rows.push((None, Some(port.clone()), format!("{port} (offline)"), false, true));
            }
        }
        rows
    }

    /// `Settings → Show` (#437): Show mode behavior/display options, permanent
    /// MIDI Show controls with Learn and manual CC/Note/Program editor, and
    /// ALSA Midi Through toggle.
    pub(super) fn settings_show_page(&mut self, ui: &mut egui::Ui, actions: &mut Vec<SettingsAction>) {
        // 1. Show mode preferences
        let hold = self.show_mode_cfg.leave_hold_secs;
        let fmt_hold = |s: f32| {
            if (s - s.round()).abs() < 0.05 {
                format!("{s:.1} s")
            } else {
                format!("{s:.2} s")
            }
        };
        Self::settings_row(
            ui,
            "Exit hold time",
            "How long Esc must be held before releasing leaves Show mode.",
            |ui| {
                let dd_w = (ui.available_width() - 8.0).clamp(110.0, 180.0);
                theme::Dropdown::new("dd_settings_show_hold", &fmt_hold(hold))
                    .width(dd_w)
                    .popup_min_width(140.0)
                    .text_size(12.0)
                    .show_ui(ui, |menu| {
                        for &secs in crate::config::LEAVE_HOLD_CHOICES {
                            if menu.item((hold - secs).abs() < 0.05, &fmt_hold(secs)) {
                                actions.push(SettingsAction::SetShowLeaveHold(secs));
                            }
                        }
                    });
            },
        );
        Self::settings_gap(ui);

        let clock_24h = self.show_mode_cfg.clock_24h;
        let clock_label = if clock_24h { "24-hour" } else { "12-hour" };
        Self::settings_row(
            ui,
            "Clock format",
            "How the stage clock is shown at the top right of Show mode.",
            |ui| {
                let dd_w = (ui.available_width() - 8.0).clamp(110.0, 180.0);
                theme::Dropdown::new("dd_settings_show_clock", clock_label)
                    .width(dd_w)
                    .popup_min_width(140.0)
                    .text_size(12.0)
                    .show_ui(ui, |menu| {
                        if menu.item(clock_24h, "24-hour") && !clock_24h {
                            actions.push(SettingsAction::ToggleShowClock24h);
                        }
                        if menu.item(!clock_24h, "12-hour") && clock_24h {
                            actions.push(SettingsAction::ToggleShowClock24h);
                        }
                    });
            },
        );
        Self::settings_gap(ui);

        let show_notes = self.show_mode_cfg.show_notes;
        Self::settings_row(
            ui,
            "Song notes & charts",
            "Shows each song's cues, lyrics, or ChordPro chart panel in Show mode.",
            |ui| {
                if theme::chip_button(ui, if show_notes { "On" } else { "Off" }, show_notes).clicked() {
                    actions.push(SettingsAction::ToggleShowNotes);
                }
            },
        );
        Self::settings_gap(ui);

        let show_meters = self.show_mode_cfg.show_rig_meters;
        Self::settings_row(
            ui,
            "Rig meters",
            "Shows live per-rig meters beside the Master meter in Show mode.",
            |ui| {
                if theme::chip_button(ui, if show_meters { "On" } else { "Off" }, show_meters).clicked() {
                    actions.push(SettingsAction::ToggleShowRigMeters);
                }
            },
        );
        Self::settings_gap(ui);

        let start_show = self.show_mode_cfg.start_in_show_mode;
        Self::settings_row(
            ui,
            "Start in Show mode",
            "Opens directly onto the locked stage screen when kabelsalat launches.",
            |ui| {
                if theme::chip_button(ui, if start_show { "On" } else { "Off" }, start_show).clicked() {
                    actions.push(SettingsAction::ToggleStartInShowMode);
                }
            },
        );
        Self::settings_gap(ui);

        // 2. Permanent MIDI show controls
        theme::section_label(ui, "Show controls");
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new("MIDI CC, Note, or Program Change bindings for Show mode and setlist stepping. Saved permanently by MIDI port name in config.json.")
                .size(11.5)
                .color(theme::INK_2),
        );
        ui.add_space(8.0);

        if self.has_session_show_overrides() {
            egui::Frame::new()
                .fill(theme::DARK_PANEL_HI)
                .stroke(Stroke::new(1.0, theme::HAIRLINE))
                .corner_radius(3.0)
                .inner_margin(egui::Margin::symmetric(10, 6))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let gap = ui.spacing().item_spacing.x;
                        let btns_w = theme::chip_button_width(ui, "Save to config")
                            + theme::chip_button_width(ui, "Clear overrides")
                            + gap * 2.0;
                        let txt_w = (ui.available_width() - btns_w).max(60.0);
                        let g = Self::ellipsized(
                            ui.painter(),
                            "This session overrides some show controls.",
                            FontId::proportional(11.5),
                            theme::ACCENT_BRIGHT,
                            txt_w,
                        );
                        let (r, _) = ui.allocate_exact_size(egui::vec2(txt_w, 22.0), egui::Sense::hover());
                        ui.painter().galley(Pos2::new(r.left(), r.center().y - g.size().y * 0.5), g, theme::ACCENT_BRIGHT);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if theme::chip_button(ui, "Clear overrides", false).clicked() {
                                actions.push(SettingsAction::ShowControl(ShowControlAction::ClearSessionOverrides));
                            }
                            if theme::chip_button(ui, "Save to config", false).clicked() {
                                actions.push(SettingsAction::ShowControl(ShowControlAction::PromoteSessionOverrides));
                            }
                        });
                    });
                });
            ui.add_space(8.0);
        }

        let rows = self.show_control_rows_data();
        let learn = self.show_learn;
        let editing_action = self.show_control_edit.as_ref().map(|e| e.action);
        for (action, has_binding, bound_text) in rows {
            let learning = learn == Some(action);
            let editing = editing_action == Some(action);
            let (shown, col) = if learning {
                ("Press a control…".to_string(), theme::ACCENT_BRIGHT)
            } else if has_binding {
                (bound_text, theme::CREAM)
            } else {
                (bound_text, theme::CREAM_DIM)
            };
            ui.horizontal(|ui| {
                ui.set_min_height(26.0);
                let gap = ui.spacing().item_spacing.x;
                let clear_slot_w = theme::chip_button_width(ui, "Clear");
                let has_clear = has_binding && !learning;
                let clear_w = if !learning { clear_slot_w + gap } else { 0.0 };
                let edit_lbl = if editing { "Close" } else { "Edit" };
                let edit_w = if !learning {
                    theme::chip_button_width(ui, edit_lbl) + gap
                } else {
                    0.0
                };
                let learn_lbl = if learning { "Cancel" } else { "Learn" };
                let learn_w = theme::chip_button_width(ui, learn_lbl);
                let btns_w = clear_w + edit_w + learn_w + 6.0;
                let left_w = (ui.available_width() - btns_w).max(60.0);
                let label_w = (left_w * 0.42).clamp(76.0, 124.0);
                let val_w = (left_w - label_w - gap).max(32.0);

                let lg = Self::ellipsized(ui.painter(), action.label(), FontId::proportional(12.5), theme::INK, label_w);
                let (lr, _) = ui.allocate_exact_size(egui::vec2(label_w, 24.0), egui::Sense::hover());
                ui.painter().galley(Pos2::new(lr.left(), lr.center().y - lg.size().y * 0.5), lg, theme::INK);

                let vg = Self::ellipsized(ui.painter(), &shown, FontId::proportional(11.5), col, val_w);
                let (vr, vresp) = ui.allocate_exact_size(egui::vec2(val_w, 24.0), egui::Sense::hover());
                ui.painter().galley(Pos2::new(vr.left(), vr.center().y - vg.size().y * 0.5), vg, col);
                vresp.on_hover_text(shown.as_str());

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if has_clear {
                        if theme::chip_button(ui, "Clear", false).clicked() {
                            actions.push(SettingsAction::ShowControl(ShowControlAction::Clear(action)));
                        }
                    } else if !learning {
                        ui.add_space(clear_slot_w + gap);
                    }
                    if !learning && theme::chip_button(ui, edit_lbl, editing).clicked() {
                        actions.push(SettingsAction::ShowControl(ShowControlAction::OpenEdit(action)));
                    }
                    if theme::chip_button(ui, learn_lbl, learning).clicked() {
                        actions.push(SettingsAction::ShowControl(ShowControlAction::ToggleLearn(action)));
                    }
                });
            });

            if editing {
                self.show_control_edit_panel(ui, actions);
            }
        }

        ui.add_space(10.0);
        ui.label(egui::RichText::new(ShowAction::SongOfProgram.label()).font(theme::medium_font(12.5)).color(theme::INK));
        ui.add_space(4.0);
        let prog_rows = self.show_program_rows_data();
        if prog_rows.is_empty() {
            ui.label(
                egui::RichText::new("Plug in a MIDI device to pick songs with its program buttons.")
                    .size(11.5)
                    .color(theme::INK_2),
            );
        } else {
            let active_count = prog_rows.iter().filter(|r| r.4).count();
            for (d, port, name, live, on) in prog_rows {
                let full = !on && active_count >= crate::setlist::MAX_PROGRAM_DEVICES;
                ui.horizontal(|ui| {
                    ui.set_min_height(26.0);
                    let gap = ui.spacing().item_spacing.x;
                    let on_lbl = if on { "On" } else { "Off" };
                    let btn_w = theme::chip_button_width(ui, on_lbl) + gap + 4.0;
                    let name_w = (ui.available_width() - btn_w).max(60.0);
                    let col = if live { theme::CREAM } else { theme::CREAM_DIM };
                    let g = Self::ellipsized(ui.painter(), &name, FontId::proportional(12.0), col, name_w);
                    let (r, resp) = ui.allocate_exact_size(egui::vec2(name_w, 24.0), egui::Sense::hover());
                    ui.painter().galley(Pos2::new(r.left(), r.center().y - g.size().y * 0.5), g, col);
                    resp.on_hover_text(name.as_str());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let chip = ui
                            .add_enabled_ui(!full, |ui| theme::chip_button(ui, on_lbl, on))
                            .inner;
                        if chip.clicked() {
                            actions.push(SettingsAction::ShowControl(ShowControlAction::ToggleProgramPort {
                                device: d,
                                port,
                                on: !on,
                            }));
                        }
                    });
                });
            }
        }

        // 3. MIDI & config info
        Self::settings_gap(ui);
        let through_on = self.midi_through_pref;
        let through_env = std::env::var_os("KABELSALAT_MIDI_THROUGH").is_some();
        let through_desc = if through_env {
            "Lists ALSA's kernel Midi Through port as a hardware port (overridden by KABELSALAT_MIDI_THROUGH)."
        } else {
            "Lists ALSA's kernel Midi Through port alongside hardware controllers for loopback or virtual routing."
        };
        Self::settings_row(ui, "ALSA Midi Through", through_desc, |ui| {
            if theme::chip_button(ui, if through_on { "On" } else { "Off" }, through_on).clicked() {
                actions.push(SettingsAction::ToggleMidiThrough);
            }
        });
        Self::settings_gap(ui);
        let cfg_display = crate::plugin_dirs::display(&crate::assistant::config_path());
        Self::settings_row(
            ui,
            "Config file",
            "Permanent settings and show controls are stored in this JSON file.",
            |ui| {
                ui.label(egui::RichText::new(cfg_display).size(11.5).color(theme::CREAM_META));
            },
        );
    }

    fn show_control_edit_panel(&mut self, ui: &mut egui::Ui, actions: &mut Vec<SettingsAction>) {
        // Collect device options before borrowing `self.show_control_edit`.
        let mut device_opts: Vec<(Option<String>, String)> = vec![(None, "Any device".to_string())];
        for (i, raw) in self.hw_devices.iter().enumerate() {
            let key = midi_port_key(raw).to_string();
            if !device_opts.iter().any(|(k, _)| k.as_deref() == Some(key.as_str())) {
                device_opts.push((Some(key), self.hw_device_label(i as u32)));
            }
        }
        if let Some(port) = self
            .show_control_edit
            .as_ref()
            .and_then(|e| e.device_port.as_ref())
        {
            if !device_opts.iter().any(|(k, _)| k.as_deref() == Some(port.as_str())) {
                device_opts.push((Some(port.clone()), format!("{port} (offline)")));
            }
        }

        let Some(edit) = self.show_control_edit.as_mut() else {
            return;
        };
        let (mut save, mut cancel) = (false, false);
        egui::Frame::new()
            .fill(theme::DARK_PANEL_HI)
            .stroke(Stroke::new(1.0, theme::HAIRLINE))
            .corner_radius(3.0)
            .inner_margin(egui::Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    theme::Dropdown::new(("dd_show_ctrl_kind", edit.action.id()), edit.kind.label())
                        .width(86.0)
                        .popup_min_width(110.0)
                        .text_size(12.0)
                        .show_ui(ui, |menu| {
                            for k in MsgKind::ALL {
                                if menu.item(edit.kind == k, k.label()) {
                                    edit.kind = k;
                                }
                            }
                        });
                    let hint = match edit.kind {
                        MsgKind::Cc | MsgKind::Program => "0–127",
                        MsgKind::Note => "C2 or 0–127",
                    };
                    let field = egui::Frame::new()
                        .fill(theme::PAPER_SINK)
                        .stroke(Stroke::new(1.0, theme::HAIRLINE))
                        .corner_radius(2.0)
                        .inner_margin(egui::Margin::symmetric(8, 5))
                        .show(ui, |ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut edit.number_text)
                                    .id_salt(("show_ctrl_num", edit.action.id()))
                                    .frame(egui::Frame::NONE)
                                    .desired_width(72.0)
                                    .hint_text(hint),
                            )
                        })
                        .inner;
                    if std::mem::take(&mut edit.focus) {
                        field.request_focus();
                    }
                    if field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        save = true;
                    }
                    let ch_label = match edit.channel {
                        None => "Any ch".to_string(),
                        Some(c) => format!("Ch {}", c + 1),
                    };
                    theme::Dropdown::new(("dd_show_ctrl_ch", edit.action.id()), &ch_label)
                        .width(84.0)
                        .popup_min_width(100.0)
                        .text_size(12.0)
                        .show_ui(ui, |menu| {
                            if menu.item(edit.channel.is_none(), "Any ch") {
                                edit.channel = None;
                            }
                            for c in 0..16u8 {
                                if menu.item(edit.channel == Some(c), &format!("Ch {}", c + 1)) {
                                    edit.channel = Some(c);
                                }
                            }
                        });
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let gap = ui.spacing().item_spacing.x;
                    let btns_w = theme::primary_button_width(ui, "Save")
                        + theme::chip_button_width(ui, "Cancel")
                        + gap * 2.0
                        + 4.0;
                    let dd_w = (ui.available_width() - btns_w).clamp(100.0, 320.0);
                    let dev_label = device_opts
                        .iter()
                        .find(|(k, _)| *k == edit.device_port)
                        .map(|(_, l)| l.clone())
                        .unwrap_or_else(|| "Any device".into());
                    theme::Dropdown::new(("dd_show_ctrl_dev", edit.action.id()), &dev_label)
                        .width(dd_w)
                        .popup_min_width(200.0)
                        .text_size(12.0)
                        .show_ui(ui, |menu| {
                            for (key, lbl) in &device_opts {
                                if menu.item(&edit.device_port == key, lbl) {
                                    edit.device_port = key.clone();
                                    edit.device_index = None;
                                }
                            }
                        });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::chip_button(ui, "Cancel", false).clicked() {
                            cancel = true;
                        }
                        if theme::primary_button(ui, "Save").clicked() {
                            save = true;
                        }
                    });
                });
                if let Some(err) = &edit.error {
                    ui.add_space(4.0);
                    ui.add(egui::Label::new(egui::RichText::new(err).size(11.5).color(theme::CLIP_BRIGHT)).wrap());
                }
            });
        ui.add_space(4.0);
        if cancel {
            actions.push(SettingsAction::ShowControl(ShowControlAction::CancelEdit));
        } else if save {
            actions.push(SettingsAction::ShowControl(ShowControlAction::SaveEdit));
        }
    }

    /// `get_show_controls`: every action, its control (including offline
    /// global bindings and scope), and the devices that can pick songs.
    pub(super) fn show_controls_json(&self) -> serde_json::Value {
        let effective = self.effective_show_controls();
        let actions: Vec<serde_json::Value> = ShowAction::LEARNED
            .iter()
            .map(|&a| {
                let session_override = self.setlist.control_of(a).is_some();
                let scope = if session_override { "session" } else { "global" };
                let ctrl = if let Some(c) = effective
                    .iter()
                    .flatten()
                    .find(|c| c.action == a && c.action != ShowAction::SongOfProgram)
                {
                    let mut v = crate::setlist::control_json(*c, &|d| self.hw_device_label(d));
                    if let Some(m) = v.as_object_mut() {
                        m.insert("scope".into(), serde_json::json!(scope));
                        m.insert("connected".into(), serde_json::json!(true));
                        if let Some(port) = c
                            .device
                            .and_then(|d| self.hw_devices.get(d as usize))
                            .map(|raw| midi_port_key(raw))
                        {
                            m.insert("device_port".into(), serde_json::json!(port));
                        }
                    }
                    v
                } else if let Some(saved) = self.config_show_controls.binding_of(a) {
                    let mut v = serde_json::to_value(saved.msg).unwrap_or(serde_json::Value::Null);
                    if let Some(m) = v.as_object_mut() {
                        m.insert("label".into(), serde_json::json!(crate::setlist::msg_label(saved.msg)));
                        m.insert("device".into(), serde_json::Value::Null);
                        m.insert("device_port".into(), serde_json::json!(saved.device));
                        m.insert("device_name".into(), serde_json::json!(saved.device));
                        m.insert("connected".into(), serde_json::json!(false));
                        m.insert("scope".into(), serde_json::json!("global"));
                        if let Some(ch) = saved.channel {
                            m.insert("channel".into(), serde_json::json!(ch));
                        }
                    }
                    v
                } else {
                    serde_json::Value::Null
                };
                let row_scope = if session_override {
                    serde_json::json!("session")
                } else if !ctrl.is_null() {
                    serde_json::json!("global")
                } else {
                    serde_json::Value::Null
                };
                serde_json::json!({
                    "action": a.id(),
                    "label": a.label(),
                    "scope": row_scope,
                    "control": ctrl,
                })
            })
            .collect();

        let mut listed: Vec<u32> = (0..self.hw_devices.len() as u32)
            .chain(self.setlist.program_devices())
            .chain(self.config_show_controls.program_device_indices.iter().copied())
            .collect();
        listed.sort_unstable();
        listed.dedup();
        let mut programs: Vec<serde_json::Value> = Vec::new();
        let mut seen_ports: Vec<String> = Vec::new();
        for d in listed {
            let port = self
                .hw_devices
                .get(d as usize)
                .map(|raw| midi_port_key(raw).to_string());
            if let Some(p) = &port {
                seen_ports.push(p.clone());
            }
            let live = self.hw_live.get(d as usize).copied().unwrap_or(false);
            let mut row = serde_json::json!({
                "device": d,
                "name": self.hw_device_label(d),
                "on": self.effective_program_selects_song(d),
                "connected": live,
            });
            if let Some(p) = port {
                row["device_port"] = serde_json::json!(p);
            }
            programs.push(row);
        }
        for port in &self.config_show_controls.program_devices {
            if !seen_ports.iter().any(|p| p == port) {
                programs.push(serde_json::json!({
                    "device": serde_json::Value::Null,
                    "device_port": port,
                    "name": port,
                    "on": true,
                    "connected": false,
                }));
            }
        }
        let used = effective.iter().flatten().count();
        serde_json::json!({
            "actions": actions,
            "song_of_program": programs,
            "slots": {"used": used, "max": crate::show::MAX_SHOW_CONTROLS},
            "session_overrides": self.has_session_show_overrides(),
        })
    }

    fn tuner_reading(&self) -> TunerReading {
        let Some(u) = self.units.iter().find(|u| matches!(u.node_kind, UnitKind::AudioIn)) else {
            return TunerReading::NoInput;
        };
        let pitch = self.audio_in_tuners.get(&u.node_id).and_then(|(_, _, p)| *p);
        let device = u
            .device
            .as_deref()
            .map_or_else(|| "default input".to_string(), crate::audio_in::pretty_device_name);
        TunerReading::Input {
            name: format!("{} · {device}", sentence_case(&u.name)),
            pitch,
        }
    }

    /// The tuner a show control turns on (#277), over whatever screen is
    /// open: the note the first Audio In plays and how far off it is.
    /// Clicking it closes it.
    pub(super) fn tuner_panel(&mut self, ctx: &egui::Context) {
        if !self.tuner_on {
            self.tuner_rect = Rect::NOTHING;
            return;
        }
        let reading = self.tuner_reading();
        let (anchor, offset) = if self.show_mode.is_some() {
            (egui::Align2::RIGHT_TOP, Vec2::new(-48.0, 108.0))
        } else {
            (egui::Align2::CENTER_TOP, Vec2::new(0.0, 72.0))
        };
        let area = egui::Area::new(egui::Id::new("show_tuner"))
            .order(egui::Order::Foreground)
            .anchor(anchor, offset)
            .constrain(true);
        let res = area.show(ctx, |ui| {
            egui::Frame::new()
                .fill(theme::PAPER_SINK)
                .stroke(Stroke::new(1.5, theme::MUTED))
                .corner_radius(8.0)
                .shadow(egui::Shadow {
                    offset: [0, 8],
                    blur: 24,
                    spread: 2,
                    color: Color32::from_black_alpha(120),
                })
                .show(ui, |ui| {
                    let (rect, resp) = ui.allocate_exact_size(TUNER_SIZE, egui::Sense::click());
                    let p = ui.painter_at(rect);
                    let inner = rect.shrink2(Vec2::new(22.0, 16.0));
                    let cap = p.fonts_mut(|f| theme::layout_tracked(f, "TUNER", FontId::proportional(12.0), 0.14));
                    theme::paint_tracked(&p, inner.left_top(), &cap, theme::CREAM_DIM);
                    let close = p.layout_no_wrap("click to close".into(), FontId::proportional(11.0), theme::CREAM_DIM);
                    p.galley(Pos2::new(inner.right() - close.size().x, inner.top()), close, theme::CREAM_DIM);
                    let (note, cents, line) = match &reading {
                        TunerReading::NoInput => (None, None, "No Audio In unit in the rack".to_string()),
                        TunerReading::Input { name, pitch: None } => (None, None, format!("{name} · play a note")),
                        TunerReading::Input { name, pitch: Some(pitch) } => {
                            (Some(crate::audio_in::note_name(pitch.note)), Some(pitch.cents), name.clone())
                        }
                    };
                    let in_tune = cents.is_some_and(|c| c.abs() <= IN_TUNE_CENTS);
                    let tone = if in_tune { theme::LIVE_TEAL } else { theme::CREAM };
                    let big = p.layout_no_wrap(note.unwrap_or_else(|| "–".into()), theme::medium_font(48.0), tone);
                    let big_top = inner.top() + cap.size.y + 6.0;
                    p.galley(Pos2::new(inner.left(), big_top), big.clone(), tone);
                    if let Some(c) = cents {
                        let txt = p.layout_no_wrap(format!("{c:+.0} ¢"), theme::medium_font(24.0), tone);
                        p.galley(
                            Pos2::new(inner.right() - txt.size().x, big_top + big.size().y - txt.size().y - 6.0),
                            txt,
                            tone,
                        );
                    }
                    let bar = Rect::from_min_size(
                        Pos2::new(inner.left(), big_top + big.size().y + 8.0),
                        Vec2::new(inner.width(), 8.0),
                    );
                    p.rect_filled(bar, 4.0, theme::METER_TRACK);
                    p.rect_filled(Rect::from_center_size(bar.center(), Vec2::new(2.0, 16.0)), 1.0, theme::CREAM_DIM);
                    if let Some(c) = cents {
                        let x = bar.center().x + (c / TUNER_SPAN_CENTS).clamp(-1.0, 1.0) * bar.width() / 2.0;
                        p.rect_filled(
                            Rect::from_center_size(Pos2::new(x, bar.center().y), Vec2::new(6.0, 20.0)),
                            3.0,
                            if in_tune { theme::LIVE_TEAL } else { theme::ACCENT_BRIGHT },
                        );
                    }
                    let mut job = egui::text::LayoutJob::single_section(
                        line,
                        egui::TextFormat::simple(FontId::proportional(12.0), theme::CREAM_DIM),
                    );
                    job.wrap = egui::text::TextWrapping::truncate_at_width(inner.width());
                    let line = p.layout_job(job);
                    p.galley(Pos2::new(inner.left(), inner.bottom() - line.size().y), line, theme::CREAM_DIM);
                    resp.clicked()
                })
                .inner
        });
        self.tuner_rect = res.response.rect;
        if res.inner {
            self.tuner_on = false;
            self.tuner_rect = Rect::NOTHING;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::{MsgKind, ShowControlAction};
    use crate::show::{ShowAction, ShowControl, ShowMsg};
    use std::sync::{Arc, Mutex};

    const PEDAL: u32 = 1;

    /// Tall enough for the whole list: in a shorter window it scrolls.
    const SIZE: Vec2 = Vec2::new(1280.0, 1900.0);

    fn frame(app: &mut DemoApp, ctx: &egui::Context) {
        let mut raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, SIZE)),
            ..Default::default()
        };
        raw.time = Some(app.ui_time as f64 + 1.0);
        let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
        out.textures_delta.clear();
    }

    /// An app with a gig of three songs, wired to a real engine the way the
    /// running app is: the MIDI thread sends to the engine first
    /// (`hw_direct`) and queues for the app, and what the app sends goes
    /// through `sent` on its way to the engine.
    struct Stage {
        app: DemoApp,
        ctx: egui::Context,
        engine: crate::engine::GraphEngine,
        pub_: Arc<Mutex<crate::engine::GraphPub>>,
        engine_bus: Arc<Mutex<crate::control::CommandBus>>,
        app_drain: crate::control::CommandDrain,
        sent: Vec<Command>,
        queues: HwQueues,
    }

    impl Stage {
        fn new() -> Self {
            let mut app = DemoApp::new_with_sink(None);
            for (name, n) in [("One", 2), ("Two", 1), ("Three", 1)] {
                let parts = (0..n)
                    .map(|i| crate::setlist::Part::new(format!("{name} {i}"), vec![crate::setlist::PartRig::new(0)]))
                    .collect();
                app.setlist.songs.push(crate::setlist::Song::new(name, parts));
            }
            let (tx, drain) = crate::control::command_bus();
            let mut engine = crate::engine::GraphEngine::new(app.base_graph.clone(), drain, 48000.0, vec![]);
            let pub_ = Arc::new(Mutex::new(crate::engine::GraphPub::default()));
            engine.set_graph_pub(pub_.clone());
            let engine_bus = Arc::new(Mutex::new(tx));
            *app.hw_direct.bus.lock().unwrap() = Some(engine_bus.clone());
            let (app_tx, app_drain) = crate::control::command_bus();
            app.bus = Some(Arc::new(Mutex::new(app_tx)));
            app.graph_pub = Some(pub_.clone());
            let queues = app.hw_queues();
            let mut s = Stage {
                app,
                ctx: egui::Context::default(),
                engine,
                pub_,
                engine_bus,
                app_drain,
                sent: Vec::new(),
                queues,
            };
            s.tick();
            s
        }

        /// One UI frame, then one engine block with what the frame sent.
        fn tick(&mut self) {
            frame(&mut self.app, &self.ctx);
            let mut sent = Vec::new();
            self.app_drain.drain(|c| sent.push(c));
            for c in &sent {
                self.engine_bus.lock().unwrap().send(c.clone());
            }
            self.sent.extend(sent);
            let mut buf = vec![0.0f32; 256 * 2];
            self.engine.render(&mut buf, 2);
        }

        /// Raw bytes from the pedal, as the MIDI thread hands them on; then
        /// the engine's block and the app's frame.
        fn midi(&mut self, bytes: &[u8]) {
            hw_midi_in(PEDAL, bytes, &self.queues, &self.app.hw_direct);
            let mut buf = vec![0.0f32; 256 * 2];
            self.engine.render(&mut buf, 2);
            self.tick();
        }

        fn tap(&mut self, press: &[u8]) {
            self.midi(press);
            match press[0] & 0xF0 {
                0x90 => self.midi(&[0x80 | (press[0] & 0x0F), press[1], 0]),
                0xB0 => self.midi(&[press[0], press[1], 0]),
                _ => {}
            }
        }

        fn presses(&self, a: ShowAction) -> u64 {
            self.pub_.lock().unwrap().show_presses[a as usize]
        }
    }

    /// The press of each action's control, as raw bytes: notes, CCs and
    /// program changes on several channels.
    fn press_of(i: usize) -> Vec<u8> {
        match i % 3 {
            0 => vec![0x90 | (i as u8 % 4), 36 + i as u8, 100],
            1 => vec![0xB0 | (i as u8 % 4), 80 + i as u8, 127],
            _ => vec![0xC0 | (i as u8 % 4), 10 + i as u8],
        }
    }

    #[test]
    fn every_action_is_learned_from_raw_midi_bytes_and_triggered_by_them() {
        let mut s = Stage::new();
        for (i, action) in ShowAction::LEARNED.into_iter().enumerate() {
            s.app.show_learn = Some(action);
            s.tap(&press_of(i));
            let c = s
                .app
                .effective_show_control(action)
                .unwrap_or_else(|| panic!("{action:?} learned nothing"));
            assert_eq!(
                (c.device, c.channel),
                (Some(PEDAL), None),
                "{action:?}: learned from the pedal, on any channel"
            );
        }
        let learned: Vec<ShowMsg> = ShowAction::LEARNED
            .iter()
            .map(|&a| s.app.effective_show_control(a).unwrap().msg)
            .collect();
        assert_eq!(
            learned[..3],
            [ShowMsg::Note(36), ShowMsg::Cc(81), ShowMsg::Program(12)],
            "a note, a CC and a program change each bind"
        );
        s.tick();
        assert!(
            s.sent.iter().any(|c| matches!(c, Command::SetShowControls { controls } if controls.iter().flatten().count() == ShowAction::LEARNED.len())),
            "the engine got every control"
        );

        s.app.select_setlist(0, 0).unwrap();
        s.tick();
        let at = |s: &Stage| s.app.setlist.active.map(|p| (p.song, p.part));
        let idx = |a: ShowAction| ShowAction::LEARNED.iter().position(|&x| x == a).unwrap();
        s.tap(&press_of(idx(ShowAction::NextPart)));
        assert_eq!(at(&s), Some((0, 1)), "next part");
        s.tap(&press_of(idx(ShowAction::NextSong)));
        assert_eq!(at(&s), Some((1, 0)), "next song");
        s.tap(&press_of(idx(ShowAction::PrevSong)));
        assert_eq!(at(&s), Some((0, 0)), "previous song, at its first part");
        s.tap(&press_of(idx(ShowAction::NextSong)));
        s.tap(&press_of(idx(ShowAction::PrevPart)));
        assert_eq!(at(&s), Some((0, 1)), "previous part, across the song boundary");

        s.app.notes_down[60] = 1;
        s.tap(&press_of(idx(ShowAction::Panic)));
        assert_eq!(s.presses(ShowAction::Panic), 1);
        assert_eq!(s.app.notes_down[60], 0, "the app's keys go dark with the engine's panic");

        for _ in 0..4 {
            s.tap(&press_of(idx(ShowAction::TapTempo)));
            let mut buf = vec![0.0f32; 256 * 2];
            for _ in 0..(48000 / 2 / 256) - 2 {
                s.engine.render(&mut buf, 2);
            }
        }
        s.tick();
        assert!((s.app.bpm - 120.0).abs() <= 2.0, "four taps half a second apart: {} BPM", s.app.bpm);

        s.tap(&press_of(idx(ShowAction::MasterMute)));
        assert!(s.pub_.lock().unwrap().master_mute);
        s.tap(&press_of(idx(ShowAction::Tuner)));
        assert!(s.app.tuner_on, "the tuner opens");
        s.tap(&press_of(idx(ShowAction::Tuner)));
        assert!(!s.app.tuner_on, "and closes");
        s.sent.clear();
        s.tap(&press_of(idx(ShowAction::Record)));
        assert!(s.sent.iter().any(|c| matches!(c, Command::RecordStart)), "the app starts recording: {:?}", s.sent);
        assert!(s.app.show_mode.is_none());
        s.tap(&press_of(idx(ShowAction::ShowMode)));
        assert!(s.app.show_mode.is_some(), "ShowMode control opens Show mode");
        s.tap(&press_of(idx(ShowAction::ShowMode)));
        assert!(s.app.show_mode.is_none(), "second ShowMode press leaves Show mode");
        s.tap(&press_of(idx(ShowAction::BackingTrack)));
        assert_eq!(s.presses(ShowAction::BackingTrack), 1, "the backing track's press reached the engine");
        for a in [ShowAction::NextMarker, ShowAction::PrevMarker, ShowAction::LoopSection, ShowAction::StopAtSection] {
            s.tap(&press_of(idx(a)));
        }
        for a in ShowAction::LEARNED {
            assert!(s.presses(a) >= 1, "{a:?} was never pressed");
        }
    }

    #[test]
    fn a_show_controls_messages_move_no_key_and_no_knob_learn() {
        let mut s = Stage::new();
        s.app.show_learn = Some(ShowAction::NextPart);
        // A CC release (val < 64) does not bind; the press (val >= 64) does.
        s.midi(&[0xB0, 64, 0]);
        assert_eq!(s.app.effective_show_control(ShowAction::NextPart), None, "CC release does not bind");
        assert_eq!(s.app.show_learn, Some(ShowAction::NextPart));
        s.midi(&[0x90, 36, 100]);
        assert_eq!(s.app.notes_down[36], 0, "the key a learn binds does not light");
        s.midi(&[0x80, 36, 0]);
        s.midi(&[0x90, 36, 100]);
        assert_eq!(s.app.notes_down[36], 0, "nor does its next press");
        s.midi(&[0x90, 37, 100]);
        assert_ne!(s.app.notes_down[37], 0, "another key plays as before");
        // Binding a key while it is held still clears its keyboard-dock LED on release.
        s.app.persist_show_control(ShowAction::Panic, Some(PEDAL), None, None, ShowMsg::Note(37));
        s.tick();
        s.midi(&[0x80, 37, 0]);
        assert_eq!(s.app.notes_down[37], 0, "releasing the newly learned key clears its dock LED");
    }

    #[test]
    fn master_mute_overrides_grace_and_tap_tempo_leaves_no_remote_agent_badge() {
        let mut s = Stage::new();
        s.app.persist_show_control(ShowAction::MasterMute, Some(PEDAL), None, None, ShowMsg::Cc(82));
        s.app.persist_show_control(ShowAction::TapTempo, Some(PEDAL), None, None, ShowMsg::Cc(83));
        s.tick();
        // Inside the Mixer's 0.6 s mute grace window, a pedal toggle still syncs.
        s.app.ui_master_mute_grace = s.app.ui_time + 0.6;
        s.tap(&[0xB0, 82, 127]);
        assert!(s.app.master_mute, "pedal mute syncs even during ui_master_mute_grace");
        // Pedal tap tempo updates BPM without recording a remote agent edit.
        s.app.agent_activity = AgentActivity::default();
        for _ in 0..3 {
            s.tap(&[0xB0, 83, 127]);
            let mut buf = vec![0.0f32; 256 * 2];
            for _ in 0..(48000 / 2 / 256) - 2 {
                s.engine.render(&mut buf, 2);
            }
        }
        s.tick();
        assert!((s.app.bpm - 120.0).abs() <= 2.0, "tapped BPM adopted: {}", s.app.bpm);
        assert!(s.app.agent_activity.log.is_empty(), "tap tempo does not fire a remote agent badge: {:?}", s.app.agent_activity.log);
    }

    fn texts(shapes: &[egui::epaint::ClippedShape]) -> Vec<String> {
        fn walk(shape: &Shape, out: &mut Vec<String>) {
            match shape {
                Shape::Text(t) => out.push(t.galley.text().to_owned()),
                Shape::Vec(v) => v.iter().for_each(|s| walk(s, out)),
                _ => {}
            }
        }
        let mut out = Vec::new();
        shapes.iter().for_each(|cs| walk(&cs.shape, &mut out));
        out
    }

    #[test]
    fn the_setlist_view_lists_every_action_and_the_tuner_shows_over_it() {
        let mut app = DemoApp::new_with_sink(None);
        app.stage_view = StageView::Setlist;
        app.tuner_on = true;
        let ctx = egui::Context::default();
        frame(&mut app, &ctx);
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, SIZE)),
            time: Some(5.0),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
        out.textures_delta.clear();
        let texts = texts(&out.shapes);
        for a in ShowAction::LEARNED {
            assert!(texts.iter().any(|t| t == a.label()), "{} is listed: {texts:?}", a.label());
        }
        assert!(texts.iter().any(|t| t == ShowAction::SongOfProgram.label()), "and the program devices' row");
        assert!(texts.iter().any(|t| t == "No Audio In unit in the rack"), "the tuner says what it has to tune: {texts:?}");
    }

    #[test]
    fn the_show_control_tools_are_in_every_table_and_cover_program_devices() {
        let tools = crate::assistant::tools();
        let names: Vec<&str> = tools.as_array().unwrap().iter().filter_map(|t| t["function"]["name"].as_str()).collect();
        for name in ["get_show_controls", "set_show_control", "get_config", "set_config"] {
            assert!(names.contains(&name), "assistant tools miss {name}");
            assert!(UI_BRIDGE_TOOLS.contains(&name), "UI_BRIDGE_TOOLS miss {name}");
            assert!(include_str!("../mcp.rs").contains(&format!("#[tool(name = \"{name}\"")), "MCP misses {name}");
        }
        assert!(!names.contains(&"set_setlist_footswitch"), "one tool for every action");
        let mut app = DemoApp::new_with_sink(None);
        let v: serde_json::Value = serde_json::from_str(&app.exec_tool("set_show_control", &serde_json::json!({"action": "record", "note": 40, "device": 0}))).unwrap();
        assert_eq!(v["actions"].as_array().unwrap().iter().find(|r| r["action"] == "record").unwrap()["control"]["note"], 40, "{v}");
        app.exec_tool("set_show_control", &serde_json::json!({"action": "song_of_program", "device": 3}));
        assert!(app.effective_program_selects_song(3));
        let v: serde_json::Value = serde_json::from_str(&app.exec_tool("get_show_controls", &serde_json::json!({}))).unwrap();
        assert!(v["song_of_program"].as_array().unwrap().iter().any(|r| r["device"] == 3 && r["on"] == true), "{v}");
        assert!(app.exec_tool("set_show_control", &serde_json::json!({"action": "nope"})).starts_with("error"));
    }

    /// An Audio In unit whose tuner hears a steady 442 Hz sine (A4, +8 ¢),
    /// through the real ring and estimator the input feeds.
    fn tuning(app: &mut DemoApp) {
        let mut g = app.base_graph.clone();
        let node = g.add_node(NodeKind::AudioIn { device: None, channels: crate::control::AudioInChannel::MonoL }, crate::control::name_bytes("GUITAR"));
        app.build_rack(&g, &[]);
        let (mut tx, rx) = rtrb::RingBuffer::new(48000);
        for i in 0..24000 {
            let _ = tx.push((std::f32::consts::TAU * 442.0 * i as f32 / 48000.0).sin() * 0.5);
        }
        app.audio_in_tuners.insert(node, (rx, crate::audio_in::Tuner::new(48000), None));
        app.tuner_on = true;
    }

    #[test]
    fn the_tuner_shows_the_note_the_audio_in_plays_and_a_click_closes_it() {
        let mut app = DemoApp::new_with_sink(None);
        tuning(&mut app);
        let ctx = egui::Context::default();
        let mut texts_now = |app: &mut DemoApp, events: Vec<egui::Event>| {
            let raw = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, SIZE)),
                time: Some(app.ui_time as f64 + 1.0),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
            out.textures_delta.clear();
            texts(&out.shapes)
        };
        texts_now(&mut app, vec![]);
        let shown = texts_now(&mut app, vec![]);
        assert!(shown.iter().any(|t| t == "A4") && shown.iter().any(|t| t == "+8 ¢"), "{shown:?}");
        let at = Pos2::new(SIZE.x / 2.0, 72.0 + super::TUNER_SIZE.y / 2.0);
        texts_now(&mut app, vec![egui::Event::PointerMoved(at)]);
        let press = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        texts_now(&mut app, vec![press(true)]);
        texts_now(&mut app, vec![press(false)]);
        assert!(!app.tuner_on, "a click on the tuner closes it");
    }

    #[test]
    #[ignore = "writes PNGs: KABELSALAT_SHOTS=<dir> cargo test --bin kabelsalat show_controls_shots -- --ignored"]
    fn show_controls_shots() {
        let dir = std::path::PathBuf::from(std::env::var("KABELSALAT_SHOTS").expect("KABELSALAT_SHOTS=<dir>"));
        std::fs::create_dir_all(&dir).unwrap();
        let gig = |tune: bool| {
            let mut app = DemoApp::new_with_sink(None);
            app.hw_midi.devices.lock().unwrap().extend([
                ("Launchkey Mini:Launchkey Mini MIDI 1 24:0".into(), true),
                ("FCB1010:FCB1010 MIDI 1 32:0".into(), true),
            ]);
            app.hw_midi.rev.fetch_add(1, std::sync::atomic::Ordering::Release);
            app.adopt_hw_devices();
            if tune {
                tuning(&mut app);
            }
            for (name, bpm) in [("Opener", 124), ("Ballad", 72), ("Closer", 132)] {
                let parts = serde_json::json!([{"name": "Verse", "rigs": [{"rig": 0}]}, {"name": "Chorus", "rigs": [{"rig": 0}]}]);
                app.exec_tool("set_setlist_song", &serde_json::json!({"name": name, "bpm": bpm, "parts": parts.to_string()}));
            }
            for (action, dev, msg) in [
                (ShowAction::NextPart, 1, ShowMsg::Cc(80)),
                (ShowAction::PrevPart, 1, ShowMsg::Cc(81)),
                (ShowAction::NextSong, 0, ShowMsg::Note(36)),
                (ShowAction::Panic, 0, ShowMsg::Note(37)),
                (ShowAction::Tuner, 1, ShowMsg::Program(9)),
                (ShowAction::Record, 1, ShowMsg::Cc(85)),
                (ShowAction::ShowMode, 1, ShowMsg::Cc(86)),
            ] {
                app.persist_show_control(action, Some(dev), None, None, msg);
            }
            let _ = app.set_permanent_program_device(Some(1), None, true);
            app.select_setlist(0, 1).unwrap();
            app
        };
        // Rule 148: a fresh app per image.
        for size in [Vec2::new(1280.0, 1000.0), Vec2::new(781.0, 950.0)] {
            let mut app = gig(false);
            app.stage_view = StageView::Setlist;
            app.show_learn = Some(ShowAction::TapTempo);
            super::super::offline_shots::shot(&mut app, size, &dir.join(format!("show-controls-{}x{}.png", size.x, size.y)));
        }
        let mut app = gig(true);
        app.set_show_mode(true);
        super::super::offline_shots::shot(&mut app, Vec2::new(1280.0, 800.0), &dir.join("show-tuner-1280x800.png"));
    }

    #[test]
    fn settings_show_manual_edit_validates_saves_and_passes_layout_across_sizes() {
        use super::super::settings_and_dropdown_tests::{
            assert_settings_text_layout, click_at, run_frame,
        };
        let sizes = [
            Vec2::new(1280.0, 900.0),
            Vec2::new(1100.0, 750.0),
            Vec2::new(781.0, 468.0),
            Vec2::new(760.0, 520.0),
        ];
        for size in sizes {
            let mut app = DemoApp::new_with_sink(None);
            app.hw_midi.devices.lock().unwrap().extend([
                ("Launchkey Mini:Launchkey Mini MIDI 1 24:0".into(), true),
                ("FCB1010:FCB1010 MIDI 1 32:0".into(), true),
            ]);
            app.hw_midi.rev.fetch_add(1, std::sync::atomic::Ordering::Release);
            app.adopt_hw_devices();
            app.stage_view = StageView::Settings;
            app.settings_page = SettingsPage::Show;
            app.setlist.learn(ShowAction::Panic, Some(0), ShowMsg::Note(48));
            let ctx = egui::Context::default();
            let mut texts = Vec::new();
            for _ in 0..3 {
                texts = run_frame(&mut app, &ctx, size, vec![]).0;
            }
            assert_settings_text_layout(&app, &texts, size);

            // On desktop width/height, clicking the visible "Edit" button opens the inline editor.
            if size.y >= 800.0 {
                let start_y = texts
                    .iter()
                    .find(|(t, _)| t == "Start in Show mode")
                    .map(|(_, r)| r.bottom())
                    .expect("Start in Show mode row in Settings -> Show");
                let next_song_y = texts
                    .iter()
                    .filter(|(t, r)| t == "Next song" && r.top() > start_y)
                    .map(|(_, r)| r.center().y)
                    .min_by(|a, b| a.total_cmp(b))
                    .expect("Next song row in Settings -> Show");
                let edit_pos = texts
                    .iter()
                    .filter(|(t, r)| t == "Edit" && (r.center().y - next_song_y).abs() < 16.0)
                    .map(|(_, r)| r.center())
                    .next()
                    .expect("Edit button on Next song row");
                click_at(&mut app, &ctx, size, edit_pos);
                let _ = run_frame(&mut app, &ctx, size, vec![]).0;
                assert_eq!(app.show_control_edit.as_ref().map(|e| e.action), Some(ShowAction::NextSong));
                app.apply_show_control_action(ShowControlAction::CancelEdit);
            }

            // Open inline editor (NextPart near top and ShowMode) and check layout across all sizes.
            app.apply_show_control_action(ShowControlAction::OpenEdit(ShowAction::NextPart));
            let texts_part_edit = run_frame(&mut app, &ctx, size, vec![]).0;
            assert_settings_text_layout(&app, &texts_part_edit, size);
            app.apply_show_control_action(ShowControlAction::OpenEdit(ShowAction::ShowMode));
            let texts_edit = run_frame(&mut app, &ctx, size, vec![]).0;
            assert_eq!(app.show_control_edit.as_ref().map(|e| e.action), Some(ShowAction::ShowMode));
            assert_settings_text_layout(&app, &texts_edit, size);

            // Out-of-range CC is refused with an inline error.
            app.show_control_edit.as_mut().unwrap().kind = MsgKind::Cc;
            app.show_control_edit.as_mut().unwrap().number_text = "999".into();
            app.apply_show_control_action(ShowControlAction::SaveEdit);
            assert!(app.show_control_edit.as_ref().and_then(|e| e.error.as_ref()).is_some(), "{size:?}: invalid CC refused");

            // Valid CC 88 on Ch 3 (0-based channel 2) on FCB1010 saves cleanly.
            {
                let edit = app.show_control_edit.as_mut().unwrap();
                edit.kind = MsgKind::Cc;
                edit.number_text = "88".into();
                edit.channel = Some(2);
                edit.device_port = Some("FCB1010:FCB1010 MIDI 1".into());
            }
            app.apply_show_control_action(ShowControlAction::SaveEdit);
            assert!(app.show_control_edit.is_none());
            let saved = app.config_show_controls.binding_of(ShowAction::ShowMode).expect("ShowMode saved");
            assert_eq!(saved.msg, ShowMsg::Cc(88));
            assert_eq!(saved.channel, Some(2));
            assert_eq!(saved.device.as_deref(), Some("FCB1010:FCB1010 MIDI 1"));
            assert_eq!(
                app.effective_show_control(ShowAction::ShowMode),
                Some(ShowControl {
                    device: Some(1),
                    channel: Some(2),
                    msg: ShowMsg::Cc(88),
                    action: ShowAction::ShowMode,
                })
            );
        }
    }

    #[test]
    fn permanent_show_controls_survive_template_and_session_loads_and_follow_hotplug() {
        let mut s = Stage::new();
        // Bind FCB1010 (device 1 at startup) permanently to ShowMode and NextPart.
        s.app.hw_midi.devices.lock().unwrap().extend([
            ("Keys:Keys MIDI 1 20:0".into(), true),
            ("FCB1010:FCB1010 MIDI 1 32:0".into(), true),
        ]);
        s.app.hw_midi.rev.fetch_add(1, std::sync::atomic::Ordering::Release);
        s.app.adopt_hw_devices();

        s.app.persist_show_control(ShowAction::NextPart, Some(1), None, None, ShowMsg::Cc(80));
        s.app.persist_show_control(ShowAction::ShowMode, Some(1), None, None, ShowMsg::Cc(81));
        s.tick();

        // Loading a template, demo patch, or blank session preserves permanent bindings.
        s.app.load_demo_patch();
        s.tick();
        assert_eq!(s.app.effective_show_control(ShowAction::NextPart).map(|c| (c.device, c.msg)), Some((Some(1), ShowMsg::Cc(80))));
        assert_eq!(s.app.effective_show_control(ShowAction::ShowMode).map(|c| (c.device, c.msg)), Some((Some(1), ShowMsg::Cc(81))));

        s.app.apply_session(&crate::session::Session::default()).unwrap();
        s.tick();
        assert_eq!(s.app.effective_show_control(ShowAction::ShowMode).map(|c| c.msg), Some(ShowMsg::Cc(81)));

        // Unplugging FCB1010 deactivates its port-bound controls instead of matching device 0.
        s.app.hw_midi.devices.lock().unwrap()[1].1 = false;
        // Simulate fresh run with only FCB1010 plugged at index 0.
        s.app.hw_devices = vec!["FCB1010:FCB1010 MIDI 1 36:0".into()];
        s.app.hw_live = vec![true];
        assert_eq!(
            s.app.effective_show_control(ShowAction::ShowMode),
            Some(ShowControl {
                device: Some(0),
                channel: None,
                msg: ShowMsg::Cc(81),
                action: ShowAction::ShowMode,
            })
        );

        // Session override on NextPart is separate and can be cleared or promoted.
        s.app.exec_tool("set_show_control", &serde_json::json!({"action": "next_part", "cc": 90, "scope": "session"}));
        assert!(s.app.has_session_show_overrides());
        assert_eq!(s.app.effective_show_control(ShowAction::NextPart).map(|c| c.msg), Some(ShowMsg::Cc(90)));
        s.app.apply_show_control_action(ShowControlAction::ClearSessionOverrides);
        assert!(!s.app.has_session_show_overrides());
        assert_eq!(s.app.effective_show_control(ShowAction::NextPart).map(|c| c.msg), Some(ShowMsg::Cc(80)));
    }

    #[test]
    fn get_config_and_set_config_and_scoped_set_show_control_round_trip() {
        let mut app = DemoApp::new_with_sink(None);
        app.hw_midi.devices.lock().unwrap().push(("Launchkey:Launchkey MIDI 1 24:0".into(), true));
        app.hw_midi.rev.fetch_add(1, std::sync::atomic::Ordering::Release);
        app.adopt_hw_devices();

        let cfg: serde_json::Value = serde_json::from_str(&app.exec_tool("get_config", &serde_json::json!({}))).unwrap();
        assert_eq!(cfg["show_mode"]["clock_24h"], true);
        assert_eq!(cfg["session"]["reopen_last_session"], false);
        assert_eq!(cfg["display"]["ui_scale"], serde_json::Value::Null);

        let updated: serde_json::Value = serde_json::from_str(&app.exec_tool("set_config", &serde_json::json!({
            "show_leave_hold_secs": 2.0,
            "show_clock_24h": false,
            "show_notes": false,
            "show_rig_meters": false,
            "start_in_show_mode": true,
            "reopen_last_session": true,
            "record_sync": true,
            "ui_scale": 1.5,
            "cpu_overlay": true,
            "system_ime": true,
            "plugin_sandbox": false,
            "midi_through": true,
        }))).unwrap();
        assert_eq!(updated["show_mode"]["leave_hold_secs"], 2.0);
        assert_eq!(updated["show_mode"]["clock_24h"], false);
        assert_eq!(updated["show_mode"]["show_notes"], false);
        assert_eq!(updated["show_mode"]["show_rig_meters"], false);
        assert_eq!(updated["show_mode"]["start_in_show_mode"], true);
        assert_eq!(updated["session"]["reopen_last_session"], true);
        assert_eq!(updated["session"]["record_sync"], true);
        assert_eq!(updated["display"]["ui_scale"], 1.5);
        assert_eq!(updated["display"]["cpu_overlay"], true);
        assert_eq!(updated["display"]["system_ime"], true);
        assert_eq!(updated["plugins"]["sandbox"], false);
        assert_eq!(updated["midi"]["midi_through"], true);

        // Null resets ui_scale back to Auto and invalid inputs fail atomically.
        let reset: serde_json::Value = serde_json::from_str(&app.exec_tool("set_config", &serde_json::json!({"ui_scale": null}))).unwrap();
        assert_eq!(reset["display"]["ui_scale"], serde_json::Value::Null);
        assert!(app.exec_tool("set_config", &serde_json::json!({"ui_scale": 9.0})).starts_with("error:"));
        assert!(app.exec_tool("set_config", &serde_json::json!({})).starts_with("error:"));

        // Scoped show control binding with channel & port name.
        let sc: serde_json::Value = serde_json::from_str(&app.exec_tool("set_show_control", &serde_json::json!({
            "action": "show_mode",
            "cc": 70,
            "channel": 4,
            "device_port": "Launchkey:Launchkey MIDI 1",
        }))).unwrap();
        let row = sc["actions"].as_array().unwrap().iter().find(|r| r["action"] == "show_mode").unwrap();
        assert_eq!(row["scope"], "global");
        assert_eq!(row["control"]["cc"], 70);
        assert_eq!(row["control"]["channel"], 4);
        assert_eq!(row["control"]["device_port"], "Launchkey:Launchkey MIDI 1");
    }

    #[test]
    #[ignore = "writes PNGs: KABELSALAT_SHOTS=<dir> cargo test --bin kabelsalat settings_show_shots -- --ignored"]
    fn settings_show_shots() {
        let dir = std::path::PathBuf::from(std::env::var("KABELSALAT_SHOTS").expect("KABELSALAT_SHOTS=<dir>"));
        std::fs::create_dir_all(&dir).unwrap();
        let make = |open_editor: bool| {
            let mut app = DemoApp::new_with_sink(None);
            app.hw_midi.devices.lock().unwrap().extend([
                ("Launchkey Mini:Launchkey Mini MIDI 1 24:0".into(), true),
                ("FCB1010:FCB1010 MIDI 1 32:0".into(), true),
            ]);
            app.hw_midi.rev.fetch_add(1, std::sync::atomic::Ordering::Release);
            app.adopt_hw_devices();
            for (action, dev, ch, msg) in [
                (ShowAction::NextPart, 1, None, ShowMsg::Cc(80)),
                (ShowAction::PrevPart, 1, None, ShowMsg::Cc(81)),
                (ShowAction::NextSong, 0, None, ShowMsg::Note(36)),
                (ShowAction::Panic, 0, Some(0), ShowMsg::Note(37)),
                (ShowAction::Tuner, 1, None, ShowMsg::Program(9)),
                (ShowAction::Record, 1, None, ShowMsg::Cc(85)),
                (ShowAction::ShowMode, 1, None, ShowMsg::Cc(86)),
            ] {
                app.persist_show_control(action, Some(dev), None, ch, msg);
            }
            let _ = app.set_permanent_program_device(Some(1), None, true);
            app.stage_view = StageView::Settings;
            app.settings_page = SettingsPage::Show;
            if open_editor {
                app.apply_show_control_action(ShowControlAction::OpenEdit(ShowAction::NextPart));
            }
            app
        };
        // Rule 148: a fresh app per image.
        let mut desktop = make(false);
        super::super::offline_shots::shot(&mut desktop, Vec2::new(1280.0, 800.0), &dir.join("settings-show-1280x800.png"));
        let mut compact = make(true);
        super::super::offline_shots::shot(&mut compact, Vec2::new(781.0, 950.0), &dir.join("settings-show-781x950.png"));
    }
}
