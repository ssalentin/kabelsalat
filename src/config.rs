//! Persistent application configuration stored in `~/.config/kabelsalat/config.json` (#437).
//!
//! Keeps machine- and player-wide preferences outside `.ksalat` session files:
//! - [`ShowModeConfig`]: Show mode exit hold duration, clock format, notes &
//!   meter visibility, and start-in-Show-mode preference.
//! - [`ShowControlsConfig`]: Permanent MIDI CC / Note / Program Change bindings
//!   for Show actions (`NextPart`, `ShowMode`, `Panic`, …) and program-change
//!   song-selection devices, keyed by stable MIDI port name (`midi_port_key`).
//! - App startup & behavior preferences: `reopen_last_session`, `last_session`,
//!   `record_sync`, `ui_scale`, `system_ime`, `plugin_sandbox`, and `midi_through`.
//!
//! Rule 37: every `load_*()` / `save_*()` helper is a no-op returning defaults
//! under `cfg(test)`, backed by `*_from` / `*_to` path helpers for isolated
//! unit tests.
//! Rule 183: process-wide runtime flags (`plugin_sandbox`, `midi_through`) are
//! thread-local in tests so parallel tests never leak into each other.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::assistant::{config_path, save_config_values_to};
use crate::show::{
    ShowAction, ShowControl, ShowControls, ShowMsg, MAX_PROGRAM_DEVICES, NO_SHOW_CONTROLS,
};

pub const SHOW_MODE_FIELD: &str = "show_mode";
pub const SHOW_CONTROLS_FIELD: &str = "show_controls";
pub const REOPEN_LAST_SESSION_FIELD: &str = "reopen_last_session";
pub const LAST_SESSION_FIELD: &str = "last_session";
pub const RECORD_SYNC_FIELD: &str = "record_sync";
pub const UI_SCALE_FIELD: &str = "ui_scale";
pub const SYSTEM_IME_FIELD: &str = "system_ime";
pub const PLUGIN_SANDBOX_FIELD: &str = "plugin_sandbox";
pub const MIDI_THROUGH_FIELD: &str = "midi_through";

pub const LEAVE_HOLD_CHOICES: &[f32] = &[0.5, 1.0, 2.0, 3.0];
pub const DEFAULT_LEAVE_HOLD_SECS: f32 = 1.0;
pub const MIN_LEAVE_HOLD_SECS: f32 = 0.25;
pub const MAX_LEAVE_HOLD_SECS: f32 = 5.0;

pub const UI_SCALE_CHOICES: &[f32] = &[1.0, 1.25, 1.5, 1.6, 1.75, 2.0, 2.5];
pub const MIN_UI_SCALE: f32 = 1.0;
pub const MAX_UI_SCALE: f32 = 3.0;

/// Read a single JSON field from `path`.
pub(crate) fn load_config_value_from(path: &Path, field: &str) -> Option<Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|mut v| v.get_mut(field).map(Value::take))
}

/// Read a string field from `path` (empty strings count as `None`).
pub(crate) fn load_config_str_from(path: &Path, field: &str) -> Option<String> {
    load_config_value_from(path, field)
        .and_then(|v| v.as_str().map(str::to_string))
        .filter(|s| !s.trim().is_empty())
}

// ---------------------------------------------------------------------------
// Show mode preferences
// ---------------------------------------------------------------------------

/// Permanent preferences for the locked stage screen (`Settings → Show`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShowModeConfig {
    /// Seconds Esc must be held before releasing leaves Show mode (`0.25..=5.0`).
    pub leave_hold_secs: f32,
    /// Show the stage clock in 24-hour (`true`, e.g. `21:47`) or 12-hour format (`false`, `9:47 PM`).
    pub clock_24h: bool,
    /// Show the active song's notes / ChordPro chart panel in Show mode.
    pub show_notes: bool,
    /// Show per-rig meters below the Master meter in Show mode.
    pub show_rig_meters: bool,
    /// Open Show mode automatically when the app launches.
    pub start_in_show_mode: bool,
}

impl Default for ShowModeConfig {
    fn default() -> Self {
        Self {
            leave_hold_secs: DEFAULT_LEAVE_HOLD_SECS,
            clock_24h: true,
            show_notes: true,
            show_rig_meters: true,
            start_in_show_mode: false,
        }
    }
}

