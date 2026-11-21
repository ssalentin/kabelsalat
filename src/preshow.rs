//! Pre-show check (#278): what could go wrong before the first song, found
//! in the dressing room. One table of checks ([`CHECKS`]) reads what the show
//! needs (a [`Session`] with its setlist) and what this machine has right now
//! ([`Facts`]); each check answers ok, warning or problem in a sentence that
//! says what to do. The Setlist view's "Check show" panel, the
//! `preshow_check` agent tool and `kabelsalat preshow-check <session>` all
//! run this table. Collecting the facts does the I/O (device lists, the
//! plugin scan, an offline probe of every rig: [`gather`], [`probe`]) and
//! never runs on the UI thread; the checks are pure and never change the
//! session.

pub mod gather;
pub mod probe;

use serde::Serialize;

use crate::audio_in::InputDevice;
use crate::audio_out::OutputDevice;
use crate::control::{clap_plugin_key, name_str, MidiFxPlugin, Node, NodeKind, SendSource};
use crate::engine_info::LoadLevel;
use crate::session::Session;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warning,
    Problem,
}

/// What one check found.
#[derive(Clone, Debug, PartialEq)]
pub struct Outcome {
    pub status: Status,
    pub text: String,
}

impl Outcome {
    fn ok(text: impl Into<String>) -> Self {
        Self { status: Status::Ok, text: text.into() }
    }

    fn warning(text: impl Into<String>) -> Self {
        Self { status: Status::Warning, text: text.into() }
    }

    fn problem(text: impl Into<String>) -> Self {
        Self { status: Status::Problem, text: text.into() }
    }
}

pub struct Check {
    pub id: &'static str,
    pub title: &'static str,
    pub run: fn(&Session, &Facts) -> Outcome,
}

/// Every check, in the order the report lists them.
pub const CHECKS: [Check; 9] = [
    Check { id: "midi_devices", title: "MIDI devices", run: midi_devices },
    Check { id: "audio_output", title: "Audio output", run: audio_output },
    Check { id: "audio_inputs", title: "Audio inputs", run: audio_inputs },
    Check { id: "plugins", title: "Plugins", run: plugins },
    Check { id: "rigs_sound", title: "Every rig sounds", run: rigs_sound },
    Check { id: "backing_tracks", title: "Backing tracks", run: backing_tracks },
    Check { id: "disk", title: "Disk space", run: disk },
    Check { id: "render", title: "Buffer and headroom", run: render },
    Check { id: "power", title: "Power", run: power },
];

/// What this machine has right now. `None` = could not be found out.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    /// Hardware MIDI ports plugged in, by port name without ALSA's
    /// client:port numbers (rule 58).
    pub midi_ports: Option<Vec<String>>,
    pub outputs: Option<Vec<OutputDevice>>,
    /// The output chosen in Settings; `None` = the system default.
    pub chosen_output: Option<String>,
    /// The running app's output stream: `Some(false)` = none is running.
    /// `None` outside the app.
    pub audio_running: Option<bool>,
    pub inputs: Option<Vec<InputDevice>>,
    pub plugins: Option<Installed>,
    pub probe: Option<Probe>,
    /// Each backing-track path the setlist names.
    pub files: Vec<(String, FileFact)>,
    pub disk: Option<Disk>,
    pub render: Option<Render>,
    pub power: Option<Power>,
}

/// The plugin scan: CLAP ids and LV2 URIs (with "is an instrument").
#[derive(Clone, Debug, Default)]
pub struct Installed {
    pub clap: Vec<String>,
    pub lv2: Vec<(String, bool)>,
}

impl Installed {
    fn has(&self, need: &Plugin) -> bool {
        match need {
            Plugin::Clap(id) => self.clap.iter().any(|c| c == id),
            Plugin::Lv2(uri) => self.lv2.iter().any(|(u, _)| u == uri),
        }
    }

    pub fn lv2_instrument(&self, uri: &str) -> bool {
        self.lv2.iter().any(|(u, inst)| u == uri && *inst)
    }
}

/// What the offline probe heard.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Probe {
    /// The loudest master sample of each rig with an instrument while that
    /// rig played its demo phrase alone.
    pub rigs: Vec<(u8, f32)>,
    /// Installed plugins that did not load, by node, with why.
    pub failed: Vec<(u32, String)>,
}

/// A master peak below this is silence (−60 dBFS).
pub const HEARD: f32 = 0.001;

