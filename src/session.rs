//! Sessions, undo, starter templates (task-8, docs/ARCHITECTURE.md §2b/§7).
//!
//! A session is the control-plane [`Graph`] plus knob/param values, opaque
//! plugin state blobs, and an optional MIDI-matrix config slot. It persists
//! as a `name.ksalat/` *package directory*:
//!
//! ```text
//! My Song.ksalat/
//!   session.json        # format version, name, graph, params, matrix, state refs
//!   state/
//!     node-1.bin        # opaque plugin blobs, one file per Clap node
//! ```
//!
//! Blobs are captured by the caller on the main thread via
//! `ClapVoiceMain::save_state` and handed to [`Session::set_plugin_state`];
//! this module never calls host code. On restore the caller iterates
//! [`Session::plugin_states`] and feeds each blob to `ClapVoiceMain::load_state`.
//!
//! Saves are atomic: a fresh package is staged in a hidden sibling directory
//! (every file written temp-then-renamed and fsynced), then swapped into place
//! with directory renames. An existing package is never written into — it is
//! moved aside, replaced, and only then removed. A failed save leaves the
//! user's original package untouched.
//!
//! Undo/redo is snapshot-based ([`History`]): graph + params + matrix are
//! cloned before each edit; plugin blobs are *not* snapshotted (they are
//! captured on demand at save time).

#![allow(dead_code)] // public API lands in the UI/CLI with task-10 integration

use std::collections::VecDeque;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::control::{
    name_bytes, name_str, DspKind, EdgeId, Graph, NodeId, NodeKind, ParamId, PortKind, PortRef, Rig,
};
// Param ids are per-node; these mirror the engine's current knob table
// (task-10 generalizes it — the session stores opaque (node, param, value)
// triples either way).
use crate::engine::{RigMix, P_LEVEL, P_MIX, P_TIME, P_VOL, P_WARM};

/// On-disk format version. Bump on breaking layout changes; `load` rejects
/// versions it does not understand with a plain-language error.
pub const FORMAT_VERSION: u32 = 1;
/// The manifest inside a package.
pub const SESSION_FILE: &str = "session.json";
/// Subdirectory holding opaque plugin state blobs.
pub const STATE_DIR: &str = "state";
/// Package directory extension.
pub const PACKAGE_EXT: &str = "ksalat";
/// The manifest inside a single-rig preset package.
pub const RIG_PRESET_FILE: &str = "rig.json";
/// Package directory extension for single-rig presets.
pub const RIG_PRESET_EXT: &str = "ksalat-rig";
/// On-disk format version for single-rig preset manifests.
pub const RIG_PRESET_FORMAT_VERSION: u32 = 1;

/// The Surge XT CLAP id — the reference plugin (docs/ARCHITECTURE.md §1), native on ARM.
const SURGE_ID: &str = "org.surge-synth-team.surge-xt";

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// One knob/param value: `SetParam { node, param, value }` at rest.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParamValue {
    pub node: NodeId,
    pub param: ParamId,
    pub value: f64,
}

/// An opaque plugin state blob for one Clap node. `data` is whatever
/// `ClapVoiceMain::save_state` produced — the session never interprets it.
#[derive(Clone, Debug, PartialEq)]
pub struct PluginState {
    pub node: NodeId,
    pub plugin_id: String,
    pub data: Vec<u8>,
}

/// A complete session: the patch, its knob values, plugin state, and the
/// MIDI-matrix config slot (task-9 fills the slot; the session keeps it opaque).
#[derive(Clone, Debug)]
pub struct Session {
    pub name: String,
    pub graph: Graph,
    params: Vec<ParamValue>, // sorted by (node, param), unique
    states: Vec<PluginState>, // sorted by node, unique
    matrix: Option<Value>,
    /// Rig names in rig order (#33); empty = the default "Rig A/B/…".
    pub rig_names: Vec<String>,
}

impl Default for Session {
    fn default() -> Self {
        Self::new("Untitled", Graph::default())
    }
}

impl Session {
    pub fn new(name: impl Into<String>, graph: Graph) -> Self {
        Self {
            name: name.into(),
            graph,
            params: Vec::new(),
            states: Vec::new(),
            matrix: None,
            rig_names: Vec::new(),
        }
    }

    // -- knobs / params ----------------------------------------------------

    pub fn param(&self, node: NodeId, param: ParamId) -> Option<f64> {
        self.params
            .iter()
            .find(|p| p.node == node && p.param == param)
            .map(|p| p.value)
    }

    /// Upsert a knob value (the persisted form of `Command::SetParam`).
    pub fn set_param(&mut self, node: NodeId, param: ParamId, value: f64) {
        match self
            .params
            .iter_mut()
            .find(|p| p.node == node && p.param == param)
        {
            Some(p) => p.value = value,
            None => {
                self.params.push(ParamValue { node, param, value });
                self.params.sort_by_key(|p| (p.node, p.param));
            }
        }
    }

    pub fn params(&self) -> &[ParamValue] {
        &self.params
    }

    // -- plugin state blobs --------------------------------------------------

    pub fn plugin_state(&self, node: NodeId) -> Option<&PluginState> {
        self.states.iter().find(|s| s.node == node)
    }

    pub fn plugin_states(&self) -> &[PluginState] {
        &self.states
    }

    /// Store a blob captured by the caller (main thread,
    /// `ClapVoiceMain::save_state`). Replaces any previous blob for the node.
    pub fn set_plugin_state(
        &mut self,
        node: NodeId,
        plugin_id: impl Into<String>,
        data: Vec<u8>,
    ) {
        let plugin_id = plugin_id.into();
        match self.states.iter_mut().find(|s| s.node == node) {
            Some(s) => {
                s.plugin_id = plugin_id;
                s.data = data;
            }
            None => {
                self.states.push(PluginState {
                    node,
                    plugin_id,
                    data,
                });
                self.states.sort_by_key(|s| s.node);
            }
        }
    }

    // -- MIDI matrix slot (task-9) -------------------------------------------

    /// Opaque matrix config. task-9 serializes its routing table here; the
    /// session persists it verbatim without understanding it.
    pub fn matrix(&self) -> Option<&Value> {
        self.matrix.as_ref()
    }

    pub fn set_matrix(&mut self, matrix: Option<Value>) {
        self.matrix = matrix;
    }

    // -- integrity -------------------------------------------------------------

    /// Drop params/blobs whose node no longer exists (call after `RemoveNode`).
    pub fn prune_to_graph(&mut self) {
        self.params.retain(|p| self.graph.node(p.node).is_some());
        self.states.retain(|s| self.graph.node(s.node).is_some());
    }

