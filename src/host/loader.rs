//! Background CLAP plugin loader (#207): instantiates, warms up and retires
//! CLAP voices on a dedicated main thread (`kabelsalat-clap-loader`) so neither
//! the audio thread nor the UI thread ever blocks on plugin I/O (rule 46).
//!
//! Every `ClapVoiceMain` created by the loader stays on that thread for its
//! entire lifetime (respecting CLAP's `[main-thread]` contract and keeping
//! thread-local `CLAP_PARKED` reuse working for never-free plugins, rule 133).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};

use super::{ClapVoice, ClapVoiceMain, ParamDesc, VoiceController};
use crate::control::NodeId;
use crate::engine::VoiceSlot;
use crate::main_controls::MainControls;
use crate::transport::Transport;

/// Ask glibc on Linux to return freed heap arenas to the OS after retiring a
/// heavy plugin instance (only ever called on background loader threads).
pub(crate) fn trim_heap() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> std::ffi::c_int;
        }
        // SAFETY: `malloc_trim(0)` is thread-safe in glibc and only releases
        // unused free memory at the top of arenas back to the kernel.
        unsafe {
            let _ = malloc_trim(0);
        }
    }
}

struct LoadJob {
    node: NodeId,
    plugin_id: String,
    sample_rate: f64,
    state: Option<Vec<u8>>,
    gen: u64,
}

enum Req {
    Load(LoadJob),
    SetWanted {
        gen: u64,
        nodes: Option<Vec<NodeId>>,
    },
    Retire {
        voice_id: u64,
        node: NodeId,
        plugin_id: String,
        slot: VoiceSlot,
        save_state: bool,
    },
    Call {
        voice_id: u64,
        op: Box<dyn FnOnce(Option<&mut ClapVoiceMain>) + Send>,
    },
}

/// Async reply from the background CLAP loader thread to the host/UI.
pub enum ClapLoaderResp {
    Loaded {
        node: NodeId,
        gen: u64,
        voice_id: u64,
        plugin_id: String,
        slot: VoiceSlot,
        ctrl: VoiceController,
        main_controls: MainControls,
        has_editor: bool,
    },
    LoadFailed {
        node: NodeId,
        gen: u64,
        plugin_id: String,
        error: String,
    },
    Cancelled {
        node: NodeId,
        gen: u64,
    },
    Retired {
        voice_id: u64,
        node: NodeId,
        plugin_id: String,
        saved_state: Option<Vec<u8>>,
    },
}

/// Handle to the dedicated `kabelsalat-clap-loader` background thread.
pub struct ClapLoader {
    tx: mpsc::Sender<Req>,
    rx: mpsc::Receiver<ClapLoaderResp>,
    busy: Arc<AtomicBool>,
}

impl ClapLoader {
    /// Start the background CLAP loader thread. Parks completely when idle
    /// (rule 47) and exits cleanly when this handle is dropped.
    pub fn spawn() -> Self {
        let (req_tx, req_rx) = mpsc::channel::<Req>();
        let (resp_tx, resp_rx) = mpsc::channel::<ClapLoaderResp>();
        let busy = Arc::new(AtomicBool::new(false));
        let busy_bg = busy.clone();
        std::thread::Builder::new()
            .name("kabelsalat-clap-loader".into())
            .spawn(move || loader_loop(req_rx, resp_tx, busy_bg))
            .expect("spawn clap loader");
        Self { tx: req_tx, rx: resp_rx, busy }
    }

    pub fn request_load(
        &self,
        node: NodeId,
        plugin_id: String,
        sample_rate: f64,
        state: Option<Vec<u8>>,
        gen: u64,
    ) {
        let _ = self.tx.send(Req::Load(LoadJob { node, plugin_id, sample_rate, state, gen }));
    }

    /// Tell the loader which nodes are still wanted for `gen` so stale queued
    /// loads are cancelled without instantiating the plugin.
    pub fn set_wanted(&self, gen: u64, nodes: Option<Vec<NodeId>>) {
        let _ = self.tx.send(Req::SetWanted { gen, nodes });
    }