pub fn clamp_leave_hold_secs(secs: f32) -> f32 {
    if secs.is_finite() {
        (secs.clamp(MIN_LEAVE_HOLD_SECS, MAX_LEAVE_HOLD_SECS) * 100.0).round() / 100.0
    } else {
        DEFAULT_LEAVE_HOLD_SECS
    }
}

impl ShowModeConfig {
    pub fn normalized(mut self) -> Self {
        self.leave_hold_secs = clamp_leave_hold_secs(self.leave_hold_secs);
        self
    }

    pub fn from_json(v: &Value) -> Self {
        let mut cfg = Self::default();
        if let Some(obj) = v.as_object() {
            if let Some(secs) = obj.get("leave_hold_secs").and_then(Value::as_f64) {
                cfg.leave_hold_secs = secs as f32;
            }
            if let Some(b) = obj.get("clock_24h").and_then(Value::as_bool) {
                cfg.clock_24h = b;
            }
            if let Some(b) = obj.get("show_notes").and_then(Value::as_bool) {
                cfg.show_notes = b;
            }
            if let Some(b) = obj.get("show_rig_meters").and_then(Value::as_bool) {
                cfg.show_rig_meters = b;
            }
            if let Some(b) = obj.get("start_in_show_mode").and_then(Value::as_bool) {
                cfg.start_in_show_mode = b;
            }
        }
        cfg.normalized()
    }

    pub fn to_json(&self) -> Value {
        serde_json::to_value(self.normalized()).unwrap_or_else(|_| json!({}))
    }
}

pub fn load_show_mode_from(path: &Path) -> ShowModeConfig {
    load_config_value_from(path, SHOW_MODE_FIELD)
        .map(|v| ShowModeConfig::from_json(&v))
        .unwrap_or_default()
}

pub fn save_show_mode_to(path: &Path, cfg: &ShowModeConfig) -> std::io::Result<()> {
    save_config_values_to(path, &[(SHOW_MODE_FIELD, cfg.to_json())])
}

pub fn load_show_mode() -> ShowModeConfig {
    if cfg!(test) {
        return ShowModeConfig::default();
    }
    load_show_mode_from(&config_path())
}

pub fn save_show_mode(cfg: &ShowModeConfig) -> std::io::Result<()> {
    if cfg!(test) {
        return Ok(());
    }
    save_show_mode_to(&config_path(), cfg)
}

// ---------------------------------------------------------------------------
// Permanent show controls (port-keyed across restarts and sessions)
// ---------------------------------------------------------------------------

/// One permanent MIDI show-control binding stored in `config.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedShowControl {
    pub action: ShowAction,
    pub msg: ShowMsg,
    /// Stable hardware MIDI port name (`midi_port_key`); `None` matches any device
    /// unless [`Self::device_index`] is set in memory for an unnamed device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// In-memory index fallback when bound from an unnamed device index in
    /// headless mode or tests.
    #[serde(skip)]
    pub device_index: Option<u32>,
    /// Optional MIDI channel `0..=15`; `None` matches any channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u8>,
}

/// Permanent show-control bindings and program-change song-selection ports.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShowControlsConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<SavedShowControl>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub program_devices: Vec<String>,
    /// In-memory device indices for `SongOfProgram` when bound without a port name.
    #[serde(skip)]
    pub program_device_indices: Vec<u32>,
}

fn valid_show_msg(msg: ShowMsg) -> bool {
    match msg {
        ShowMsg::Cc(n) | ShowMsg::Note(n) | ShowMsg::Program(n) => n <= 127,
        ShowMsg::AnyProgram => false,
    }
}

fn devices_overlap(
    a_port: Option<&str>,
    a_idx: Option<u32>,
    b_port: Option<&str>,
    b_idx: Option<u32>,
) -> bool {
    match (a_port, a_idx, b_port, b_idx) {
        (None, None, _, _) | (_, _, None, None) => true,
        (Some(ap), _, Some(bp), _) => ap == bp,
        (None, Some(ai), None, Some(bi)) => ai == bi,
        _ => false,
    }
}

fn channels_overlap(a: Option<u8>, b: Option<u8>) -> bool {
    match (a, b) {
        (None, _) | (_, None) => true,
        (Some(ac), Some(bc)) => (ac & 0x0F) == (bc & 0x0F),
    }
}