    /// Plain-language integrity check, run on save and load: every edge must
    /// reference real nodes, ids must be unique, params/blobs must belong to
    /// existing nodes, and blobs must match their node's plugin.
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            bail!("The session needs a name before it can be saved.");
        }
        let mut node_ids: Vec<NodeId> = self.graph.nodes.iter().map(|n| n.id).collect();
        node_ids.sort_unstable();
        if node_ids.windows(2).any(|w| w[0] == w[1]) {
            bail!("The patch contains duplicate node ids — it cannot be saved.");
        }
        let mut edge_ids: Vec<EdgeId> = self.graph.edges.iter().map(|e| e.id).collect();
        edge_ids.sort_unstable();
        if edge_ids.windows(2).any(|w| w[0] == w[1]) {
            bail!("The patch contains duplicate cable ids — it cannot be saved.");
        }
        for e in &self.graph.edges {
            if e.from.node == e.to.node {
                bail!("A cable plugs a node into itself — unplug it and try again.");
            }
            if self.graph.node(e.from.node).is_none() || self.graph.node(e.to.node).is_none() {
                bail!(
                    "Cable {} references a node that no longer exists.",
                    e.id
                );
            }
        }
        for p in &self.params {
            if self.graph.node(p.node).is_none() {
                bail!(
                    "A knob value belongs to node {} which is no longer in the patch. \
                     Call prune_to_graph() after removing nodes.",
                    p.node
                );
            }
            if !p.value.is_finite() {
                bail!("A knob value is not a finite number (node {}).", p.node);
            }
        }
        for s in &self.states {
            match self.graph.node(s.node) {
                Some(n) => match &n.kind {
                    NodeKind::Clap { plugin_id } if name_str(plugin_id) == s.plugin_id => {}
                    NodeKind::Clap { plugin_id } => bail!(
                        "The state blob for node {} was saved by '{}' but the node now \
                         hosts '{}'.",
                        s.node,
                        s.plugin_id,
                        name_str(plugin_id)
                    ),
                    NodeKind::Lv2 { uri } if uri.as_str() == s.plugin_id => {}
                    NodeKind::Lv2 { uri } => bail!(
                        "The state blob for node {} was saved by '{}' but the node now \
                         hosts '{}'.",
                        s.node,
                        s.plugin_id,
                        uri.as_str()
                    ),
                    _ => bail!(
                        "Node {} has a plugin state blob but is not a plugin.",
                        s.node
                    ),
                },
                None => bail!(
                    "A plugin state blob belongs to node {} which is no longer in the \
                     patch. Call prune_to_graph() after removing nodes.",
                    s.node
                ),
            }
            if s.plugin_id.is_empty() {
                bail!("The state blob for node {} has no plugin id.", s.node);
            }
        }
        Ok(())
    }

    // -- package I/O -----------------------------------------------------------

    /// `parent/name.ksalat` with a plain-language check on the name.
    pub fn package_path(parent: &Path, name: &str) -> Result<PathBuf> {
        validate_package_path(parent, name, PACKAGE_EXT, "session")
    }

    /// Atomically save into the package directory `package` (e.g.
    /// `~/sessions/My Song.ksalat`). The previous package, if any, is replaced
    /// wholesale and is never modified in place; on any error it stays intact.
    pub fn save(&self, package: &Path) -> Result<()> {
        self.validate()?;
        stage_package(package, "session", |staging| self.write_package(staging))
    }

    /// Write manifest + blobs into a fresh staging directory. Files are
    /// written temp-then-renamed and fsynced; the manifest goes last so a
    /// half-written staging dir is never a valid package.
    fn write_package(&self, dir: &Path) -> Result<()> {
        let refs = write_state_files(dir, &self.states)?;
        let manifest = SessionFile {
            format: FORMAT_VERSION,
            name: self.name.clone(),
            graph: self.graph.clone(),
            params: self.params.clone(),
            matrix: self.matrix.clone(),
            states: refs,
            rig_names: self.rig_names.clone(),
        };
        let json = serde_json::to_vec_pretty(&manifest)?;
        write_atomic(&dir.join(SESSION_FILE), &json)?;
        Ok(())
    }

    /// Load a package. The graph, knobs, matrix slot, and blobs come back
    /// exactly as saved; the caller restores blobs via `ClapVoiceMain::load_state`
    /// and re-issues knob values as `SetParam` commands.
    pub fn load(package: &Path) -> Result<Session> {
        let manifest_path = package.join(SESSION_FILE);
        let text = fs::read_to_string(&manifest_path).with_context(|| {
            format!(
                "Could not read {} — is this a kabelsalat session package?",
                manifest_path.display()
            )
        })?;
        let manifest: SessionFile = serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid session JSON.", manifest_path.display()))?;
        if manifest.format != FORMAT_VERSION {
            bail!(
                "This session uses format version {} but this build understands version {}. \
                 It may have been written by a newer kabelsalat.",
                manifest.format,
                FORMAT_VERSION
            );
        }
        let states = read_state_files(package, &manifest.states, "session")?;
        let session = Session {
            name: manifest.name,
            graph: manifest.graph,
            params: manifest.params,
            states,
            matrix: manifest.matrix,
            rig_names: manifest.rig_names,
        };
        session.validate()?;
        Ok(session)
    }
}

fn validate_package_path(parent: &Path, name: &str, ext: &str, kind: &str) -> Result<PathBuf> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed == "."
        || trimmed == ".."
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains('\0')
    {
        bail!("'{name}' is not a usable {kind} name — avoid slashes and dots-only names.");
    }
    Ok(parent.join(format!("{trimmed}.{ext}")))
}

fn stage_package(package: &Path, kind: &str, write: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    let basename = package
        .file_name()
        .and_then(|s| s.to_str())
        .with_context(|| format!("The {kind} package path has no usable directory name."))?;
    let parent = package.parent().unwrap_or(Path::new(""));
    fs::create_dir_all(parent)
        .with_context(|| format!("Could not create the folder {}.", parent.display()))?;
    if package.exists() && !package.is_dir() {
        bail!(
            "{} already exists and is not a {kind} package — choose another name.",
            package.display()
        );
    }
    let staging = parent.join(format!(".{basename}.staging-{}", unique_suffix()));
    fs::create_dir(&staging)
        .with_context(|| format!("Could not stage the {kind} at {}.", staging.display()))?;
    let result = write(&staging).and_then(|()| swap_package(package, &staging));
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn write_state_files(dir: &Path, states: &[PluginState]) -> Result<Vec<StateRef>> {
    let mut refs = Vec::new();
    if !states.is_empty() {
        let state_dir = dir.join(STATE_DIR);
        fs::create_dir(&state_dir)?;
        for s in states {
            let rel = format!("{STATE_DIR}/node-{}.bin", s.node);
            write_atomic(&dir.join(&rel), &s.data)
                .with_context(|| format!("Could not write plugin state for node {}.", s.node))?;
            refs.push(StateRef {
                node: s.node,
                plugin_id: s.plugin_id.clone(),
                file: rel,
            });
        }
    }
    Ok(refs)
}

fn read_state_files(package: &Path, refs: &[StateRef], kind: &str) -> Result<Vec<PluginState>> {
    let mut states = Vec::new();
    for r in refs {
        let rel = safe_relative(&r.file)?;
        let data = fs::read(package.join(&rel)).with_context(|| {
            format!(
                "The {kind} is missing its plugin state file '{}' (node {}).",
                r.file, r.node
            )
        })?;
        states.push(PluginState {
            node: r.node,
            plugin_id: r.plugin_id.clone(),
            data,
        });
    }
    Ok(states)
}

// ---------------------------------------------------------------------------
// On-disk manifest (blobs live in separate files, referenced by path)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct SessionFile {
    format: u32,
    name: String,
    graph: Graph,
    #[serde(default)]
    params: Vec<ParamValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    matrix: Option<Value>,
    #[serde(default)]
    states: Vec<StateRef>,
    /// Rig names (older sessions have none → default names).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    rig_names: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct StateRef {
    node: NodeId,
    plugin_id: String,
    file: String,
}