    /// Retire a background-loaded voice on its creating thread (optionally
    /// saving its latest state first, race-free because `slot` is already off
    /// the audio thread).
    pub fn retire(
        &self,
        voice_id: u64,
        node: NodeId,
        plugin_id: String,
        slot: VoiceSlot,
        save_state: bool,
    ) {
        let _ = self.tx.send(Req::Retire { voice_id, node, plugin_id, slot, save_state });
    }

    pub fn try_recv(&self) -> Option<ClapLoaderResp> {
        self.rx.try_recv().ok()
    }

    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Relaxed)
    }

    /// Build a UI-side proxy for a voice whose `ClapVoiceMain` lives on this
    /// loader thread.
    pub fn proxy(
        &self,
        voice_id: u64,
        main_controls: MainControls,
        has_editor: bool,
    ) -> BgClapMain {
        BgClapMain {
            voice_id,
            tx: self.tx.clone(),
            busy: self.busy.clone(),
            main_controls,
            has_editor,
            editor_open: false,
        }
    }
}

fn loader_loop(
    req_rx: mpsc::Receiver<Req>,
    resp_tx: mpsc::Sender<ClapLoaderResp>,
    busy: Arc<AtomicBool>,
) {
    const PUMP_EVERY: Duration = Duration::from_millis(16);
    let mut mains: HashMap<u64, ClapVoiceMain> = HashMap::new();
    let mut pending: VecDeque<LoadJob> = VecDeque::new();
    let mut retired_states: HashMap<(u64, NodeId), Vec<u8>> = HashMap::new();
    let mut cur_gen: u64 = 0;
    let mut wanted: Option<Vec<NodeId>> = None;
    let mut last_pump = Instant::now();

    let handle_immediate = |msg: Req,
                            mains: &mut HashMap<u64, ClapVoiceMain>,
                            pending: &mut VecDeque<LoadJob>,
                            retired_states: &mut HashMap<(u64, NodeId), Vec<u8>>,
                            cur_gen: &mut u64,
                            wanted: &mut Option<Vec<NodeId>>,
                            resp_tx: &mpsc::Sender<ClapLoaderResp>,
                            busy: &AtomicBool| {
        match msg {
            Req::Load(job) => pending.push_back(job),
            Req::SetWanted { gen, nodes } => {
                if gen > *cur_gen {
                    retired_states.clear();
                }
                if gen >= *cur_gen {
                    *cur_gen = gen;
                    *wanted = nodes;
                }
            }
            Req::Retire { voice_id, node, plugin_id, slot, save_state } => {
                busy.store(true, Ordering::Release);
                let saved_state = if let Some(mut main) = mains.remove(&voice_id) {
                    let st = if save_state { main.save_state().ok() } else { None };
                    main.retire(slot.into_voice());
                    trim_heap();
                    st
                } else {
                    drop(slot);
                    None
                };
                if let Some(st) = &saved_state {
                    if node != NodeId::MAX {
                        retired_states.insert((*cur_gen, node), st.clone());
                    }
                }
                busy.store(false, Ordering::Release);
                let _ = resp_tx.send(ClapLoaderResp::Retired {
                    voice_id,
                    node,
                    plugin_id,
                    saved_state,
                });
            }
            Req::Call { voice_id, op } => {
                op(mains.get_mut(&voice_id));
            }
        }
    };

    loop {
        // Always drain immediate control/retire/query messages ahead of loads.
        loop {
            match req_rx.try_recv() {
                Ok(msg) => handle_immediate(
                    msg,
                    &mut mains,
                    &mut pending,
                    &mut retired_states,
                    &mut cur_gen,
                    &mut wanted,
                    &resp_tx,
                    &busy,
                ),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    teardown_loader_thread(mains);
                    return;
                }
            }
        }

        if let Some(job) = pending.pop_front() {
            let stale = job.gen < cur_gen
                || wanted.as_ref().is_some_and(|w| !w.contains(&job.node));
            if stale {
                let _ = resp_tx.send(ClapLoaderResp::Cancelled { node: job.node, gen: job.gen });
                continue;
            }
            busy.store(true, Ordering::Release);
            let effective_state = retired_states.remove(&(job.gen, job.node)).or(job.state);
            match ClapVoice::new_plugin(job.sample_rate, &job.plugin_id) {
                Ok((mut voice, mut main)) => {
                    if let Some(blob) = &effective_state {
                        let _ = main.load_state(blob);
                    }
                    // Warm up 2 silent blocks on a separate warmup thread
                    // (obeying CLAP's [audio-thread] != [main-thread] contract)
                    // so start_processing, state-load XML parsing (Surge XT
                    // applies state inside process(), rule 38), and lazy
                    // allocations finish off the real audio callback.
                    let sr = job.sample_rate;
                    std::thread::scope(|s| {
                        s.spawn(|| {
                            let mut warm = [0.0f32; 256 * 2];
                            let mut t = Transport::default();
                            voice.render(&mut warm, t);
                            t.advance(256, sr);
                            voice.render(&mut warm, t);
                        });
                    });
                    main.service();
                    main.poll_main_controls();

                    let ctrl = voice.controller();
                    let voice_id = voice.voice_id();
                    let main_controls = main.main_controls().clone();
                    let has_editor = main.has_editor();
                    let slot = VoiceSlot::new(voice);
                    mains.insert(voice_id, main);
                    let _ = resp_tx.send(ClapLoaderResp::Loaded {
                        node: job.node,
                        gen: job.gen,
                        voice_id,
                        plugin_id: job.plugin_id,
                        slot,
                        ctrl,
                        main_controls,
                        has_editor,
                    });
                }
                Err(e) => {
                    let error = format!("Load {} failed: {e}", job.plugin_id);
                    let _ = resp_tx.send(ClapLoaderResp::LoadFailed {
                        node: job.node,
                        gen: job.gen,
                        plugin_id: job.plugin_id,
                        error,
                    });
                }
            }
            busy.store(false, Ordering::Release);
            if last_pump.elapsed() >= PUMP_EVERY {
                for m in mains.values_mut() {
                    m.pump();
                }
                last_pump = Instant::now();
            }
            continue;
        }

        // Idle: park completely when nothing is hosted, or wake every 16 ms to
        // serve plugin callbacks/editors (rules 47, 181).
        let msg = if mains.is_empty() {
            match req_rx.recv() {
                Ok(m) => Some(m),
                Err(_) => {
                    teardown_loader_thread(mains);
                    return;
                }
            }
        } else {
            let wait = PUMP_EVERY.saturating_sub(last_pump.elapsed());
            match req_rx.recv_timeout(wait) {
                Ok(m) => Some(m),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    teardown_loader_thread(mains);
                    return;
                }
            }
        };
        if let Some(m) = msg {
            handle_immediate(
                m,
                &mut mains,
                &mut pending,
                &mut retired_states,
                &mut cur_gen,
                &mut wanted,
                &resp_tx,
                &busy,
            );
        }
        if last_pump.elapsed() >= PUMP_EVERY {
            for m in mains.values_mut() {
                m.pump();
            }
            last_pump = Instant::now();
        }
    }
}