fn table_rank(c: &ShowControl) -> (usize, Option<u32>) {
    let pos = ShowAction::ALL
        .iter()
        .position(|&a| a == c.action)
        .unwrap_or(ShowAction::ALL.len());
    (pos, c.device)
}

impl ShowControlsConfig {
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
            && self.program_devices.is_empty()
            && self.program_device_indices.is_empty()
    }

    pub fn binding_of(&self, action: ShowAction) -> Option<&SavedShowControl> {
        self.bindings.iter().find(|b| b.action == action)
    }

    /// Bind `action` to `msg` on `(device, channel)`. Displaces any other
    /// action whose binding overlaps on `(msg, device, channel)` and returns it.
    pub fn set_binding(
        &mut self,
        action: ShowAction,
        msg: ShowMsg,
        device: Option<String>,
        device_index: Option<u32>,
        channel: Option<u8>,
    ) -> Option<ShowAction> {
        if action == ShowAction::SongOfProgram || !valid_show_msg(msg) {
            return None;
        }
        let device = device.filter(|s| !s.trim().is_empty());
        let device_index = if device.is_some() { None } else { device_index };
        let channel = channel.map(|c| c & 0x0F);
        let overlaps = |b: &SavedShowControl| {
            b.msg == msg
                && devices_overlap(
                    b.device.as_deref(),
                    b.device_index,
                    device.as_deref(),
                    device_index,
                )
                && channels_overlap(b.channel, channel)
        };
        let displaced = self
            .bindings
            .iter()
            .find(|b| b.action != action && overlaps(b))
            .map(|b| b.action);
        self.bindings.retain(|b| b.action != action && !overlaps(b));
        self.bindings.push(SavedShowControl {
            action,
            msg,
            device,
            device_index,
            channel,
        });
        self.sort_bindings();
        displaced
    }

    pub fn clear_binding(&mut self, action: ShowAction) -> bool {
        let before = self.bindings.len();
        self.bindings.retain(|b| b.action != action);
        self.bindings.len() != before
    }

    pub fn program_selects_port(&self, port_key: &str) -> bool {
        self.program_devices.iter().any(|p| p == port_key)
    }

    pub fn program_selects_index(&self, d: u32) -> bool {
        self.program_device_indices.contains(&d)
    }

    pub fn set_program_port(&mut self, port_key: String, on: bool) {
        let port_key = port_key.trim().to_string();
        if port_key.is_empty() {
            return;
        }
        self.program_devices.retain(|p| p != &port_key);
        if on && self.total_program_count() < MAX_PROGRAM_DEVICES {
            self.program_devices.push(port_key);
        }
    }

    pub fn set_program_index(&mut self, d: u32, on: bool) {
        self.program_device_indices.retain(|&x| x != d);
        if on && self.total_program_count() < MAX_PROGRAM_DEVICES {
            self.program_device_indices.push(d);
        }
    }

    fn total_program_count(&self) -> usize {
        self.program_devices.len() + self.program_device_indices.len()
    }

    fn sort_bindings(&mut self) {
        self.bindings.sort_by_key(|b| {
            ShowAction::LEARNED
                .iter()
                .position(|&a| a == b.action)
                .unwrap_or(ShowAction::LEARNED.len())
        });
    }

    pub fn normalize(&mut self) {
        for b in &mut self.bindings {
            if b.device.as_deref().is_some_and(|s| s.trim().is_empty()) {
                b.device = None;
            }
            if b.device.is_some() {
                b.device_index = None;
            }
        }
        self.bindings.retain(|b| {
            b.action != ShowAction::SongOfProgram
                && valid_show_msg(b.msg)
                && b.channel.is_none_or(|c| c <= 15)
        });
        self.sort_bindings();
        let mut seen_actions = Vec::new();
        self.bindings.retain(|b| {
            if seen_actions.contains(&b.action) {
                false
            } else {
                seen_actions.push(b.action);
                true
            }
        });
        self.program_devices.retain(|p| !p.trim().is_empty());
        let mut seen_ports = Vec::new();
        self.program_devices.retain(|p| {
            if seen_ports.contains(p) {
                false
            } else {
                seen_ports.push(p.clone());
                true
            }
        });
        self.program_devices.truncate(MAX_PROGRAM_DEVICES);
        let mut seen_idx = Vec::new();
        self.program_device_indices.retain(|&d| {
            if seen_idx.contains(&d) {
                false
            } else {
                seen_idx.push(d);
                true
            }
        });
        let rem = MAX_PROGRAM_DEVICES.saturating_sub(self.program_devices.len());
        self.program_device_indices.truncate(rem);
    }

    /// Resolve permanent bindings against `hw_devices`. Bindings naming a port
    /// that is not currently listed in `hw_devices` are skipped so an unplugged
    /// device never broadens to match another controller.
    pub fn resolve(
        &self,
        hw_devices: &[String],
        port_key_fn: impl for<'a> Fn(&'a str) -> &'a str,
    ) -> Vec<ShowControl> {
        let mut out = Vec::new();
        for b in &self.bindings {
            if b.action == ShowAction::SongOfProgram || !valid_show_msg(b.msg) {
                continue;
            }
            let device = match &b.device {
                Some(port) => {
                    match hw_devices.iter().position(|raw| port_key_fn(raw) == port) {
                        Some(idx) => Some(idx as u32),
                        None => continue, // offline port stays inactive until plugged in
                    }
                }
                None => b.device_index,
            };
            out.push(ShowControl {
                device,
                channel: b.channel.map(|c| c & 0x0F),
                msg: b.msg,
                action: b.action,
            });
        }
        for port in &self.program_devices {
            if let Some(idx) = hw_devices.iter().position(|raw| port_key_fn(raw) == port) {
                out.push(ShowControl {
                    device: Some(idx as u32),
                    channel: None,
                    msg: ShowMsg::AnyProgram,
                    action: ShowAction::SongOfProgram,
                });
            }
        }
        for &idx in &self.program_device_indices {
            out.push(ShowControl {
                device: Some(idx),
                channel: None,
                msg: ShowMsg::AnyProgram,
                action: ShowAction::SongOfProgram,
            });
        }
        out.sort_by_key(table_rank);
        let mut seen = Vec::new();
        out.retain(|c| {
            let key = if c.action == ShowAction::SongOfProgram {
                (c.action, c.device)
            } else {
                (c.action, None)
            };
            if seen.contains(&key) {
                false
            } else {
                seen.push(key);
                true
            }
        });
        out
    }

    /// Merge global permanent show controls with per-session overrides (`session_controls`),
    /// producing the fixed-size engine table. Session overrides replace global bindings
    /// for the same action (and displace colliding messages), and union program devices.
    pub fn merge_with_session(
        &self,
        session_controls: &[ShowControl],
        hw_devices: &[String],
        port_key_fn: impl for<'a> Fn(&'a str) -> &'a str,
    ) -> ShowControls {
        let mut merged = self.resolve(hw_devices, port_key_fn);
        for &sc in session_controls {
            if sc.action == ShowAction::SongOfProgram {
                if sc.device.is_some()
                    && !merged
                        .iter()
                        .any(|c| c.action == ShowAction::SongOfProgram && c.device == sc.device)
                {
                    merged.push(ShowControl {
                        msg: ShowMsg::AnyProgram,
                        channel: None,
                        ..sc
                    });
                }
                continue;
            }
            if sc.msg == ShowMsg::AnyProgram {
                continue;
            }
            let overlaps = |c: &ShowControl| {
                c.action != ShowAction::SongOfProgram
                    && c.msg == sc.msg
                    && (c.device.is_none() || sc.device.is_none() || c.device == sc.device)
                    && channels_overlap(c.channel, sc.channel)
            };
            merged.retain(|c| c.action == ShowAction::SongOfProgram || (c.action != sc.action && !overlaps(c)));
            merged.push(sc);
        }
        merged.sort_by_key(table_rank);
        let mut prog_count = 0usize;
        merged.retain(|c| {
            if c.action == ShowAction::SongOfProgram {
                prog_count += 1;
                prog_count <= MAX_PROGRAM_DEVICES
            } else {
                true
            }
        });
        let mut table = NO_SHOW_CONTROLS;
        for (slot, c) in table.iter_mut().zip(merged) {
            *slot = Some(c);
        }
        table
    }

    pub fn from_json(v: &Value) -> Self {
        let mut cfg = Self::default();
        if let Some(list) = v.get("bindings").and_then(Value::as_array) {
            for item in list {
                if let Ok(b) = serde_json::from_value::<SavedShowControl>(item.clone()) {
                    cfg.bindings.push(b);
                }
            }
        }
        if let Some(list) = v.get("program_devices").and_then(Value::as_array) {
            for item in list {
                if let Some(p) = item.as_str() {
                    cfg.program_devices.push(p.to_string());
                }
            }
        }
        cfg.normalize();
        cfg
    }

    pub fn to_json(&self) -> Value {
        let mut copy = self.clone();
        copy.normalize();
        serde_json::to_value(copy).unwrap_or_else(|_| json!({}))
    }
}