/// Reject absolute paths and anything with `..`/root/prefix components so a
/// hand-edited manifest can never reach outside the package.
fn safe_relative(file: &str) -> Result<PathBuf> {
    let p = Path::new(file);
    if file.is_empty() || p.is_absolute() {
        bail!("The session references an invalid state file path '{file}'.");
    }
    if !p
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
    {
        bail!("The session state file '{file}' points outside the package — refusing to load it.");
    }
    Ok(p.to_path_buf())
}

// ---------------------------------------------------------------------------
// Atomic file writes
// ---------------------------------------------------------------------------

fn unique_suffix() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed),
        nanos
    )
}

/// Write `bytes` to `path` via a unique temp file + fsync + rename, so readers
/// never see a partial file. Cleans up the temp file on failure.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .context("atomic write target has no file name")?;
    let tmp = path.with_file_name(format!(".{name}.tmp-{}", unique_suffix()));
    let write = || -> Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    };
    let result = write();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Best-effort directory fsync so the rename itself is durable.
fn sync_dir(dir: &Path) {
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
}

/// Swap a fully written staging package into place. The previous package, if
/// any, is moved aside first and only removed once the new one is live; on a
/// failed swap the original is restored. Shared by [`Session::save`] and
/// [`MultiRigSession::save`].
fn swap_package(package: &Path, staging: &Path) -> Result<()> {
    let basename = package
        .file_name()
        .and_then(|s| s.to_str())
        .context("The session package path has no usable directory name.")?;
    let parent = package.parent().unwrap_or(Path::new(""));
    if package.exists() {
        let backup = parent.join(format!(".{basename}.old-{}", unique_suffix()));
        fs::rename(package, &backup).with_context(|| {
            format!("Could not move the old session aside ({}).", package.display())
        })?;
        match fs::rename(staging, package) {
            Ok(()) => {
                // The new package is live; old-copy cleanup is best-effort.
                let _ = fs::remove_dir_all(&backup);
            }
            Err(e) => {
                // Put the original back before reporting the failure.
                let _ = fs::rename(&backup, package);
                return Err(e).with_context(|| {
                    format!("Could not put the new session at {}.", package.display())
                });
            }
        }
    } else {
        fs::rename(staging, package).with_context(|| {
            format!("Could not put the new session at {}.", package.display())
        })?;
    }
    sync_dir(parent);
    Ok(())
}

// ---------------------------------------------------------------------------
// Multi-rig sessions (task-22): Vec<Rig> + active, same atomic package I/O
// ---------------------------------------------------------------------------

/// The manifest inside a multi-rig package.
pub const MULTI_RIG_FILE: &str = "rigs.json";
/// On-disk format version for multi-rig manifests.
pub const MULTI_RIG_FORMAT_VERSION: u32 = 1;

/// A multi-rig session: several independent rigs (each a full rack graph bound
/// to a hardware MIDI device) plus the UI-selected rig index. Persists as a
/// `name.ksalat/` package with a `rigs.json` manifest, atomically like
/// [`Session`]. Single-graph [`Session`] packages keep working unchanged;
/// [`MultiRigSession::load`] falls back to wrapping one as a single rig.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MultiRigSession {
    pub name: String,
    pub rigs: Vec<Rig>,
    #[serde(default)]
    pub active: usize,
}

impl MultiRigSession {
    pub fn new(name: impl Into<String>, rigs: Vec<Rig>, active: usize) -> Self {
        Self { name: name.into(), rigs, active }
    }

    /// The merged patch the engine plays (`Command::LoadRigs` does this too).
    pub fn merged_graph(&self) -> Graph {
        crate::control::merge_rigs(&self.rigs)
    }

    /// Plain-language integrity check, run on save and load.
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            bail!("The session needs a name before it can be saved.");
        }
        if self.rigs.is_empty() {
            bail!("A multi-rig session needs at least one rig.");
        }
        if self.active >= self.rigs.len() {
            bail!(
                "The active rig index {} is out of range ({} rigs).",
                self.active,
                self.rigs.len()
            );
        }
        for (i, rig) in self.rigs.iter().enumerate() {
            if rig.name.trim().is_empty() {
                bail!("Rig {} needs a name before the session can be saved.", i + 1);
            }
            let g = &rig.graph;
            let mut node_ids: Vec<NodeId> = g.nodes.iter().map(|n| n.id).collect();
            node_ids.sort_unstable();
            if node_ids.windows(2).any(|w| w[0] == w[1]) {
                bail!("Rig '{}' contains duplicate node ids.", rig.name);
            }
            let mut edge_ids: Vec<EdgeId> = g.edges.iter().map(|e| e.id).collect();
            edge_ids.sort_unstable();
            if edge_ids.windows(2).any(|w| w[0] == w[1]) {
                bail!("Rig '{}' contains duplicate cable ids.", rig.name);
            }
            for e in &g.edges {
                if e.from.node == e.to.node {
                    bail!("Rig '{}': a cable plugs a node into itself.", rig.name);
                }
                if g.node(e.from.node).is_none() || g.node(e.to.node).is_none() {
                    bail!(
                        "Rig '{}': cable {} references a node that no longer exists.",
                        rig.name,
                        e.id
                    );
                }
            }
        }
        Ok(())
    }

    /// Atomically save into the package directory `package`, mirroring
    /// [`Session::save`]: staged in a hidden sibling, swapped by rename, the
    /// previous package untouched on any error.
    pub fn save(&self, package: &Path) -> Result<()> {
        self.validate()?;
        stage_package(package, "session", |staging| {
            let manifest = MultiRigFile {
                format: MULTI_RIG_FORMAT_VERSION,
                session: self.clone(),
            };
            let json = serde_json::to_vec_pretty(&manifest)?;
            write_atomic(&staging.join(MULTI_RIG_FILE), &json)
        })
    }

    /// Load a multi-rig package. Falls back to a single-graph [`Session`]
    /// package (wrapped as one rig with no device binding) so old sessions
    /// open in multi-rig mode.
    pub fn load(package: &Path) -> Result<MultiRigSession> {
        let manifest_path = package.join(MULTI_RIG_FILE);
        if !manifest_path.exists() {
            let s = Session::load(package)?;
            return Ok(MultiRigSession {
                name: s.name,
                rigs: vec![Rig::new("Rig 1", s.graph, None)],
                active: 0,
            });
        }
        let text = fs::read_to_string(&manifest_path).with_context(|| {
            format!(
                "Could not read {} — is this a kabelsalat multi-rig package?",
                manifest_path.display()
            )
        })?;
        let manifest: MultiRigFile = serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid multi-rig JSON.", manifest_path.display()))?;
        if manifest.format != MULTI_RIG_FORMAT_VERSION {
            bail!(
                "This multi-rig session uses format version {} but this build understands \
                 version {}. It may have been written by a newer kabelsalat.",
                manifest.format,
                MULTI_RIG_FORMAT_VERSION
            );
        }
        manifest.session.validate()?;
        Ok(manifest.session)
    }
}