/// Safe teardown when the `ClapLoader` handle drops: never run TLS destructors
/// on `CLAP_NEVER_FREE` parked plugins (rule 133), and never drop a live
/// `ClapVoiceMain` whose audio half may still exist on another thread.
fn teardown_loader_thread(mut mains: HashMap<u64, ClapVoiceMain>) {
    for (_, main) in mains.drain() {
        std::mem::forget(main);
    }
    super::leak_parked_clap_on_thread_exit();
}

/// UI-side handle to a `ClapVoiceMain` hosted on `kabelsalat-clap-loader`.
pub struct BgClapMain {
    voice_id: u64,
    tx: mpsc::Sender<Req>,
    busy: Arc<AtomicBool>,
    main_controls: MainControls,
    has_editor: bool,
    editor_open: bool,
}

/// Bounded wait for non-blocking per-frame UI queries so a background plugin
/// load can never freeze the UI thread (rule 46).
const UI_POLL_TIMEOUT: Duration = Duration::from_millis(2);

impl BgClapMain {
    fn call<R: Send + 'static>(&self, f: impl FnOnce(&mut ClapVoiceMain) -> R + Send + 'static) -> Option<R> {
        let (reply_tx, reply_rx) = mpsc::sync_channel::<Option<R>>(1);
        let op = Box::new(move |m: Option<&mut ClapVoiceMain>| {
            let _ = reply_tx.send(m.map(f));
        });
        if self.tx.send(Req::Call { voice_id: self.voice_id, op }).is_err() {
            return None;
        }
        reply_rx.recv().ok().flatten()
    }

    fn try_call<R: Send + 'static>(
        &self,
        timeout: Duration,
        f: impl FnOnce(&mut ClapVoiceMain) -> R + Send + 'static,
    ) -> Option<R> {
        if self.busy.load(Ordering::Acquire) {
            return None;
        }
        let (reply_tx, reply_rx) = mpsc::sync_channel::<Option<R>>(1);
        let op = Box::new(move |m: Option<&mut ClapVoiceMain>| {
            let _ = reply_tx.send(m.map(f));
        });
        if self.tx.send(Req::Call { voice_id: self.voice_id, op }).is_err() {
            return None;
        }
        reply_rx.recv_timeout(timeout).ok().flatten()
    }
}