pub fn load_show_controls_from(path: &Path) -> ShowControlsConfig {
    load_config_value_from(path, SHOW_CONTROLS_FIELD)
        .map(|v| ShowControlsConfig::from_json(&v))
        .unwrap_or_default()
}

pub fn save_show_controls_to(path: &Path, cfg: &ShowControlsConfig) -> std::io::Result<()> {
    save_config_values_to(path, &[(SHOW_CONTROLS_FIELD, cfg.to_json())])
}

pub fn load_show_controls() -> ShowControlsConfig {
    if cfg!(test) {
        return ShowControlsConfig::default();
    }
    load_show_controls_from(&config_path())
}

pub fn save_show_controls(cfg: &ShowControlsConfig) -> std::io::Result<()> {
    if cfg!(test) {
        return Ok(());
    }
    save_show_controls_to(&config_path(), cfg)
}

// ---------------------------------------------------------------------------
// Additional permanent app options
// ---------------------------------------------------------------------------

pub fn load_reopen_last_session_from(path: &Path) -> bool {
    load_config_value_from(path, REOPEN_LAST_SESSION_FIELD)
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

pub fn save_reopen_last_session_to(path: &Path, on: bool) -> std::io::Result<()> {
    save_config_values_to(path, &[(REOPEN_LAST_SESSION_FIELD, json!(on))])
}

pub fn load_reopen_last_session() -> bool {
    if cfg!(test) {
        return false;
    }
    load_reopen_last_session_from(&config_path())
}

pub fn save_reopen_last_session(on: bool) -> std::io::Result<()> {
    if cfg!(test) {
        return Ok(());
    }
    save_reopen_last_session_to(&config_path(), on)
}

pub fn load_last_session_from(path: &Path) -> Option<String> {
    load_config_str_from(path, LAST_SESSION_FIELD)
}

pub fn save_last_session_to(path: &Path, name: Option<&str>) -> std::io::Result<()> {
    let val = name
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| json!(s))
        .unwrap_or(Value::Null);
    save_config_values_to(path, &[(LAST_SESSION_FIELD, val)])
}

