//! First run (#287) in the app: the sound a fresh install makes, and the
//! one sentence it says when something is wrong. The pure part and the
//! scripted start are `crate::firstrun`.

use super::*;

/// The test note is quiet and short.
const TEST_NOTE: u8 = 72;
const TEST_VELOCITY: f32 = 0.45;
const TEST_LEN_S: f32 = 0.6;
/// The app has had time to open its stream and host its voices.
const START_AFTER_S: f32 = 1.5;
/// A first-run sentence stays long enough to read.
const NOTICE_SECS: f32 = 25.0;

#[derive(Default)]
pub(super) struct FirstRun {
    /// Why the app has no output (set at start when it could not open one).
    pub(super) no_output: Option<String>,
    /// Decided once: is this the first start of this install?
    fresh: Option<bool>,
    /// Realtime check running off the UI thread (rule 210).
    rt_rx: Option<std::sync::mpsc::Receiver<Option<String>>>,
    rt_warning: Option<String>,
    checked: bool,
    /// When the test note ends.
    note_off_at: Option<f32>,
    said: bool,
}

impl DemoApp {
    /// Once per frame, cheap: the first start's test sound, the keyboard,
    /// and the sentence about what is wrong.
    pub(super) fn track_first_run(&mut self) {
        if cfg!(test) && self.first_run.fresh.is_none() && !self.first_run.checked {
            return;
        }
        if let (Some(off), Some(bus)) = (self.first_run.note_off_at, &self.bus) {
            if self.ui_time >= off {
                let _ = bus.lock().map(|mut b| b.send(Command::NoteOff { note: TEST_NOTE, source: SourceId::Keys, channel: 0 }));
                self.first_run.note_off_at = None;
            }
        }
        if self.ui_time < START_AFTER_S {
            return;
        }
        if !self.first_run.checked {
            self.first_run.checked = true;
            let fresh = *self.first_run.fresh.get_or_insert_with(|| crate::assistant::load_config_str("first_run_done").is_none());
            if self.audio_host.is_some() {
                // Gather off the UI thread: the doctor reads /proc and rtkit.
                let (tx, rx) = std::sync::mpsc::channel();
                self.first_run.rt_rx = Some(rx);
                let _ = std::thread::Builder::new().name("first-run-check".into()).spawn(move || {
                    let rt = crate::audio_doctor::gather_realtime();
                    let _ = tx.send(crate::audio_doctor::startup_rt_warning(&rt));
                });
                if fresh && self.bus.is_some() {
                    // A note sounds before anything is asked, and the keyboard is up.
                    if let Some(bus) = &self.bus {
                        let _ = bus.lock().map(|mut b| b.send(Command::NoteOn { note: TEST_NOTE, velocity: TEST_VELOCITY, source: SourceId::Keys, channel: 0 }));
                    }
                    self.first_run.note_off_at = Some(self.ui_time + TEST_LEN_S);
                    if self.dock == DockView::None {
                        self.dock = DockView::Keyboard;
                    }
                    let _ = crate::assistant::save_config_field("first_run_done", "1");
                }
            }
        }
        if let Some(rx) = &self.first_run.rt_rx {
            match rx.try_recv() {
                Ok(w) => {
                    self.first_run.rt_warning = w;
                    self.first_run.rt_rx = None;
                    self.first_run.said = false;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.first_run.rt_rx = None,
                Err(_) => {}
            }
        }
        if self.first_run.said || self.first_run.rt_rx.is_some() {
            return;
        }
        self.first_run.said = true;
        let facts = crate::firstrun::Facts {
            no_output: self.first_run.no_output.clone().or_else(|| self.audio_host.is_none().then(|| "none could be opened".to_string())),
            buffer_ms: self.audio_host.as_ref().map(|h| h.buffer_frames as f32 / h.sample_rate.max(1.0) as f32 * 1000.0),
            realtime_warning: self.first_run.rt_warning.clone(),
        };
        let msg = crate::firstrun::sentence(&facts).or_else(|| {
            (crate::factory::fallback_active() && self.first_run.fresh == Some(true)).then(|| crate::firstrun::factory_sentence().to_string())
        });
        if let Some(msg) = msg {
            self.patch_notice = Some((msg, self.ui_time + NOTICE_SECS));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_app_without_an_output_says_so_in_one_sentence_that_offers_the_audio_doctor() {
        let mut app = DemoApp::new_with_sink(None);
        app.first_run.fresh = Some(true);
        app.first_run.no_output = Some("no device".into());
        app.ui_time = 2.0;
        app.track_first_run();
        let (msg, until) = app.patch_notice.clone().expect("the problem is told");
        assert!(msg.starts_with("No sound output was found (no device)"), "{msg}");
        assert!(msg.contains("Audio Doctor"));
        assert!(until > app.ui_time + 10.0, "it stays long enough to read");
    }

    #[test]
    fn nothing_is_said_or_played_before_the_app_had_time_to_start() {
        let mut app = DemoApp::new_with_sink(None);
        app.first_run.fresh = Some(true);
        app.ui_time = 0.5;
        app.track_first_run();
        assert!(app.patch_notice.is_none() && !app.first_run.checked);
    }
}