/// On-disk multi-rig manifest (rig graphs are self-contained; plugin state
/// blobs are captured per rig by the caller, like single-graph sessions).
#[derive(Serialize, Deserialize)]
struct MultiRigFile {
    format: u32,
    #[serde(flatten)]
    session: MultiRigSession,
}

// ---------------------------------------------------------------------------
// Single-rig presets (#34): one rig's patch, knobs, blobs, matrix, mix, name
// ---------------------------------------------------------------------------

/// Per-CLAP-node input switches and loaded patch display name inside a [`RigPreset`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RigClapMeta {
    pub node: NodeId,
    #[serde(default = "default_true")]
    pub keyboard: bool,
    #[serde(default = "default_true")]
    pub hardware: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch_name: Option<String>,
}

fn default_true() -> bool {
    true
}

/// A saved single-rig configuration: one rig-relative [`Session`] (`node.rig == 0`
/// for every node) plus the rig's display name, mixer settings, hardware MIDI
/// device binding, and per-synth source switches / patch name.
#[derive(Clone, Debug)]
pub struct RigPreset {
    pub name: String,
    pub rig_name: String,
    pub session: Session,
    pub mix: RigMix,
    pub midi_device: Option<u32>,
    pub clap_meta: Vec<RigClapMeta>,
}

impl RigPreset {
    pub fn new(name: impl Into<String>, rig_name: impl Into<String>, session: Session) -> Self {
        Self {
            name: name.into(),
            rig_name: rig_name.into(),
            session,
            mix: RigMix::DEFAULT,
            midi_device: None,
            clap_meta: Vec::new(),
        }
    }

    pub fn package_path(parent: &Path, name: &str) -> Result<PathBuf> {
        validate_package_path(parent, name, RIG_PRESET_EXT, "rig preset")
    }

    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            bail!("The rig preset needs a name before it can be saved.");
        }
        self.session.validate()?;
        if self.session.graph.nodes.iter().any(|n| n.rig != 0) {
            bail!("A rig preset must hold one rig-relative graph (node.rig == 0).");
        }
        if !self.mix.vol.is_finite() || !self.mix.pan.is_finite() {
            bail!("The rig mixer settings contain a non-finite number.");
        }
        for m in &self.clap_meta {
            match self.session.graph.node(m.node) {
                Some(n) if matches!(n.kind, NodeKind::Clap { .. }) => {}
                _ => bail!("Rig preset metadata references node {} which is not a CLAP synth.", m.node),
            }
        }
        Ok(())
    }

    pub fn save(&self, package: &Path) -> Result<()> {
        self.validate()?;
        stage_package(package, "rig preset", |staging| {
            let refs = write_state_files(staging, self.session.plugin_states())?;
            let manifest = RigPresetFile {
                format: RIG_PRESET_FORMAT_VERSION,
                name: self.name.clone(),
                rig_name: self.rig_name.clone(),
                graph: self.session.graph.clone(),
                params: self.session.params().to_vec(),
                matrix: self.session.matrix().cloned(),
                states: refs,
                mix: self.mix,
                midi_device: self.midi_device,
                clap_meta: self.clap_meta.clone(),
            };
            let json = serde_json::to_vec_pretty(&manifest)?;
            write_atomic(&staging.join(RIG_PRESET_FILE), &json)
        })
    }

    pub fn load(package: &Path) -> Result<RigPreset> {
        let manifest_path = package.join(RIG_PRESET_FILE);
        let text = fs::read_to_string(&manifest_path).with_context(|| {
            format!(
                "Could not read {} — is this a kabelsalat rig preset?",
                manifest_path.display()
            )
        })?;
        let manifest: RigPresetFile = serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid rig preset JSON.", manifest_path.display()))?;
        if manifest.format != RIG_PRESET_FORMAT_VERSION {
            bail!(
                "This rig preset uses format version {} but this build understands version {}. \
                 It may have been written by a newer kabelsalat.",
                manifest.format,
                RIG_PRESET_FORMAT_VERSION
            );
        }
        let states = read_state_files(package, &manifest.states, "rig preset")?;
        let mut session = Session::new(manifest.name.clone(), manifest.graph);
        for p in manifest.params {
            session.set_param(p.node, p.param, p.value);
        }
        for s in states {
            session.set_plugin_state(s.node, s.plugin_id, s.data);
        }
        session.set_matrix(manifest.matrix);
        let preset = RigPreset {
            name: manifest.name,
            rig_name: manifest.rig_name,
            session,
            mix: manifest.mix,
            midi_device: manifest.midi_device,
            clap_meta: manifest.clap_meta,
        };
        preset.validate()?;
        Ok(preset)
    }
}

#[derive(Serialize, Deserialize)]
struct RigPresetFile {
    format: u32,
    name: String,
    #[serde(default)]
    rig_name: String,
    graph: Graph,
    #[serde(default)]
    params: Vec<ParamValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    matrix: Option<Value>,
    #[serde(default)]
    states: Vec<StateRef>,
    #[serde(default)]
    mix: RigMix,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    midi_device: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    clap_meta: Vec<RigClapMeta>,
}

/// Default directory for saved rig presets (sibling to `~/kabelsalat-sessions`).
pub fn rig_presets_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let dir = PathBuf::from(home).join("kabelsalat-rig-presets");
    let _ = fs::create_dir_all(&dir);
    dir
}