/// Either a local (UI-thread) `ClapVoiceMain` or a proxy to one hosted on the
/// background `kabelsalat-clap-loader` thread (#207).
pub enum HostedClapMain {
    Local(ClapVoiceMain),
    Bg(BgClapMain),
}

impl From<ClapVoiceMain> for HostedClapMain {
    fn from(m: ClapVoiceMain) -> Self {
        HostedClapMain::Local(m)
    }
}

impl HostedClapMain {
    pub fn is_bg(&self) -> bool {
        matches!(self, HostedClapMain::Bg(_))
    }

    /// True while the background loader is instantiating or retiring a plugin.
    /// Periodic UI polls skip when busy so the UI thread never waits on a load.
    pub fn is_bg_busy(&self) -> bool {
        match self {
            HostedClapMain::Local(_) => false,
            HostedClapMain::Bg(bg) => bg.busy.load(Ordering::Acquire),
        }
    }

    pub fn has_editor(&mut self) -> bool {
        match self {
            HostedClapMain::Local(m) => m.has_editor(),
            HostedClapMain::Bg(bg) => bg.has_editor,
        }
    }

    pub fn editor_open(&self) -> bool {
        match self {
            HostedClapMain::Local(m) => m.editor_open(),
            HostedClapMain::Bg(bg) => bg.editor_open,
        }
    }

    pub fn open_editor(&mut self, title: &str) -> Result<()> {
        match self {
            HostedClapMain::Local(m) => m.open_editor(title),
            HostedClapMain::Bg(bg) => {
                let title = title.to_string();
                let res = bg
                    .call(move |m| {
                        let r = m.open_editor(&title);
                        (r, m.editor_open())
                    })
                    .ok_or_else(|| anyhow!("plugin loader unavailable"))?;
                bg.editor_open = res.1;
                res.0
            }
        }
    }

    pub fn close_editor(&mut self) {
        match self {
            HostedClapMain::Local(m) => m.close_editor(),
            HostedClapMain::Bg(bg) => {
                let _ = bg.call(|m| m.close_editor());
                bg.editor_open = false;
            }
        }
    }

    pub fn pump(&mut self) {
        match self {
            HostedClapMain::Local(m) => m.pump(),
            // Background loader pumps its own mains every 16 ms.
            HostedClapMain::Bg(bg) => {
                if bg.editor_open {
                    if let Some(open) = bg.try_call(UI_POLL_TIMEOUT, |m| m.editor_open()) {
                        bg.editor_open = open;
                    }
                }
            }
        }
    }

    pub fn poll_latency(&mut self) -> u32 {
        match self {
            HostedClapMain::Local(m) => m.poll_latency(),
            HostedClapMain::Bg(bg) => bg.try_call(UI_POLL_TIMEOUT, |m| m.poll_latency()).unwrap_or(0),
        }
    }

    pub fn save_state(&mut self) -> Result<Vec<u8>> {
        match self {
            HostedClapMain::Local(m) => m.save_state(),
            HostedClapMain::Bg(bg) => bg
                .call(|m| m.save_state())
                .ok_or_else(|| anyhow!("plugin loader unavailable"))?,
        }
    }