pub fn load_last_session() -> Option<String> {
    if cfg!(test) {
        return None;
    }
    load_last_session_from(&config_path())
}

pub fn save_last_session(name: Option<&str>) -> std::io::Result<()> {
    if cfg!(test) {
        return Ok(());
    }
    save_last_session_to(&config_path(), name)
}

pub fn load_record_sync_from(path: &Path) -> bool {
    load_config_value_from(path, RECORD_SYNC_FIELD)
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

pub fn save_record_sync_to(path: &Path, on: bool) -> std::io::Result<()> {
    save_config_values_to(path, &[(RECORD_SYNC_FIELD, json!(on))])
}

pub fn load_record_sync() -> bool {
    if cfg!(test) {
        return false;
    }
    load_record_sync_from(&config_path())
}

pub fn save_record_sync(on: bool) -> std::io::Result<()> {
    if cfg!(test) {
        return Ok(());
    }
    save_record_sync_to(&config_path(), on)
}

pub fn valid_ui_scale(scale: f32) -> bool {
    scale.is_finite() && (MIN_UI_SCALE..=MAX_UI_SCALE).contains(&scale)
}

pub fn normalize_ui_scale(scale: Option<f32>) -> Option<f32> {
    scale
        .filter(|&s| valid_ui_scale(s))
        .map(|s| (s * 100.0).round() / 100.0)
}

pub fn load_ui_scale_from(path: &Path) -> Option<f32> {
    load_config_value_from(path, UI_SCALE_FIELD)
        .and_then(|v| v.as_f64())
        .map(|s| s as f32)
        .filter(|&s| valid_ui_scale(s))
        .map(|s| (s * 100.0).round() / 100.0)
}

pub fn save_ui_scale_to(path: &Path, scale: Option<f32>) -> std::io::Result<()> {
    let val = normalize_ui_scale(scale).map(|s| json!(s)).unwrap_or(Value::Null);
    save_config_values_to(path, &[(UI_SCALE_FIELD, val)])
}

pub fn load_ui_scale() -> Option<f32> {
    if cfg!(test) {
        return None;
    }
    load_ui_scale_from(&config_path())
}

pub fn save_ui_scale(scale: Option<f32>) -> std::io::Result<()> {
    if cfg!(test) {
        return Ok(());
    }
    save_ui_scale_to(&config_path(), scale)
}

pub fn load_system_ime_from(path: &Path) -> bool {
    load_config_value_from(path, SYSTEM_IME_FIELD)
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

pub fn save_system_ime_to(path: &Path, on: bool) -> std::io::Result<()> {
    save_config_values_to(path, &[(SYSTEM_IME_FIELD, json!(on))])
}

pub fn load_system_ime() -> bool {
    if cfg!(test) {
        return false;
    }
    load_system_ime_from(&config_path())
}

pub fn save_system_ime(on: bool) -> std::io::Result<()> {
    if cfg!(test) {
        return Ok(());
    }
    save_system_ime_to(&config_path(), on)
}

// ---------------------------------------------------------------------------
// Process-wide flags (with rule 183 per-thread isolation under cfg(test))
// ---------------------------------------------------------------------------

#[cfg(not(test))]
static PLUGIN_SANDBOX: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

#[cfg(not(test))]
static MIDI_THROUGH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
thread_local! {
    static PLUGIN_SANDBOX_TL: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
    static MIDI_THROUGH_TL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub fn load_plugin_sandbox_from(path: &Path) -> bool {
    load_config_value_from(path, PLUGIN_SANDBOX_FIELD)
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

pub fn save_plugin_sandbox_to(path: &Path, on: bool) -> std::io::Result<()> {
    save_config_values_to(path, &[(PLUGIN_SANDBOX_FIELD, json!(on))])
}

pub fn plugin_sandbox() -> bool {
    #[cfg(not(test))]
    {
        PLUGIN_SANDBOX.load(std::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(test)]
    {
        PLUGIN_SANDBOX_TL.with(|c| c.get())
    }
}

pub fn set_plugin_sandbox(on: bool) {
    #[cfg(not(test))]
    {
        PLUGIN_SANDBOX.store(on, std::sync::atomic::Ordering::Relaxed);
    }
    #[cfg(test)]
    {
        PLUGIN_SANDBOX_TL.with(|c| c.set(on));
    }
}

pub fn save_plugin_sandbox(on: bool) -> std::io::Result<()> {
    set_plugin_sandbox(on);
    if cfg!(test) {
        return Ok(());
    }
    save_plugin_sandbox_to(&config_path(), on)
}

pub fn load_midi_through_from(path: &Path) -> bool {
    load_config_value_from(path, MIDI_THROUGH_FIELD)
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

pub fn save_midi_through_to(path: &Path, on: bool) -> std::io::Result<()> {
    save_config_values_to(path, &[(MIDI_THROUGH_FIELD, json!(on))])
}

pub fn midi_through() -> bool {
    #[cfg(not(test))]
    {
        MIDI_THROUGH.load(std::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(test)]
    {
        MIDI_THROUGH_TL.with(|c| c.get())
    }
}

pub fn set_midi_through(on: bool) {
    #[cfg(not(test))]
    {
        MIDI_THROUGH.store(on, std::sync::atomic::Ordering::Relaxed);
    }
    #[cfg(test)]
    {
        MIDI_THROUGH_TL.with(|c| c.set(on));
    }
}

pub fn save_midi_through(on: bool) -> std::io::Result<()> {
    set_midi_through(on);
    if cfg!(test) {
        return Ok(());
    }
    save_midi_through_to(&config_path(), on)
}

/// Read process-wide config flags once at startup (no-op under `cfg(test)`, rule 37).
pub fn init() {
    if cfg!(test) {
        return;
    }
    let path = config_path();
    set_plugin_sandbox(load_plugin_sandbox_from(&path));
    set_midi_through(load_midi_through_from(&path));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(tag: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "ksalat-config-{tag}-{}-{n}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn port_key(raw: &str) -> &str {
        crate::clocksend::midi_port_key(raw)
    }

    #[test]
    fn show_mode_and_scalar_prefs_round_trip_and_preserve_unrelated_fields() {
        let dir = ScratchDir::new("roundtrip");
        let path = dir.0.join("config.json");
        std::fs::write(
            &path,
            r#"{"openrouter_api_key": "sk-or-secret", "audio_buffer_frames": 128}"#,
        )
        .unwrap();

        let show_mode = ShowModeConfig {
            leave_hold_secs: 2.5,
            clock_24h: false,
            show_notes: false,
            show_rig_meters: false,
            start_in_show_mode: true,
        };
        save_show_mode_to(&path, &show_mode).unwrap();
        save_reopen_last_session_to(&path, true).unwrap();
        save_last_session_to(&path, Some("Friday Gig")).unwrap();
        save_record_sync_to(&path, true).unwrap();
        save_ui_scale_to(&path, Some(1.75)).unwrap();
        save_system_ime_to(&path, true).unwrap();
        save_plugin_sandbox_to(&path, false).unwrap();
        save_midi_through_to(&path, true).unwrap();

        assert_eq!(load_show_mode_from(&path), show_mode);
        assert!(load_reopen_last_session_from(&path));
        assert_eq!(load_last_session_from(&path).as_deref(), Some("Friday Gig"));
        assert!(load_record_sync_from(&path));
        assert_eq!(load_ui_scale_from(&path), Some(1.75));
        assert!(load_system_ime_from(&path));
        assert!(!load_plugin_sandbox_from(&path));
        assert!(load_midi_through_from(&path));

        // Saving null for ui_scale restores Auto (None), and unrelated keys remain.
        save_ui_scale_to(&path, None).unwrap();
        assert_eq!(load_ui_scale_from(&path), None);
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["openrouter_api_key"], "sk-or-secret");
        assert_eq!(raw["audio_buffer_frames"], 128);
    }

    #[test]
    fn broken_and_out_of_range_config_values_fall_back_safely() {
        let dir = ScratchDir::new("broken");
        let path = dir.0.join("config.json");
        std::fs::write(
            &path,
            r#"{
                "show_mode": {
                    "leave_hold_secs": 99.0,
                    "clock_24h": "yes",
                    "show_notes": false
                },
                "show_controls": {
                    "bindings": [
                        {"action": "next_part", "msg": {"cc": 200}, "device": "Bad"},
                        {"action": "song_of_program", "msg": "any_program"},
                        {"action": "panic", "msg": {"note": 36}, "channel": 25},
                        {"action": "show_mode", "msg": {"cc": 80}, "device": "FCB1010:FCB1010 MIDI 1", "channel": 2},
                        {"action": "show_mode", "msg": {"cc": 81}, "device": "Duplicate"}
                    ],
                    "program_devices": ["", "FCB1010:FCB1010 MIDI 1", "FCB1010:FCB1010 MIDI 1"]
                },
                "ui_scale": 9.5
            }"#,
        )
        .unwrap();

        let sm = load_show_mode_from(&path);
        assert_eq!(sm.leave_hold_secs, MAX_LEAVE_HOLD_SECS);
        assert!(sm.clock_24h);
        assert!(!sm.show_notes);
        assert_eq!(load_ui_scale_from(&path), None);

        let sc = load_show_controls_from(&path);
        assert_eq!(sc.bindings.len(), 1);
        assert_eq!(sc.bindings[0].action, ShowAction::ShowMode);
        assert_eq!(sc.bindings[0].msg, ShowMsg::Cc(80));
        assert_eq!(sc.bindings[0].channel, Some(2));
        assert_eq!(sc.program_devices, vec!["FCB1010:FCB1010 MIDI 1"]);
    }

    #[test]
    fn show_controls_resolve_by_port_name_skip_offline_and_merge_session_overrides() {
        let dir = ScratchDir::new("controls");
        let path = dir.0.join("config.json");

        let mut cfg = ShowControlsConfig::default();
        assert_eq!(
            cfg.set_binding(
                ShowAction::NextPart,
                ShowMsg::Cc(80),
                Some("FCB1010:FCB1010 MIDI 1".into()),
                None,
                Some(1),
            ),
            None
        );
        // Same CC on a different device does NOT displace NextPart.
        assert_eq!(
            cfg.set_binding(
                ShowAction::ShowMode,
                ShowMsg::Cc(80),
                Some("Launchkey:Launchkey MIDI 1".into()),
                None,
                None,
            ),
            None
        );
        // Same CC on Any device overlaps and displaces both? Test displacement on same port+channel.
        assert_eq!(
            cfg.set_binding(
                ShowAction::PrevPart,
                ShowMsg::Cc(80),
                Some("FCB1010:FCB1010 MIDI 1".into()),
                None,
                None, // any channel overlaps channel 1
            ),
            Some(ShowAction::NextPart)
        );
        // Re-bind NextPart cleanly to CC 81 on FCB1010.
        cfg.set_binding(
            ShowAction::NextPart,
            ShowMsg::Cc(81),
            Some("FCB1010:FCB1010 MIDI 1".into()),
            None,
            None,
        );
        cfg.set_binding(ShowAction::Panic, ShowMsg::Note(36), None, None, None);
        cfg.set_program_port("FCB1010:FCB1010 MIDI 1".into(), true);

        save_show_controls_to(&path, &cfg).unwrap();
        let reloaded = load_show_controls_from(&path);
        assert_eq!(reloaded, cfg);

        // Scan order 1: Launchkey is 0, FCB1010 is 1 (ALSA client numbers stripped).
        let hw1 = vec![
            "Launchkey:Launchkey MIDI 1 24:0".to_string(),
            "FCB1010:FCB1010 MIDI 1 32:0".to_string(),
        ];
        let table1 = reloaded.merge_with_session(&[], &hw1, port_key);
        let items1: Vec<ShowControl> = table1.into_iter().flatten().collect();
        assert!(items1.contains(&ShowControl {
            device: Some(1),
            channel: None,
            msg: ShowMsg::Cc(81),
            action: ShowAction::NextPart,
        }));
        assert!(items1.contains(&ShowControl {
            device: Some(0),
            channel: None,
            msg: ShowMsg::Cc(80),
            action: ShowAction::ShowMode,
        }));
        assert!(items1.contains(&ShowControl {
            device: None,
            channel: None,
            msg: ShowMsg::Note(36),
            action: ShowAction::Panic,
        }));
        assert!(items1.contains(&ShowControl {
            device: Some(1),
            channel: None,
            msg: ShowMsg::AnyProgram,
            action: ShowAction::SongOfProgram,
        }));

        // Scan order 2: FCB1010 is unplugged — its bindings stay inactive (never broaden to device 0 or None).
        let hw2 = vec!["Launchkey:Launchkey MIDI 1 28:0".to_string()];
        let table2 = reloaded.merge_with_session(&[], &hw2, port_key);
        let items2: Vec<ShowControl> = table2.into_iter().flatten().collect();
        assert!(!items2.iter().any(|c| c.action == ShowAction::NextPart));
        assert!(!items2.iter().any(|c| c.action == ShowAction::SongOfProgram));
        assert!(items2.iter().any(|c| c.action == ShowAction::ShowMode && c.device == Some(0)));
        assert!(items2.iter().any(|c| c.action == ShowAction::Panic && c.device.is_none()));

        // Session override replaces global Panic and unions program devices.
        let session_override = [
            ShowControl {
                device: Some(0),
                channel: None,
                msg: ShowMsg::Note(48),
                action: ShowAction::Panic,
            },
            ShowControl {
                device: Some(0),
                channel: None,
                msg: ShowMsg::AnyProgram,
                action: ShowAction::SongOfProgram,
            },
        ];
        let table3 = reloaded.merge_with_session(&session_override, &hw1, port_key);
        let items3: Vec<ShowControl> = table3.into_iter().flatten().collect();
        let panic = items3.iter().find(|c| c.action == ShowAction::Panic).unwrap();
        assert_eq!((panic.device, panic.msg), (Some(0), ShowMsg::Note(48)));
        assert_eq!(
            items3.iter().filter(|c| c.action == ShowAction::SongOfProgram).count(),
            2
        );
    }

    #[test]
    fn runtime_flags_are_isolated_per_test_thread() {
        set_plugin_sandbox(false);
        set_midi_through(true);
        assert!(!plugin_sandbox());
        assert!(midi_through());
        let (other_sb, other_mt) = std::thread::spawn(|| (plugin_sandbox(), midi_through()))
            .join()
            .unwrap();
        assert!(other_sb);
        assert!(!other_mt);
    }
}