/// Sorted list of saved rig preset names in `dir`.
pub fn list_rig_presets(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| {
                    let p = e.path();
                    if p.extension().is_some_and(|x| x == RIG_PRESET_EXT) {
                        p.file_stem().map(|s| s.to_string_lossy().into_owned())
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Delete a saved rig preset package by name from `dir`.
pub fn delete_rig_preset(dir: &Path, name: &str) -> Result<()> {
    let path = RigPreset::package_path(dir, name)?;
    if !path.exists() {
        bail!("rig preset '{name}' does not exist");
    }
    fs::remove_dir_all(&path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Undo / redo (snapshot-based)
// ---------------------------------------------------------------------------

/// What a snapshot restores: the patch, knob values, and matrix slot.
/// Plugin blobs and the session name are deliberately not snapshotted —
/// blobs are captured on demand at save time, and renaming is not an edit.
#[derive(Clone)]
struct Snapshot {
    graph: Graph,
    params: Vec<ParamValue>,
    matrix: Option<Value>,
}

/// Snapshot undo stack. Usage: `history.record(&session)` *before* each
/// user-visible mutation, then mutate `session.graph` / params freely.
/// `undo`/`redo` swap whole snapshots — robust for any edit, including
/// structural ones (add/remove node, plug/unplug).
pub struct History {
    past: VecDeque<Snapshot>,
    future: VecDeque<Snapshot>,
    limit: usize,
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

impl History {
    /// 128 undo steps — generous for live patching, bounded in memory
    /// (the graph is capped at 64 nodes).
    pub fn new() -> Self {
        Self::with_limit(128)
    }

    pub fn with_limit(limit: usize) -> Self {
        Self {
            past: VecDeque::new(),
            future: VecDeque::new(),
            limit: limit.max(1),
        }
    }

    fn snapshot(s: &Session) -> Snapshot {
        Snapshot {
            graph: s.graph.clone(),
            params: s.params.clone(),
            matrix: s.matrix.clone(),
        }
    }

    fn restore(s: &mut Session, snap: Snapshot) {
        s.graph = snap.graph;
        s.params = snap.params;
        s.matrix = snap.matrix;
    }

    /// Record the current state before an edit. Clears the redo branch
    /// (a fresh edit forks history) and evicts the oldest snapshot at the cap.
    pub fn record(&mut self, session: &Session) {
        self.future.clear();
        if self.past.len() == self.limit {
            self.past.pop_front();
        }
        self.past.push_back(Self::snapshot(session));
    }

    /// Restore the previous state; returns false when there is nothing to undo.
    pub fn undo(&mut self, session: &mut Session) -> bool {
        match self.past.pop_back() {
            Some(snap) => {
                self.future.push_back(Self::snapshot(session));
                Self::restore(session, snap);
                true
            }
            None => false,
        }
    }

    /// Re-apply the last undone edit; returns false when there is nothing to redo.
    pub fn redo(&mut self, session: &mut Session) -> bool {
        match self.future.pop_back() {
            Some(snap) => {
                self.past.push_back(Self::snapshot(session));
                Self::restore(session, snap);
                true
            }
            None => false,
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.past.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.future.is_empty()
    }

    pub fn clear(&mut self) {
        self.past.clear();
        self.future.clear();
    }
}

// ---------------------------------------------------------------------------
// Starter templates (docs/ARCHITECTURE.md §2b: the graph is never a blank page of fear)
// ---------------------------------------------------------------------------

/// One-click starting points. Each builds a pre-wired, playable [`Graph`]
/// from the existing node kinds plus sensible default knob values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Template {
    /// Singing lead through a lush reverb, dry path kept for clarity.
    LeadReverb,
    /// Two Surge layers (drums + bass) on one keyboard, each with its own
    /// tone control. Key splits arrive with the MIDI matrix (task-9).
    DrumsBass,
    /// Slow pad into long delay and big reverb — instant atmosphere.
    AmbientPad,
}

impl Template {
    pub const ALL: [Template; 3] = [
        Template::LeadReverb,
        Template::DrumsBass,
        Template::AmbientPad,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Template::LeadReverb => "Lead + Reverb",
            Template::DrumsBass => "Drums + Bass",
            Template::AmbientPad => "Ambient Pad",
        }
    }

    /// The pre-wired patch. Edge ids are assigned sequentially from 1.
    pub fn graph(self) -> Graph {
        self.build().0
    }

    /// A ready-to-play session: patch + default knob values.
    pub fn session(self) -> Session {
        let (graph, defaults) = self.build();
        let mut s = Session::new(self.title(), graph);
        for (node, param, value) in defaults {
            s.set_param(node, param, value);
        }
        s
    }

    fn build(self) -> (Graph, Vec<(NodeId, ParamId, f64)>) {
        let mut g = Graph::default();
        g.reserve(64, 256);
        let mut defaults = Vec::new();
        // Templates pick their own edge ids (Graph's counter is private to
        // the control plane); sequential ids are unique by construction.
        let mut eid: EdgeId = 1;
        let mut link = |g: &mut Graph, from: NodeId, to: NodeId, to_port: u8, kind: PortKind| {
            let ok = g.connect_with_id(
                eid,
                PortRef { node: from, port: 0 },
                PortRef { node: to, port: to_port },
                kind,
            );
            assert!(ok, "template wiring must be valid");
            eid += 1;
        };
        let surge = || NodeKind::Clap {
            plugin_id: name_bytes(SURGE_ID),
        };

        match self {
            Template::LeadReverb => {
                let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
                let lead = g.add_node(surge(), name_bytes("LEAD"));
                let verb = g.add_node(NodeKind::Dsp(DspKind::Reverb), name_bytes("REVERB"));
                let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER OUT"));
                link(&mut g, keys, lead, 0, PortKind::Midi);
                link(&mut g, lead, verb, 0, PortKind::Audio);
                link(&mut g, verb, master, 0, PortKind::Audio);
                link(&mut g, lead, master, 1, PortKind::Audio); // dry
                defaults.extend([
                    (lead, P_LEVEL, 0.85),
                    (verb, 0, 0.35), // reverb mix (param table: task-7)
                    (master, P_VOL, 0.75),
                    (master, P_WARM, 0.1),
                ]);
            }
            Template::DrumsBass => {
                let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
                let drums = g.add_node(surge(), name_bytes("DRUMS"));
                let bass = g.add_node(surge(), name_bytes("BASS"));
                let drum_gain = g.add_node(NodeKind::Dsp(DspKind::Gain), name_bytes("DRUM LEVEL"));
                let bass_filter = g.add_node(NodeKind::Dsp(DspKind::Filter), name_bytes("BASS FILTER"));
                let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER OUT"));
                // Layered on one keyboard for now; splits/layers via task-9.
                link(&mut g, keys, drums, 0, PortKind::Midi);
                link(&mut g, keys, bass, 0, PortKind::Midi);
                link(&mut g, drums, drum_gain, 0, PortKind::Audio);
                link(&mut g, drum_gain, master, 0, PortKind::Audio);
                link(&mut g, bass, bass_filter, 0, PortKind::Audio);
                link(&mut g, bass_filter, master, 1, PortKind::Audio);
                defaults.extend([
                    (drums, P_LEVEL, 0.9),
                    (bass, P_LEVEL, 0.8),
                    (drum_gain, 0, 0.9),   // gain (param table: task-7)
                    (bass_filter, 0, 0.45), // cutoff (param table: task-7)
                    (master, P_VOL, 0.75),
                    (master, P_WARM, 0.1),
                ]);
            }
            Template::AmbientPad => {
                let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
                let pad = g.add_node(surge(), name_bytes("PAD"));
                let delay = g.add_node(NodeKind::Dsp(DspKind::Delay), name_bytes("DELAY"));
                let verb = g.add_node(NodeKind::Dsp(DspKind::Reverb), name_bytes("REVERB"));
                let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER OUT"));
                link(&mut g, keys, pad, 0, PortKind::Midi);
                link(&mut g, pad, delay, 0, PortKind::Audio);
                link(&mut g, delay, verb, 0, PortKind::Audio);
                link(&mut g, verb, master, 0, PortKind::Audio);
                link(&mut g, pad, master, 1, PortKind::Audio); // dry
                defaults.extend([
                    (pad, P_LEVEL, 0.7),
                    (delay, P_TIME, 0.6),
                    (delay, P_MIX, 0.45),
                    (verb, 0, 0.6), // reverb mix (param table: task-7)
                    (master, P_VOL, 0.75),
                    (master, P_WARM, 0.05),
                ]);
            }
        }
        (g, defaults)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Test packages live under the cargo target dir (never /tmp — small
    /// tmpfs on this machine); fall back to the repo target dir.
    fn test_dir(tag: &str) -> PathBuf {
        let base = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"));
        let d = base.join("session-tests").join(format!("{tag}-{}", unique_suffix()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Everything that must survive a round trip, as comparable JSON
    /// (Graph/Node/Edge don't derive PartialEq; serde does the comparing).
    fn content(s: &Session) -> Value {
        json!({
            "name": s.name,
            "graph": serde_json::to_value(&s.graph).unwrap(),
            "params": serde_json::to_value(s.params()).unwrap(),
            "matrix": s.matrix().cloned().unwrap_or(Value::Null),
        })
    }

    fn blobs(s: &Session) -> Vec<(NodeId, String, Vec<u8>)> {
        s.plugin_states()
            .iter()
            .map(|p| (p.node, p.plugin_id.clone(), p.data.clone()))
            .collect()
    }

    fn sample_session() -> Session {
        let mut s = Template::LeadReverb.session();
        s.name = "Round Trip".into();
        s.set_param(1, P_LEVEL, 0.42); // overwrite a default
        s.set_matrix(Some(json!({"routes": [{"from": "keys", "to": 1, "transpose": -12}]})));
        // Opaque, non-UTF8 bytes — the session must not interpret them.
        s.set_plugin_state(1, SURGE_ID, vec![0x00, 0xff, 0x80, 0x43, 0x4c, 0x41, 0x50, 0x01]);
        s
    }

    #[test]
    fn save_load_round_trip() {
        let dir = test_dir("roundtrip");
        let pkg = dir.join("Round Trip.ksalat");
        let original = sample_session();
        original.save(&pkg).unwrap();

        // Package layout: manifest + one blob file, nothing else.
        assert!(pkg.join(SESSION_FILE).is_file());
        assert!(pkg.join(STATE_DIR).join("node-1.bin").is_file());

        let loaded = Session::load(&pkg).unwrap();
        assert_eq!(content(&original), content(&loaded));
        assert_eq!(blobs(&original), blobs(&loaded));
        // Node-id counters survive too: new nodes don't collide.
        let mut g = loaded.graph.clone();
        let existing: Vec<NodeId> = g.nodes.iter().map(|n| n.id).collect();
        let id = g.add_node(NodeKind::Keys, name_bytes("LATE"));
        assert!(!existing.contains(&id));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn overwrite_replaces_package_and_leaves_no_stale_files() {
        let dir = test_dir("overwrite");
        let pkg = dir.join("Set.ksalat");
        let mut first = sample_session();
        first.save(&pkg).unwrap();

        // Second save: blob removed, param changed — old blob must not linger.
        first.states.clear();
        first.set_param(1, P_LEVEL, 0.1);
        first.save(&pkg).unwrap();
        let loaded = Session::load(&pkg).unwrap();
        assert!(loaded.plugin_states().is_empty());
        assert!(!pkg.join(STATE_DIR).join("node-1.bin").exists());
        assert_eq!(loaded.param(1, P_LEVEL), Some(0.1));
        // No staging/backup litter next to the package.
        let litter: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(litter.is_empty(), "staging/backup dirs left behind: {litter:?}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_save_keeps_original_untouched() {
        let dir = test_dir("failed-save");
        let pkg = dir.join("Precious.ksalat");
        let good = sample_session();
        good.save(&pkg).unwrap();
        let before = fs::read(pkg.join(SESSION_FILE)).unwrap();

        // Orphaned blob → validation fails → original must be byte-identical.
        let mut bad = good.clone();
        bad.set_plugin_state(99, SURGE_ID, vec![1, 2, 3]);
        assert!(bad.save(&pkg).is_err());
        assert_eq!(fs::read(pkg.join(SESSION_FILE)).unwrap(), before);
        assert_eq!(blobs(&Session::load(&pkg).unwrap()), blobs(&good));
        // No staging litter from the failed attempt.
        let litter: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(litter.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_rejects_escape_paths_missing_files_and_future_versions() {
        let dir = test_dir("bad-load");
        let pkg = dir.join("Evil.ksalat");
        fs::create_dir_all(pkg.join(STATE_DIR)).unwrap();
        let manifest = json!({
            "format": FORMAT_VERSION,
            "name": "Evil",
            "graph": serde_json::to_value(Graph::default()).unwrap(),
            "states": [{"node": 0, "plugin_id": "x", "file": "../outside.bin"}],
        });
        fs::write(pkg.join(SESSION_FILE), manifest.to_string()).unwrap();
        let err = Session::load(&pkg).unwrap_err().to_string();
        assert!(err.contains("outside the package"), "{err}");

        // Missing blob file.
        let manifest = json!({
            "format": FORMAT_VERSION,
            "name": "Evil",
            "graph": serde_json::to_value(Graph::default()).unwrap(),
            "states": [{"node": 0, "plugin_id": "x", "file": "state/node-0.bin"}],
        });
        fs::write(pkg.join(SESSION_FILE), manifest.to_string()).unwrap();
        assert!(Session::load(&pkg).is_err());

        // Future format version.
        let manifest = json!({
            "format": FORMAT_VERSION + 1,
            "name": "Future",
            "graph": serde_json::to_value(Graph::default()).unwrap(),
        });
        fs::write(pkg.join(SESSION_FILE), manifest.to_string()).unwrap();
        let err = Session::load(&pkg).unwrap_err().to_string();
        assert!(err.contains("newer kabelsalat"), "{err}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rig_names_round_trip_through_the_package() {
        let dir = std::env::temp_dir().join(format!("ksalat-rignames-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = Session::new("names", Graph::demo_patch());
        s.rig_names = vec!["Bass".into(), "Pads".into()];
        let pkg = dir.join("names.ksalat");
        s.save(&pkg).unwrap();
        assert_eq!(Session::load(&pkg).unwrap().rig_names, vec!["Bass".to_string(), "Pads".to_string()]);
        // Older packages (no field) load with default names.
        let mut plain = Session::new("plain", Graph::demo_patch());
        plain.rig_names.clear();
        let pkg2 = dir.join("plain.ksalat");
        plain.save(&pkg2).unwrap();
        let json = std::fs::read_to_string(pkg2.join("session.json")).unwrap();
        assert!(!json.contains("rig_names"), "absent when empty: old readers stay happy");
        assert!(Session::load(&pkg2).unwrap().rig_names.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undo_redo_restores_graph_params_and_matrix() {
        let mut s = Template::LeadReverb.session();
        let mut h = History::new();
        let start = content(&s);

        // Edit 1: add a node + cable, turn a knob, set matrix.
        h.record(&s);
        let extra = s.graph.add_node(NodeKind::Dsp(DspKind::Limiter), name_bytes("LIMITER"));
        assert!(s.graph.connect_with_id(
            50,
            PortRef { node: 2, port: 0 },
            PortRef { node: extra, port: 0 },
            PortKind::Audio,
        ));
        s.set_param(1, P_LEVEL, 0.2);
        s.set_matrix(Some(json!({"x": 1})));
        let edited = content(&s);
        assert_ne!(start, edited);

        // Undo → exactly the starting state; redo → exactly the edited state.
        assert!(h.undo(&mut s));
        assert_eq!(content(&s), start);
        assert!(h.can_redo());
        assert!(h.redo(&mut s));
        assert_eq!(content(&s), edited);
        assert!(!h.can_redo());

        // A fresh edit after an undo forks history: the redo branch is dropped.
        assert!(h.undo(&mut s)); // back to start; redo now offers `edited`
        assert_eq!(content(&s), start);
        assert!(h.can_redo());
        h.record(&s); // fork: the pending redo is discarded
        s.set_param(1, P_LEVEL, 0.9);
        assert!(!h.can_redo());
        assert!(h.undo(&mut s));
        assert_eq!(content(&s), start);

        // Empty history is a no-op, not a panic.
        h.clear();
        assert!(!h.undo(&mut s));
        assert!(!h.redo(&mut s));
    }

    #[test]
    fn undo_respects_the_limit() {
        let mut s = Session::default();
        let mut h = History::with_limit(3);
        for i in 0..5 {
            h.record(&s);
            s.set_param(0, 0, i as f64);
        }
        // Only the last 3 edits are undoable.
        for _ in 0..3 {
            assert!(h.undo(&mut s));
        }
        assert!(!h.can_undo());
        assert_eq!(s.param(0, 0), Some(1.0)); // edit #2's snapshot (0-based i=1)
    }

    #[test]
    fn undo_does_not_touch_blobs_or_name() {
        let mut s = sample_session();
        let mut h = History::new();
        h.record(&s);
        s.graph.add_node(NodeKind::Keys, name_bytes("X"));
        s.name = "Renamed".into();
        assert!(h.undo(&mut s));
        assert_eq!(s.name, "Renamed"); // names are not edits
        assert_eq!(blobs(&s), blobs(&sample_session())); // blobs not snapshotted
    }

    #[test]
    fn templates_are_valid_and_playable() {
        for t in Template::ALL {
            let s = t.session();
            s.validate().unwrap_or_else(|e| panic!("{}: {e}", t.title()));
            let g = &s.graph;
            assert!(!g.nodes.is_empty());

            // Every edge references real nodes; no self/duplicate cables.
            for e in &g.edges {
                assert!(g.node(e.from.node).is_some(), "{}: dangling from", t.title());
                assert!(g.node(e.to.node).is_some(), "{}: dangling to", t.title());
                assert_ne!(e.from.node, e.to.node);
            }
            let mut ids: Vec<EdgeId> = g.edges.iter().map(|e| e.id).collect();
            ids.sort_unstable();
            assert!(ids.windows(2).all(|w| w[0] != w[1]));

            // Playable shape: keys → at least one Clap synth → audio → master.
            let keys: Vec<NodeId> = g.nodes.iter()
                .filter(|n| matches!(n.kind, NodeKind::Keys)).map(|n| n.id).collect();
            let synths: Vec<NodeId> = g.nodes.iter()
                .filter(|n| matches!(n.kind, NodeKind::Clap { .. })).map(|n| n.id).collect();
            let masters: Vec<NodeId> = g.nodes.iter()
                .filter(|n| matches!(n.kind, NodeKind::MasterOut)).map(|n| n.id).collect();
            assert!(!keys.is_empty() && !synths.is_empty() && !masters.is_empty());
            assert!(synths.iter().any(|&syn| keys.iter().any(|&k| g.has_route(k, syn, PortKind::Midi))));

            // Audio reachability: some synth reaches some master over audio edges.
            let reaches_master = |syn: NodeId| -> bool {
                let mut seen = vec![syn];
                let mut stack = vec![syn];
                while let Some(n) = stack.pop() {
                    if masters.contains(&n) {
                        return true;
                    }
                    for e in &g.edges {
                        if e.kind == PortKind::Audio && e.from.node == n && !seen.contains(&e.to.node) {
                            seen.push(e.to.node);
                            stack.push(e.to.node);
                        }
                    }
                }
                false
            };
            assert!(synths.iter().any(|&syn| reaches_master(syn)), "{}: no audio path to master", t.title());

            // Default knobs reference real nodes and are finite.
            for p in s.params() {
                assert!(g.node(p.node).is_some());
                assert!(p.value.is_finite());
            }
            // Templates round-trip too.
            let dir = test_dir("template");
            let pkg = dir.join(format!("{}.ksalat", t.title()));
            s.save(&pkg).unwrap();
            let loaded = Session::load(&pkg).unwrap();
            assert_eq!(content(&s), content(&loaded));
            let _ = fs::remove_dir_all(&dir);
        }
    }

    // --- Multi-rig sessions (task-22) ---------------------------------------

    /// One rig: Keys(0) → Surge(1) → Master(2), cables 1 and 2.
    fn mini_rig(name: &str, dev: Option<u32>) -> Rig {
        let mut g = Graph::default();
        let keys = g.add_node(NodeKind::Keys, name_bytes("KEYS"));
        let synth = g.add_node(NodeKind::Clap { plugin_id: name_bytes(SURGE_ID) }, name_bytes("SYNTH"));
        let master = g.add_node(NodeKind::MasterOut, name_bytes("MASTER"));
        assert!(g.connect_with_id(1, PortRef { node: keys, port: 0 }, PortRef { node: synth, port: 0 }, PortKind::Midi));
        assert!(g.connect_with_id(2, PortRef { node: synth, port: 0 }, PortRef { node: master, port: 0 }, PortKind::Audio));
        Rig::new(name, g, dev)
    }

    fn rig_content(s: &MultiRigSession) -> Value {
        json!({
            "name": s.name,
            "active": s.active,
            "rigs": s.rigs.iter().map(|r| json!({
                "name": r.name,
                "midi_device": r.midi_device,
                "graph": serde_json::to_value(&r.graph).unwrap(),
            })).collect::<Vec<_>>(),
        })
    }

    #[test]
    fn multi_rig_save_load_round_trip() {
        let dir = test_dir("multi-rig-roundtrip");
        let pkg = dir.join("Two Rigs.ksalat");
        let original = MultiRigSession::new(
            "Two Rigs",
            vec![mini_rig("A", Some(2)), mini_rig("B", None)],
            1,
        );
        original.save(&pkg).unwrap();
        assert!(pkg.join(MULTI_RIG_FILE).is_file());

        let loaded = MultiRigSession::load(&pkg).unwrap();
        assert_eq!(rig_content(&original), rig_content(&loaded));
        // The merged graph matches the control-plane merge exactly.
        assert_eq!(
            serde_json::to_value(loaded.merged_graph()).unwrap(),
            serde_json::to_value(crate::control::merge_rigs(&original.rigs)).unwrap()
        );
        // Overwrite: change active + device, no stale manifest content.
        let mut edited = loaded;
        edited.active = 0;
        edited.rigs[1].midi_device = Some(7);
        edited.save(&pkg).unwrap();
        let reloaded = MultiRigSession::load(&pkg).unwrap();
        assert_eq!(reloaded.active, 0);
        assert_eq!(reloaded.rigs[1].midi_device, Some(7));
        let litter: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(litter.is_empty(), "staging/backup dirs left behind: {litter:?}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn multi_rig_failed_save_keeps_original() {
        let dir = test_dir("multi-rig-failed-save");
        let pkg = dir.join("Precious Rigs.ksalat");
        let good = MultiRigSession::new("Good", vec![mini_rig("A", Some(2))], 0);
        good.save(&pkg).unwrap();
        let before = fs::read(pkg.join(MULTI_RIG_FILE)).unwrap();

        // Out-of-range active index → validation fails → original untouched.
        let mut bad = good.clone();
        bad.active = 5;
        assert!(bad.save(&pkg).is_err());
        assert_eq!(fs::read(pkg.join(MULTI_RIG_FILE)).unwrap(), before);
        // Empty rig list and empty names are rejected too.
        assert!(MultiRigSession::new("X", vec![], 0).save(&pkg).is_err());
        assert!(MultiRigSession::new("X", vec![mini_rig("  ", None)], 0).save(&pkg).is_err());
        let litter: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(litter.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn multi_rig_load_wraps_single_graph_session() {
        // Old single-graph packages open in multi-rig mode as one rig.
        let dir = test_dir("multi-rig-fallback");
        let pkg = dir.join("Old Song.ksalat");
        let single = sample_session();
        single.save(&pkg).unwrap();
        let multi = MultiRigSession::load(&pkg).unwrap();
        assert_eq!(multi.name, single.name);
        assert_eq!(multi.rigs.len(), 1);
        assert_eq!(multi.active, 0);
        assert_eq!(multi.rigs[0].midi_device, None);
        assert_eq!(
            serde_json::to_value(&multi.rigs[0].graph).unwrap(),
            serde_json::to_value(&single.graph).unwrap()
        );
        // Single-graph Session::load still works on the same package.
        assert!(Session::load(&pkg).is_ok());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn multi_rig_load_rejects_future_versions() {
        let dir = test_dir("multi-rig-bad-load");
        let pkg = dir.join("Future Rigs.ksalat");
        fs::create_dir_all(&pkg).unwrap();
        let manifest = json!({
            "format": MULTI_RIG_FORMAT_VERSION + 1,
            "name": "Future",
            "rigs": [],
            "active": 0,
        });
        fs::write(pkg.join(MULTI_RIG_FILE), manifest.to_string()).unwrap();
        let err = MultiRigSession::load(&pkg).unwrap_err().to_string();
        assert!(err.contains("newer kabelsalat"), "{err}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_and_package_path_guards() {
        let mut s = sample_session();
        s.graph.remove_node(1); // the Surge node
        assert!(s.validate().is_err()); // orphaned param + blob
        s.prune_to_graph();
        s.validate().unwrap();
        assert!(s.plugin_states().is_empty());
        assert!(s.param(1, P_LEVEL).is_none());

        let parent = Path::new("/home/user/sessions");
        assert_eq!(
            Session::package_path(parent, "My Song").unwrap(),
            Path::new("/home/user/sessions/My Song.ksalat")
        );
        for bad in ["", "  ", "..", "a/b", "a\\b"] {
            assert!(Session::package_path(parent, bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn rig_preset_round_trip_list_and_guards() {
        let dir = test_dir("rig-presets");
        let mut s = sample_session();
        s.name = "Lead Preset".into();
        let mut preset = RigPreset::new("Lead Preset", "Lead", s);
        preset.mix = RigMix { vol: 0.65, pan: -0.35, mute: false, solo: true };
        preset.midi_device = Some(3);
        preset.clap_meta = vec![RigClapMeta {
            node: 1,
            keyboard: false,
            hardware: true,
            patch_name: Some("Wurli".into()),
        }];

        let pkg = RigPreset::package_path(&dir, "Lead Preset").unwrap();
        preset.save(&pkg).unwrap();
        assert!(pkg.join(RIG_PRESET_FILE).is_file());
        assert!(pkg.join(STATE_DIR).join("node-1.bin").is_file());
        assert_eq!(list_rig_presets(&dir), vec!["Lead Preset".to_string()]);

        let loaded = RigPreset::load(&pkg).unwrap();
        assert_eq!(loaded.name, "Lead Preset");
        assert_eq!(loaded.rig_name, "Lead");
        assert_eq!(loaded.mix, preset.mix);
        assert_eq!(loaded.midi_device, Some(3));
        assert_eq!(loaded.clap_meta, preset.clap_meta);
        assert_eq!(content(&loaded.session), content(&preset.session));
        assert_eq!(blobs(&loaded.session), blobs(&preset.session));

        let mut bad = preset.clone();
        bad.session.graph.nodes[0].rig = 2;
        assert!(bad.validate().is_err());

        let evil = dir.join("Evil.ksalat-rig");
        fs::create_dir_all(&evil).unwrap();
        fs::write(
            evil.join(RIG_PRESET_FILE),
            json!({
                "format": RIG_PRESET_FORMAT_VERSION,
                "name": "Evil",
                "graph": serde_json::to_value(Graph::default()).unwrap(),
                "states": [{"node": 0, "plugin_id": "x", "file": "../leak.bin"}],
            })
            .to_string(),
        )
        .unwrap();
        assert!(RigPreset::load(&evil).unwrap_err().to_string().contains("outside the package"));
        fs::write(
            evil.join(RIG_PRESET_FILE),
            json!({
                "format": RIG_PRESET_FORMAT_VERSION + 1,
                "name": "Future",
                "graph": serde_json::to_value(Graph::default()).unwrap(),
            })
            .to_string(),
        )
        .unwrap();
        assert!(RigPreset::load(&evil).unwrap_err().to_string().contains("newer kabelsalat"));

        delete_rig_preset(&dir, "Lead Preset").unwrap();
        delete_rig_preset(&dir, "Evil").unwrap();
        assert!(list_rig_presets(&dir).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