    pub fn load_state(&mut self, data: &[u8]) -> Result<()> {
        match self {
            HostedClapMain::Local(m) => m.load_state(data),
            HostedClapMain::Bg(bg) => {
                let data = data.to_vec();
                bg.call(move |m| m.load_state(&data))
                    .ok_or_else(|| anyhow!("plugin loader unavailable"))?
            }
        }
    }

    pub fn retire(self, voice: ClapVoice) {
        match self {
            HostedClapMain::Local(m) => {
                m.retire(voice);
                trim_heap();
            }
            HostedClapMain::Bg(bg) => {
                let plugin_id = voice.plugin_id().to_string();
                let slot = VoiceSlot::new(voice);
                let _ = bg.tx.send(Req::Retire {
                    voice_id: bg.voice_id,
                    node: NodeId::MAX,
                    plugin_id,
                    slot,
                    save_state: false,
                });
            }
        }
    }

    pub fn param_list(&mut self) -> Vec<ParamDesc> {
        match self {
            HostedClapMain::Local(m) => m.param_list(),
            HostedClapMain::Bg(bg) => bg.call(|m| m.param_list()).unwrap_or_default(),
        }
    }

    pub fn main_controls(&self) -> &MainControls {
        match self {
            HostedClapMain::Local(m) => m.main_controls(),
            HostedClapMain::Bg(bg) => &bg.main_controls,
        }
    }

    pub fn poll_main_controls(&mut self) -> bool {
        match self {
            HostedClapMain::Local(m) => m.poll_main_controls(),
            HostedClapMain::Bg(bg) => {
                if let Some((changed, mc)) = bg.try_call(UI_POLL_TIMEOUT, |m| {
                    let c = m.poll_main_controls();
                    (c, if c { Some(m.main_controls().clone()) } else { None })
                }) {
                    if let Some(mc) = mc {
                        bg.main_controls = mc;
                    }
                    changed
                } else {
                    false
                }
            }
        }
    }

    pub fn param_desc(&mut self, id: u32) -> Option<ParamDesc> {
        match self {
            HostedClapMain::Local(m) => m.param_desc(id),
            HostedClapMain::Bg(bg) => bg.call(move |m| m.param_desc(id)).flatten(),
        }
    }

    pub fn param_value(&mut self, id: u32) -> Option<f64> {
        match self {
            HostedClapMain::Local(m) => m.param_value(id),
            HostedClapMain::Bg(bg) => bg.try_call(UI_POLL_TIMEOUT, move |m| m.param_value(id)).flatten(),
        }
    }

    pub fn param_text(&mut self, id: u32, value: f64) -> String {
        match self {
            HostedClapMain::Local(m) => m.param_text(id, value),
            HostedClapMain::Bg(bg) => bg
                .try_call(UI_POLL_TIMEOUT, move |m| m.param_text(id, value))
                .unwrap_or_else(|| format!("{value:.3}")),
        }
    }

    pub fn param_text_opt(&mut self, id: u32, value: f64) -> Option<String> {
        match self {
            HostedClapMain::Local(m) => m.param_text_opt(id, value),
            HostedClapMain::Bg(bg) => bg
                .try_call(UI_POLL_TIMEOUT, move |m| m.param_text_opt(id, value))
                .flatten(),
        }
    }

    pub fn param_text_to_value(&mut self, id: u32, text: &str) -> Option<f64> {
        match self {
            HostedClapMain::Local(m) => m.param_text_to_value(id, text),
            HostedClapMain::Bg(bg) => {
                let text = text.to_string();
                bg.call(move |m| m.param_text_to_value(id, &text)).flatten()
            }
        }
    }

    pub fn param_options(&mut self, p: &ParamDesc) -> Vec<(f64, String)> {
        match self {
            HostedClapMain::Local(m) => m.param_options(p),
            HostedClapMain::Bg(bg) => {
                let p = p.clone();
                bg.call(move |m| m.param_options(&p)).unwrap_or_default()
            }
        }
    }

    pub fn resolve_param_text(&mut self, p: &ParamDesc, text: &str) -> Option<f64> {
        match self {
            HostedClapMain::Local(m) => m.resolve_param_text(p, text),
            HostedClapMain::Bg(bg) => {
                let p = p.clone();
                let text = text.to_string();
                bg.call(move |m| m.resolve_param_text(&p, &text)).flatten()
            }
        }
    }
}