#[derive(Clone, Debug, PartialEq)]
pub enum FileFact {
    Missing,
    Unreadable(String),
    Audio { seconds: f64 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Disk {
    pub dir: String,
    pub free_bytes: u64,
}

/// How long rendering one buffer takes.
#[derive(Clone, Debug, PartialEq)]
pub struct Render {
    pub sample_rate: u32,
    pub buffer_frames: u32,
    pub avg_us: f64,
    pub max_us: f64,
    /// Dropouts the running app counted; `None` = measured offline.
    pub xruns: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Power {
    Mains { battery: Option<u8> },
    Battery { percent: Option<u8> },
    NoBattery,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Row {
    pub id: &'static str,
    pub title: &'static str,
    pub status: Status,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Report {
    pub session: String,
    pub songs: usize,
    pub rows: Vec<Row>,
    pub took_ms: u64,
}

impl Report {
    pub fn of(session: &Session, facts: &Facts, took: std::time::Duration) -> Self {
        Self {
            session: session.name.clone(),
            songs: session.setlist.songs.len(),
            rows: run(session, facts),
            took_ms: took.as_millis() as u64,
        }
    }

    pub fn count(&self, status: Status) -> usize {
        self.rows.iter().filter(|r| r.status == status).count()
    }

    /// "All 9 checks passed" or "2 problems, 1 warning".
    pub fn summary(&self) -> String {
        let (p, w) = (self.count(Status::Problem), self.count(Status::Warning));
        let n = |k: usize, one: &str, many: &str| format!("{k} {}", if k == 1 { one } else { many });
        match (p, w) {
            (0, 0) => format!("All {} checks passed", self.rows.len()),
            (0, w) => format!("{} · {} ok", n(w, "warning", "warnings"), self.count(Status::Ok)),
            (p, 0) => format!("{} · {} ok", n(p, "problem", "problems"), self.count(Status::Ok)),
            (p, w) => format!("{} · {} · {} ok", n(p, "problem", "problems"), n(w, "warning", "warnings"), self.count(Status::Ok)),
        }
    }

    /// The report as terminal lines.
    pub fn text(&self) -> String {
        let songs = match self.songs {
            1 => "1 song".to_string(),
            n => format!("{n} songs"),
        };
        let mut out = format!(
            "Pre-show check: {} ({songs}) — {} ({:.1} s)\n",
            self.session,
            self.summary(),
            self.took_ms as f64 / 1000.0
        );
        let w = self.rows.iter().map(|r| r.title.len()).max().unwrap_or(0);
        for r in &self.rows {
            out += &format!("  {} {:<w$}  {}\n", symbol(r.status), r.title, r.text);
        }
        out
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "session": self.session,
            "songs": self.songs,
            "summary": self.summary(),
            "checks": self.rows,
            "took_ms": self.took_ms,
        })
    }
}

pub fn symbol(status: Status) -> &'static str {
    match status {
        Status::Ok => "✓",
        Status::Warning => "!",
        Status::Problem => "✗",
    }
}

pub fn run(session: &Session, facts: &Facts) -> Vec<Row> {
    CHECKS
        .iter()
        .map(|c| {
            let o = (c.run)(session, facts);
            Row { id: c.id, title: c.title, status: o.status, text: o.text }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The checks
// ---------------------------------------------------------------------------

fn midi_devices(s: &Session, f: &Facts) -> Outcome {
    // Port name → who listens to it.
    let mut wanted: Vec<(String, Vec<String>)> = Vec::new();
    let mut want = |port: &str, user: String| match wanted.iter_mut().find(|(p, _)| p == port) {
        Some((_, users)) if users.contains(&user) => {}
        Some((_, users)) => users.push(user),
        None => wanted.push((port.to_string(), vec![user])),
    };
    for (&d, port) in s.hw_ports() {
        let rigs: Vec<String> = (0..s.rig_devices.len() as u8).filter(|&r| s.rig_devices[r as usize] == Some(d)).map(|r| rig_label(s, r)).collect();
        if rigs.is_empty() {
            want(port, "a matrix route".into());
        }
        for r in rigs {
            want(port, r);
        }
    }
    for m in s.midi_sources() {
        if let (Some(port), Some(n)) = (&m.device, s.graph.node(m.node)) {
            want(port, rig_label(s, n.rig));
        }
    }
    if wanted.is_empty() {
        return Outcome::ok("The show binds no MIDI device: it plays from the laptop keys and any device you plug in.");
    }
    let Some(listed) = &f.midi_ports else {
        return Outcome::warning("Could not list the MIDI devices, so the ones the show binds were not checked.");
    };
    let missing: Vec<String> = wanted
        .iter()
        .filter(|(p, _)| !listed.contains(p))
        .map(|(p, users)| format!("{} ({})", port_label(p), users.join(", ")))
        .collect();
    if missing.is_empty() {
        let names: Vec<String> = wanted.iter().map(|(p, _)| port_label(p)).collect();
        return Outcome::ok(format!("Plugged in: {}.", names.join(", ")));
    }
    Outcome::problem(format!(
        "Not plugged in: {}. Plug {} in, or choose another device in the Matrix view.",
        missing.join("; "),
        if missing.len() == 1 { "it" } else { "them" }
    ))
}

fn audio_output(s: &Session, f: &Facts) -> Outcome {
    if f.audio_running == Some(false) {
        return Outcome::problem("No audio output is running: choose an output in Settings ▸ Audio, then check again.");
    }
    let Some(outputs) = &f.outputs else {
        return Outcome::warning("Could not list the audio outputs: is PipeWire running?");
    };
    let label = |raw: &str| outputs.iter().find(|o| o.raw == raw).map_or_else(|| crate::audio_in::pretty_device_name(raw), |o| o.name.clone());
    let mut problems = Vec::new();
    let main = match &f.chosen_output {
        Some(raw) if outputs.iter().any(|o| &o.raw == raw) => Some(format!("Plays to {}.", label(raw))),
        Some(raw) => {
            problems.push(format!("The chosen output {} is not connected: plug it in, or choose another in Settings ▸ Audio.", label(raw)));
            None
        }
        None => match outputs.iter().find(|o| o.default) {
            Some(o) => Some(format!("Plays to the system default, {}.", o.name)),
            None => {
                problems.push("No audio output is connected: plug in a sound card or headphones.".to_string());
                None
            }
        },
    };
    for out in s.bus_outs() {
        let sink = out.sink.as_str();
        if sink.is_empty() || outputs.iter().any(|o| o.raw == sink) {
            continue;
        }
        let bus = match out.bus {
            SendSource::Rig(r) => rig_label(s, r),
            SendSource::Node(id) => s.graph.node(id).map_or_else(|| format!("Bus {id}"), |n| node_label(s, n)),
        };
        problems.push(format!("{bus} plays to {}, which is not connected: plug it in, or send it back to Master in the Mixer.", label(sink)));
    }
    if problems.is_empty() {
        Outcome::ok(main.unwrap_or_default())
    } else {
        Outcome::problem(problems.join(" "))
    }
}

fn audio_inputs(s: &Session, f: &Facts) -> Outcome {
    let units: Vec<(&Node, Option<&String>)> = s
        .graph
        .nodes
        .iter()
        .filter_map(|n| match &n.kind {
            NodeKind::AudioIn { device, .. } => Some((n, device.as_ref())),
            _ => None,
        })
        .collect();
    if units.is_empty() {
        return Outcome::ok("The show uses no audio input.");
    }
    let Some(inputs) = &f.inputs else {
        return Outcome::warning("Could not list the audio inputs: is PipeWire running?");
    };
    let mut found = Vec::new();
    let mut missing = Vec::new();
    for (n, device) in units {
        let hit = match device {
            Some(raw) => inputs.iter().find(|i| &i.raw == raw),
            None => inputs.iter().find(|i| i.default).or(inputs.first()),
        };
        match (hit, device) {
            (Some(i), _) => found.push(format!("{} for {}", i.name, node_label(s, n))),
            (None, Some(raw)) => missing.push(format!("{} records from {}", node_label(s, n), crate::audio_in::pretty_device_name(raw))),
            (None, None) => missing.push(format!("{} records from the default input", node_label(s, n))),
        }
    }
    if missing.is_empty() {
        return Outcome::ok(format!("Connected: {}.", found.join(", ")));
    }
    Outcome::problem(format!(
        "{}, which is not connected. Plug it in, or choose another input on the unit.",
        missing.join("; ")
    ))
}

/// A plugin a node needs.
#[derive(Clone, Debug, PartialEq)]
enum Plugin {
    Clap(String),
    Lv2(String),
}

fn needed_plugin(n: &Node) -> Option<Plugin> {
    match &n.kind {
        NodeKind::Clap { plugin_id } => Some(Plugin::Clap(clap_plugin_key(plugin_id.as_str()).to_string())),
        NodeKind::Lv2 { uri } | NodeKind::MidiFx { plugin: MidiFxPlugin::Lv2 { uri } } => Some(Plugin::Lv2(uri.as_str().to_string())),
        _ => None,
    }
}

/// Nodes whose plugin the scan does not list.
fn not_installed<'a>(s: &'a Session, installed: &Installed) -> Vec<&'a Node> {
    s.graph.nodes.iter().filter(|n| needed_plugin(n).is_some_and(|p| !installed.has(&p))).collect()
}

fn plugins(s: &Session, f: &Facts) -> Outcome {
    let hosted = s.graph.nodes.iter().filter(|n| needed_plugin(n).is_some()).count();
    if hosted == 0 {
        return Outcome::ok("The show hosts no plugin.");
    }
    let Some(installed) = &f.plugins else {
        return Outcome::warning("The plugin scan has not finished: check again in a moment.");
    };
    let mut problems = Vec::new();
    let missing: Vec<String> = not_installed(s, installed).into_iter().map(|n| node_label(s, n)).collect();
    if !missing.is_empty() {
        problems.push(format!(
            "Not installed: {}. Install {} in the Plugins view, or take {} out of the rig.",
            missing.join(", "),
            if missing.len() == 1 { "it" } else { "them" },
            if missing.len() == 1 { "it" } else { "them" }
        ));
    }
    for (node, why) in f.probe.iter().flat_map(|p| &p.failed) {
        if let Some(n) = s.graph.node(*node) {
            problems.push(format!("{} is installed but did not load: {why}.", node_label(s, n)));
        }
    }
    if !problems.is_empty() {
        return Outcome::problem(problems.join(" "));
    }
    let what = if hosted == 1 { "The plugin".to_string() } else { format!("The {hosted} plugins") };
    match f.probe {
        Some(_) => Outcome::ok(format!("{what} of the show {} installed and load{}.", if hosted == 1 { "is" } else { "are" }, if hosted == 1 { "s" } else { "" })),
        None => Outcome::ok(format!("{what} of the show {} installed.", if hosted == 1 { "is" } else { "are" })),
    }
}

fn rigs_sound(s: &Session, f: &Facts) -> Outcome {
    let Some(probe) = &f.probe else {
        return Outcome::warning(if f.plugins.is_none() {
            "The rigs were not probed: the plugin scan has not finished."
        } else {
            "The rigs were not probed."
        });
    };
    // A rig with a plugin that is missing or did not load is under Plugins.
    let broken = |rig: u8| {
        s.graph.nodes.iter().any(|n| {
            n.rig == rig
                && (probe.failed.iter().any(|(id, _)| *id == n.id)
                    || f.plugins.as_ref().is_some_and(|i| needed_plugin(n).is_some_and(|p| !i.has(&p))))
        })
    };
    let silent: Vec<String> = probe.rigs.iter().filter(|(r, peak)| *peak < HEARD && !broken(*r)).map(|(r, _)| rig_label(s, *r)).collect();
    if !silent.is_empty() {
        return Outcome::problem(format!(
            "{} made no sound when {} demo phrase played: check {} instrument's level, the cables to Master and the strip in the Mixer.",
            silent.join(", "),
            if silent.len() == 1 { "its" } else { "their" },
            if silent.len() == 1 { "its" } else { "each" }
        ));
    }
    let heard: Vec<String> = probe
        .rigs
        .iter()
        .filter(|(r, _)| !broken(*r))
        .map(|(r, peak)| format!("{} ({:.0} dBFS)", rig_label(s, *r), 20.0 * peak.log10()))
        .collect();
    if s.master_mute && !heard.is_empty() {
        return Outcome::problem("Every rig sounds, but kabelsalat's master mix is muted: unmute it in the Mixer.");
    }
    let skipped: Vec<String> = probe.rigs.iter().filter(|(r, _)| broken(*r)).map(|(r, _)| rig_label(s, *r)).collect();
    let mut text = if heard.is_empty() && skipped.is_empty() {
        "No rig has an instrument to play.".to_string()
    } else if heard.is_empty() {
        String::new()
    } else {
        format!("Sound when played: {}.", heard.join(", "))
    };
    if !skipped.is_empty() {
        text += &format!(
            "{}{} not probed: a plugin is missing (see Plugins).",
            if text.is_empty() { "" } else { " " },
            skipped.join(", ")
        );
    }
    Outcome::ok(text)
}

fn backing_tracks(s: &Session, f: &Facts) -> Outcome {
    let tracks: Vec<(usize, &str, &str)> = s
        .setlist
        .songs
        .iter()
        .enumerate()
        .filter_map(|(i, song)| song.backing.as_deref().map(|p| (i, song.name.as_str(), p)))
        .collect();
    if tracks.is_empty() {
        return Outcome::ok("No song plays a backing track.");
    }
    let mut problems = Vec::new();
    let mut seconds = 0.0;
    for (i, song, path) in &tracks {
        match f.files.iter().find(|(p, _)| p == path).map(|(_, fact)| fact) {
            Some(FileFact::Audio { seconds: s }) => seconds += s,
            Some(FileFact::Unreadable(why)) => problems.push(format!("Song {} “{song}” plays {path}, which cannot be played ({why})", i + 1)),
            Some(FileFact::Missing) | None => problems.push(format!("Song {} “{song}” plays {path}, which is missing", i + 1)),
        }
    }
    if !problems.is_empty() {
        return Outcome::problem(format!("{}. Put the file back, or give the song another backing track.", problems.join("; ")));
    }
    let n = tracks.len();
    let total = seconds.round() as u64;
    Outcome::ok(format!(
        "{} there ({}:{:02} in all).",
        if n == 1 { "The backing track is".to_string() } else { format!("All {n} backing tracks are") },
        total / 60,
        total % 60
    ))
}

/// Bytes per minute of a master take: stereo 32-bit float.
fn take_bytes_per_minute(sample_rate: u32) -> f64 {
    sample_rate as f64 * 2.0 * 4.0 * 60.0
}

fn disk(_: &Session, f: &Facts) -> Outcome {
    let Some(d) = &f.disk else {
        return Outcome::warning("Could not read the free space of the recording folder.");
    };
    let rate = f.render.as_ref().map_or(48_000, |r| r.sample_rate.max(1));
    let minutes = d.free_bytes as f64 / take_bytes_per_minute(rate);
    let free = format!("{:.1} GB free in {}", d.free_bytes as f64 / 1e9, d.dir);
    if minutes < 30.0 {
        Outcome::problem(format!("Only {free}: about {minutes:.0} minutes of recording. Free some space before the show."))
    } else if minutes < 120.0 {
        Outcome::warning(format!("{free}: about {minutes:.0} minutes of recording, less than a two-hour show."))
    } else {
        Outcome::ok(format!("{free}: about {:.0} hours of recording.", minutes / 60.0))
    }
}

fn render(_: &Session, f: &Facts) -> Outcome {
    let Some(r) = &f.render else {
        return Outcome::warning("The render time was not measured.");
    };
    let buffer_ms = r.buffer_frames as f64 / r.sample_rate.max(1) as f64 * 1000.0;
    let load = r.avg_us / (buffer_ms * 10.0);
    let worst = r.max_us / (buffer_ms * 10.0);
    let setup = format!("{} frames at {} kHz ({buffer_ms:.1} ms)", r.buffer_frames, r.sample_rate as f64 / 1000.0);
    let took = format!("rendering takes {load:.0} % of a buffer, {worst:.0} % at worst");
    match LoadLevel::of(load) {
        LoadLevel::Critical => {
            return Outcome::problem(format!("{setup}: {took}, so expect dropouts. Raise the buffer in Settings ▸ Audio, or take out a heavy plugin."));
        }
        LoadLevel::High => {
            return Outcome::warning(format!("{setup}: {took}, little headroom for a busy song. A larger buffer in Settings ▸ Audio gives more."));
        }
        LoadLevel::Normal => {}
    }
    // One slow buffer (a patch load, say) is a late block, not a full load.
    if worst > 100.0 {
        return Outcome::warning(format!(
            "{setup}: {took}: one buffer ran late. Reset the peaks in Engine Info and watch them while you play; a larger buffer gives more room."
        ));
    }
    if let Some(x) = r.xruns.filter(|&x| x > 0) {
        return Outcome::warning(format!(
            "{setup}: {took}, but the output dropped out {x} time{} since the start. Reset the peaks in Engine Info and watch it while you play.",
            if x == 1 { "" } else { "s" }
        ));
    }
    if buffer_ms > 20.0 {
        return Outcome::warning(format!("{setup}: {took}. That much latency is felt under the fingers; try a smaller buffer in Settings ▸ Audio."));
    }
    Outcome::ok(format!("{setup}: {took}."))
}

fn power(_: &Session, f: &Facts) -> Outcome {
    match f.power {
        None => Outcome::warning("Could not read the power state."),
        Some(Power::NoBattery) => Outcome::ok("No battery: the machine runs on mains power."),
        Some(Power::Mains { battery: Some(p) }) => Outcome::ok(format!("On mains power (battery {p} %).")),
        Some(Power::Mains { battery: None }) => Outcome::ok("On mains power."),
        Some(Power::Battery { percent: Some(p) }) if p < 20 => Outcome::problem(format!("On battery at {p} %: plug in the charger now.")),
        Some(Power::Battery { percent }) => Outcome::warning(format!(
            "On battery{}: plug in the charger before the show.",
            percent.map(|p| format!(" ({p} %)")).unwrap_or_default()
        )),
    }
}

// ---------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------

fn rig_count(s: &Session) -> usize {
    s.graph.nodes.iter().map(|n| n.rig as usize + 1).max().unwrap_or(1)
}

pub fn rig_label(s: &Session, rig: u8) -> String {
    s.rig_names
        .get(rig as usize)
        .filter(|n| !n.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| format!("Rig {}", (b'A' + rig.min(25)) as char))
}

fn node_label(s: &Session, n: &Node) -> String {
    let name = crate::ui::sentence_case(name_str(&n.name));
    if rig_count(s) > 1 {
        format!("{name} ({})", rig_label(s, n.rig))
    } else {
        name
    }
}

/// "FM-1:FM-1 MIDI 1" → "FM-1"; a second port keeps its number.
fn port_label(port: &str) -> String {
    let p = crate::ui::midi_port_name(port);
    if p.port.is_empty() || p.port == "Port 1" {
        p.device
    } else {
        format!("{} {}", p.device, p.port)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::control::{name_bytes, ClapId, Graph, Lv2Uri, PortKind, PortRef};
    use crate::session::MidiSources;
    use crate::setlist::{Part, PartRig, Song};

    const SURGE: &str = crate::control::SURGE_PLUGIN_ID;
    pub(crate) const ODIN: &str = "com.thewavewarden.odin2";
    const FM1: &str = "FM-1:FM-1 MIDI 1";
    const M2: &str = "alsa_output.usb-MOTU_M2_00HF-00.analog-stereo";

    pub(crate) fn rig(g: &mut Graph, rig: u8, synth: NodeKind, name: &str) -> u32 {
        let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let synth = g.add_node(synth, name_bytes(name));
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        let e = g.edges.len() as u32 * 3;
        g.connect_with_id(e + 1, PortRef { node: keys, port: 0 }, PortRef { node: synth, port: 0 }, PortKind::Midi);
        g.connect_with_id(e + 2, PortRef { node: synth, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio);
        for n in &mut g.nodes {
            if [keys, synth, master].contains(&n.id) {
                n.rig = rig;
            }
        }
        synth
    }

    pub(crate) fn clap(id: &str) -> NodeKind {
        NodeKind::Clap { plugin_id: ClapId::new(id) }
    }

    /// Two Surge rigs, an organ rig, a 20-song setlist with a backing track
    /// in song 1, rig A bound to the FM-1, and the Odin unit the third song
    /// needs.
    pub(crate) fn show() -> Session {
        let mut g = Graph::default();
        g.reserve(32, 32);
        let a = rig(&mut g, 0, clap(SURGE), "SURGE");
        let b = rig(&mut g, 1, clap(SURGE), "SURGE");
        rig(&mut g, 2, NodeKind::Lv2 { uri: Lv2Uri::new(crate::organ::B_SYNTH) }, "SETBFREE");
        let mut s = Session::new("Friday gig", g);
        s.rig_names = vec!["Piano".into(), "Pads".into(), "Organ".into()];
        s.set_hw_ports([(0, FM1.to_string())].into_iter().collect());
        s.rig_devices = vec![Some(0)];
        s.set_midi_sources(MidiSources { node: a, keyboard: false, hardware: true, device: Some(FM1.into()) });
        let _ = b;
        for i in 0..20u8 {
            let mut song = Song::new(format!("Song {}", i + 1), vec![Part::new("Main", vec![PartRig::new(i % 3)])]);
            if i == 0 {
                song.backing = Some("/gig/intro.wav".into());
            }
            s.setlist.songs.push(song);
        }
        s
    }

    pub(crate) fn healthy(s: &Session) -> Facts {
        Facts {
            midi_ports: Some(vec![FM1.into(), "MIDIMATE eX:MIDIMATE eX MIDI 1".into()]),
            outputs: Some(vec![OutputDevice { raw: M2.into(), name: "M2".into(), default: true }]),
            chosen_output: Some(M2.into()),
            audio_running: Some(true),
            inputs: Some(vec![InputDevice { raw: "alsa_input.usb-MOTU_M2".into(), name: "M2".into(), channels: 2, default: true }]),
            plugins: Some(Installed { clap: vec![SURGE.into()], lv2: vec![(crate::organ::B_SYNTH.into(), true)] }),
            probe: Some(Probe { rigs: (0..rig_count(s) as u8).map(|r| (r, 0.3)).collect(), failed: vec![] }),
            files: vec![("/gig/intro.wav".into(), FileFact::Audio { seconds: 245.0 })],
            disk: Some(Disk { dir: "~/Music/kabelsalat".into(), free_bytes: 80_000_000_000 }),
            render: Some(Render { sample_rate: 48_000, buffer_frames: 256, avg_us: 900.0, max_us: 2100.0, xruns: Some(0) }),
            power: Some(Power::Mains { battery: Some(97) }),
        }
    }

    fn row<'a>(rows: &'a [Row], id: &str) -> &'a Row {
        rows.iter().find(|r| r.id == id).unwrap()
    }

    fn problems(rows: &[Row]) -> Vec<&'static str> {
        rows.iter().filter(|r| r.status == Status::Problem).map(|r| r.id).collect()
    }

    #[test]
    fn a_healthy_twenty_song_show_passes_every_check() {
        let s = show();
        let t = std::time::Instant::now();
        let report = Report::of(&s, &healthy(&s), t.elapsed());
        assert!(report.rows.iter().all(|r| r.status == Status::Ok), "{}", report.text());
        assert_eq!(report.summary(), "All 9 checks passed");
        assert_eq!(report.songs, 20);
        assert!(t.elapsed() < std::time::Duration::from_millis(100), "the checks themselves are pure and quick");
        assert_eq!(row(&report.rows, "midi_devices").text, "Plugged in: FM-1.");
        assert_eq!(row(&report.rows, "backing_tracks").text, "The backing track is there (4:05 in all).");
    }

    #[test]
    fn a_missing_device_plugin_and_backing_track_are_exactly_the_three_problems() {
        let mut s = show();
        let odin = rig(&mut s.graph, 3, clap(ODIN), "ODIN 2");
        s.rig_names.push("Lead".into());
        let mut f = healthy(&s);
        f.midi_ports = Some(vec!["MIDIMATE eX:MIDIMATE eX MIDI 1".into()]);
        f.files = vec![("/gig/intro.wav".into(), FileFact::Missing)];
        // The probe never loads a plugin that is not installed; its rig
        // plays nothing.
        f.probe.as_mut().unwrap().rigs[3].1 = 0.0;
        let rows = run(&s, &f);
        assert_eq!(problems(&rows), ["midi_devices", "plugins", "backing_tracks"], "{rows:#?}");
        assert!(rows.iter().all(|r| r.status != Status::Warning));
        assert_eq!(row(&rows, "midi_devices").text, "Not plugged in: FM-1 (Piano). Plug it in, or choose another device in the Matrix view.");
        assert!(row(&rows, "plugins").text.starts_with("Not installed: Odin 2 (Lead)."), "{}", row(&rows, "plugins").text);
        assert_eq!(row(&rows, "backing_tracks").text, "Song 1 “Song 1” plays /gig/intro.wav, which is missing. Put the file back, or give the song another backing track.");
        assert!(row(&rows, "rigs_sound").text.ends_with("Lead not probed: a plugin is missing (see Plugins)."), "{}", row(&rows, "rigs_sound").text);
        let _ = odin;
    }

    #[test]
    fn the_check_never_changes_the_session() {
        let s = show();
        let before = serde_json::to_string(&s.setlist).unwrap();
        let mut f = healthy(&s);
        f.midi_ports = Some(vec![]);
        let _ = run(&s, &f);
        assert_eq!(serde_json::to_string(&s.setlist).unwrap(), before);
    }

    #[test]
    fn a_device_list_that_is_unknown_is_a_warning_not_a_pass() {
        let s = show();
        let mut f = healthy(&s);
        f.midi_ports = None;
        assert_eq!(row(&run(&s, &f), "midi_devices").status, Status::Warning);
        let bare = Session::new("Bare", crate::control::standard_rig_graph(true));
        assert_eq!(row(&run(&bare, &f), "midi_devices").status, Status::Ok, "a show that binds no device needs none");
    }

    #[test]
    fn a_chosen_output_that_is_gone_or_a_stopped_engine_is_a_problem() {
        let s = show();
        let mut f = healthy(&s);
        f.chosen_output = Some("alsa_output.usb-Focusrite_Scarlett_2i2".into());
        let r = run(&s, &f);
        assert_eq!(row(&r, "audio_output").status, Status::Problem);
        assert!(row(&r, "audio_output").text.contains("Focusrite Scarlett is not connected"), "{}", row(&r, "audio_output").text);
        f.chosen_output = None;
        assert_eq!(row(&run(&s, &f), "audio_output").text, "Plays to the system default, M2.");
        f.audio_running = Some(false);
        assert_eq!(row(&run(&s, &f), "audio_output").status, Status::Problem);
    }

    #[test]
    fn a_bus_routed_to_an_unplugged_output_is_a_problem() {
        let mut s = show();
        s.set_bus_outs(vec![crate::control::BusOut { bus: SendSource::Rig(1), sink: crate::control::SinkName::new("alsa_output.usb-Behringer_UMC404HD") }]);
        let r = run(&s, &healthy(&s));
        assert_eq!(row(&r, "audio_output").status, Status::Problem);
        assert!(row(&r, "audio_output").text.starts_with("Pads plays to Behringer UMC404HD"), "{}", row(&r, "audio_output").text);
    }

    #[test]
    fn an_audio_input_that_is_not_connected_is_a_problem() {
        let mut s = show();
        let vocal = s.graph.add_node(
            NodeKind::AudioIn { device: Some("alsa_input.usb-Shure_MV7".into()), channels: crate::control::AudioInChannel::MonoL },
            name_bytes("VOCAL"),
        );
        let f = healthy(&s);
        let r = run(&s, &f);
        assert_eq!(row(&r, "audio_inputs").status, Status::Problem);
        assert!(row(&r, "audio_inputs").text.starts_with("Vocal (Piano) records from Shure MV7"), "{}", row(&r, "audio_inputs").text);
        s.graph.nodes.iter_mut().find(|n| n.id == vocal).unwrap().kind =
            NodeKind::AudioIn { device: None, channels: crate::control::AudioInChannel::MonoL };
        assert_eq!(row(&run(&s, &f), "audio_inputs").text, "Connected: M2 for Vocal (Piano).");
    }

    #[test]
    fn a_plugin_that_is_installed_but_did_not_load_is_a_problem() {
        let s = show();
        let mut f = healthy(&s);
        let surge = s.graph.nodes.iter().find(|n| n.rig == 1 && matches!(n.kind, NodeKind::Clap { .. })).unwrap().id;
        f.probe.as_mut().unwrap().failed.push((surge, "the plugin refused to activate".into()));
        let r = run(&s, &f);
        assert_eq!(row(&r, "plugins").text, "Surge (Pads) is installed but did not load: the plugin refused to activate.");
        assert_eq!(row(&r, "rigs_sound").status, Status::Ok, "its rig is under Plugins, not reported twice");
        f.plugins = None;
        assert_eq!(row(&run(&s, &f), "plugins").status, Status::Warning, "a scan still running is no pass");
    }

    #[test]
    fn a_silent_rig_or_a_muted_master_is_a_problem() {
        let mut s = show();
        let mut f = healthy(&s);
        f.probe.as_mut().unwrap().rigs[1].1 = 0.0002;
        let r = run(&s, &f);
        assert_eq!(row(&r, "rigs_sound").status, Status::Problem);
        assert!(row(&r, "rigs_sound").text.starts_with("Pads made no sound"), "{}", row(&r, "rigs_sound").text);
        f = healthy(&s);
        assert!(row(&run(&s, &f), "rigs_sound").text.starts_with("Sound when played: Piano (-10 dBFS)"));
        s.master_mute = true;
        assert_eq!(row(&run(&s, &f), "rigs_sound").status, Status::Problem);
    }

    #[test]
    fn a_backing_track_the_player_cannot_read_is_a_problem() {
        let s = show();
        let mut f = healthy(&s);
        f.files = vec![("/gig/intro.wav".into(), FileFact::Unreadable("unsupported WAV (format 2, 4-bit)".into()))];
        let r = run(&s, &f);
        assert_eq!(row(&r, "backing_tracks").status, Status::Problem);
        assert!(row(&r, "backing_tracks").text.contains("cannot be played (unsupported WAV"));
    }

    #[test]
    fn little_disk_space_warns_and_almost_none_is_a_problem() {
        let s = show();
        let mut f = healthy(&s);
        f.disk = Some(Disk { dir: "/home".into(), free_bytes: 2_000_000_000 });
        assert_eq!(row(&run(&s, &f), "disk").status, Status::Warning, "2 GB is about 87 minutes");
        f.disk = Some(Disk { dir: "/home".into(), free_bytes: 300_000_000 });
        let r = run(&s, &f);
        assert_eq!(row(&r, "disk").status, Status::Problem);
        assert_eq!(row(&r, "disk").text, "Only 0.3 GB free in /home: about 13 minutes of recording. Free some space before the show.");
    }

    #[test]
    fn render_headroom_follows_the_engines_load_levels() {
        let s = show();
        let mut f = healthy(&s);
        let at = |f: &mut Facts, avg_us: f64, max_us: f64, xruns: u32| {
            f.render = Some(Render { sample_rate: 48_000, buffer_frames: 256, avg_us, max_us, xruns: Some(xruns) });
            row(&run(&s, f), "render").status
        };
        assert_eq!(at(&mut f, 1000.0, 2000.0, 0), Status::Ok);
        assert_eq!(at(&mut f, 4000.0, 4200.0, 0), Status::Warning, "75 % of a 5.3 ms buffer");
        assert_eq!(at(&mut f, 5000.0, 5200.0, 0), Status::Problem, "94 %");
        assert_eq!(at(&mut f, 1000.0, 6000.0, 0), Status::Warning, "one late block at a light load");
        assert_eq!(at(&mut f, 1000.0, 2000.0, 3), Status::Warning, "dropouts already happened");
        f.render = Some(Render { sample_rate: 48_000, buffer_frames: 2048, avg_us: 1000.0, max_us: 2000.0, xruns: None });
        assert_eq!(row(&run(&s, &f), "render").status, Status::Warning, "43 ms of latency");
        f.render = None;
        assert_eq!(row(&run(&s, &f), "render").status, Status::Warning);
    }

    #[test]
    fn running_on_battery_warns_and_a_low_battery_is_a_problem() {
        let s = show();
        let mut f = healthy(&s);
        f.power = Some(Power::Battery { percent: Some(64) });
        assert_eq!(row(&run(&s, &f), "power").status, Status::Warning);
        f.power = Some(Power::Battery { percent: Some(12) });
        assert_eq!(row(&run(&s, &f), "power").text, "On battery at 12 %: plug in the charger now.");
        f.power = Some(Power::NoBattery);
        assert_eq!(row(&run(&s, &f), "power").status, Status::Ok);
    }

    #[test]
    fn the_report_reads_as_one_line_per_check() {
        let s = show();
        let mut f = healthy(&s);
        f.power = Some(Power::Battery { percent: Some(64) });
        let report = Report::of(&s, &f, std::time::Duration::from_millis(3200));
        let text = report.text();
        assert!(text.starts_with("Pre-show check: Friday gig (20 songs) — 1 warning · 8 ok (3.2 s)\n"), "{text}");
        assert_eq!(text.lines().count(), 1 + CHECKS.len());
        assert!(text.contains("  ! Power"));
        assert_eq!(report.json()["checks"][8]["status"], "warning");
    }
}
