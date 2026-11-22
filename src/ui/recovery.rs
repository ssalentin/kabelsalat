//! Back on stage after a crash (#279): a supervised app keeps the watchdog's
//! marker on the saved session and setlist part it is on, and a restarted
//! one opens them again and says so, with the crash report one click away.

use super::*;
use crate::watchdog::{Marker, CRASH_REPORT, RECOVER, SUPERVISED};

#[derive(Default)]
pub(super) struct Recovery {
    /// Where the marker goes; None = not supervised, nothing is written.
    marker: Option<std::path::PathBuf>,
    /// The session autosave wrote while no saved session was open.
    pub(super) autosaved: Option<String>,
    written: Option<Marker>,
    notice: Option<Notice>,
    /// Where the notice was drawn last frame; the rack takes no click there.
    notice_rect: Option<Rect>,
}

impl Recovery {
    pub(super) fn has_notice(&self) -> bool {
        self.notice.is_some()
    }
}

struct Notice {
    report: Option<std::path::PathBuf>,
    /// What was reopened, e.g. "'Gig' at Encore · Outro".
    restored: Option<String>,
}

impl DemoApp {
    /// At start: keep the marker when the watchdog runs this app, and after
    /// a crash open what it names. Returns whether a session was reopened.
    pub(super) fn start_recovery(&mut self) -> bool {
        if std::env::var_os(SUPERVISED).is_none() {
            return false;
        }
        let path = Marker::path();
        let restored = if std::env::var_os(RECOVER).is_some() {
            match Marker::read(&path).map(|m| self.restore(&m)) {
                Some(Ok(what)) => what,
                Some(Err(e)) => {
                    eprintln!("recovery: {e}");
                    None
                }
                None => None,
            }
        } else {
            None
        };
        if let Some(report) = std::env::var_os(CRASH_REPORT) {
            let report = Some(std::path::PathBuf::from(report)).filter(|p| p.is_file());
            self.recovery.notice = Some(Notice { report, restored: restored.clone() });
        }
        self.recovery.marker = Some(path);
        restored.is_some()
    }

    fn restore(&mut self, m: &Marker) -> Result<Option<String>, String> {
        let restored = if let Some(name) = &m.session {
            let dir = self.effective_sessions_dir();
            self.open_saved_session(&dir, name)?;
            // A part added after the last save is not in the file: the session
            // stays open at the part it saved.
            let moved = match m.pos {
                Some(pos) => self.select_setlist(pos.song, pos.part).map(|_| ()),
                None if self.setlist.active.is_some() => self.step_setlist("clear").map(|_| ()),
                None => Ok(()),
            };
            if let Err(e) = moved {
                eprintln!("recovery: {e}");
            }
            let at = self.setlist.current().map(|(s, p, _)| format!(" at {} · {}", s.name, p.name)).unwrap_or_default();
            Some(format!("'{name}'{at}"))
        } else {
            None
        };
        if m.show_mode {
            self.set_show_mode(true);
        }
        Ok(restored)
    }

    /// Per frame: write the marker when what it should say changed.
    pub(super) fn track_recovery(&mut self) {
        let Some(path) = &self.recovery.marker else { return };
        // While a saved session is open, an older autosave of an unsaved
        // rack no longer holds it (a delete of the open one falls to nothing).
        if self.open_session.is_some() {
            self.recovery.autosaved = None;
        }
        let session = self.open_session.as_deref().or(self.recovery.autosaved.as_deref());
        let show_mode = self.show_mode.is_some();
        if self.recovery.written.as_ref().is_some_and(|w| {
            w.session.as_deref() == session && w.pos == self.setlist.active && w.show_mode == show_mode
        }) {
            return;
        }
        let now = Marker { pid: std::process::id(), session: session.map(str::to_string), pos: self.setlist.active, show_mode };
        match now.write(path) {
            Ok(()) => self.recovery.written = Some(now),
            Err(e) => {
                // Tried again on the next change, not every frame.
                eprintln!("recovery: marker not written: {e}");
                self.recovery.written = Some(now);
            }
        }
    }

    /// Whether `p` lies on the crash notice (`handle_input` leaves it alone).
    pub(super) fn crash_notice_covers(&self, p: Pos2) -> bool {
        self.recovery.notice.is_some() && self.recovery.notice_rect.is_some_and(|r| r.contains(p))
    }

    /// The note a restarted app shows until it is dismissed.
    pub(super) fn crash_notice(&mut self, ctx: &egui::Context) {
        let Some(n) = &self.recovery.notice else { return };
        let headline = match &n.restored {
            Some(what) => format!("kabelsalat restarted after a crash and reopened {what}."),
            None => "kabelsalat restarted after a crash.".to_string(),
        };
        let detail = if n.report.is_some() {
            "The report stays on this computer; Report on GitHub fills in an issue for you to send."
        } else {
            "It left no report."
        };
        let report = n.report.clone();
        let screen = ctx.content_rect();
        let width = (screen.width() - 32.0).clamp(240.0, 560.0);
        let mut dismiss = false;
        let area = egui::Area::new(egui::Id::unique("crash_notice"))
            .order(egui::Order::Foreground)
            .pivot(egui::Align2::CENTER_TOP)
            .fixed_pos(screen.center_top() + egui::vec2(0.0, 56.0))
            .constrain(true)
            .show(ctx, |ui| {
                theme::dropdown_popup_frame().inner_margin(egui::Margin::same(12)).show(ui, |ui| {
                    ui.set_width(width);
                    ui.spacing_mut().item_spacing.y = 6.0;
                    ui.add(egui::Label::new(egui::RichText::new(&headline).font(theme::medium_font(13.0)).color(theme::INK)).wrap());
                    ui.add(egui::Label::new(egui::RichText::new(detail).size(11.5).color(theme::INK_2)).wrap());
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        if let Some(report) = &report {
                            if theme::chip_button(ui, "Open folder", false).clicked() {
                                if let Some(dir) = report.parent() {
                                    open_with_desktop(dir.as_os_str());
                                }
                            }
                            if theme::chip_button(ui, "Report on GitHub", false).clicked() {
                                if let Ok(text) = std::fs::read_to_string(report) {
                                    ui.ctx().open_url(egui::OpenUrl::new_tab(issue_url(&text)));
                                }
                            }
                        }
                        if theme::chip_button(ui, "Dismiss", false).clicked() {
                            dismiss = true;
                        }
                    });
                });
            });
        self.recovery.notice_rect = Some(area.response.rect);
        if dismiss {
            self.recovery.notice = None;
        }
    }
}

fn open_with_desktop(target: &std::ffi::OsStr) {
    if let Err(e) = std::process::Command::new("xdg-open").arg(target).spawn() {
        eprintln!("recovery: xdg-open: {e}");
    }
}

/// GitHub's longest new-issue URL is about 8 KB; the report is cut to fit.
const ISSUE_URL_MAX: usize = 7500;

/// A new-issue link with the report as its body. Nothing is sent until the
/// user submits the form.
fn issue_url(report: &str) -> String {
    let cause = report.lines().find_map(|l| l.strip_prefix("cause: ")).unwrap_or("crash");
    let title: String = format!("Crash: {cause}").chars().take(100).collect();
    let mut url = format!("{}/issues/new?title={}&body=", env!("CARGO_PKG_REPOSITORY"), percent(&title));
    url.push_str(&percent("What were you doing when it crashed?\n\n\n```\n"));
    let close = percent("```\n");
    let cut = percent("[report cut to fit the link; the whole file is in the crash folder]\n");
    for line in report.lines() {
        let enc = percent(&format!("{line}\n"));
        if url.len() + enc.len() + close.len() + cut.len() > ISSUE_URL_MAX {
            url.push_str(&cut);
            break;
        }
        url.push_str(&enc);
    }
    url.push_str(&close);
    url
}

fn percent(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setlist::{Part, PartRig, SetlistPos, Song};

    fn setlist_session(dir: &std::path::Path, name: &str) {
        let mut s = crate::session::Session::new(name, crate::control::standard_rig_graph(false));
        s.setlist.songs = vec![
            Song::new("Opener", vec![Part::new("Intro", vec![PartRig::new(0)])]),
            Song::new("Encore", vec![Part::new("Verse", vec![PartRig::new(0)]), Part::new("Outro", vec![PartRig::new(0)])]),
        ];
        s.setlist.active = Some(SetlistPos { song: 0, part: 0 });
        s.save(&crate::session::Session::package_path(dir, name).unwrap()).unwrap();
    }

    fn supervised_app(dir: &std::path::Path) -> DemoApp {
        let mut app = DemoApp::new_with_sink(None);
        app.sessions_dir_override = Some(dir.to_path_buf());
        app.recovery.marker = Some(dir.join("recover.json"));
        app
    }

    #[test]
    fn a_restart_reopens_the_session_at_the_part_the_marker_names() {
        let dir = std::env::temp_dir().join(format!("kabelsalat-test-recovery-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        setlist_session(&dir, "Gig");
        let mut before = supervised_app(&dir);
        before.open_saved_session(&dir, "Gig").unwrap();
        before.select_setlist(1, 1).unwrap();
        before.track_recovery();
        let marker = Marker::read(&dir.join("recover.json")).unwrap();
        assert_eq!(marker, Marker { pid: std::process::id(), session: Some("Gig".into()), pos: Some(SetlistPos { song: 1, part: 1 }), show_mode: false });

        // The file still holds part (0, 0): the switch was never autosaved.
        let mut after = supervised_app(&dir);
        assert_eq!(after.restore(&marker).unwrap().as_deref(), Some("'Gig' at Encore · Outro"));
        assert_eq!(after.open_session.as_deref(), Some("Gig"));
        assert_eq!(after.setlist.active, Some(SetlistPos { song: 1, part: 1 }));
        assert!(after.show_mode.is_none());

        // Show mode survives a crash marker too.
        before.set_show_mode(true);
        before.track_recovery();
        let show_marker = Marker::read(&dir.join("recover.json")).unwrap();
        assert!(show_marker.show_mode);
        let mut back_on_stage = supervised_app(&dir);
        back_on_stage.restore(&show_marker).unwrap();
        assert!(back_on_stage.show_mode.is_some());

        // A song added after the last save: the session still opens.
        let mut later = supervised_app(&dir);
        let gone = Marker { pos: Some(SetlistPos { song: 2, part: 0 }), ..marker };
        assert_eq!(later.restore(&gone).unwrap().as_deref(), Some("'Gig' at Opener · Intro"));
        assert_eq!(later.open_session.as_deref(), Some("Gig"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unsaved_rack_is_found_under_the_name_autosave_wrote() {
        let dir = std::env::temp_dir().join(format!("kabelsalat-test-recovery-auto-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = supervised_app(&dir);
        app.track_recovery();
        assert_eq!(Marker::read(&dir.join("recover.json")).unwrap().session, None, "nothing on disk yet");
        app.recovery.autosaved = Some(AUTOSAVE_NAME.into());
        app.track_recovery();
        assert_eq!(Marker::read(&dir.join("recover.json")).unwrap().session.as_deref(), Some(AUTOSAVE_NAME));
        app.autosave_rebase();
        app.track_recovery();
        assert_eq!(Marker::read(&dir.join("recover.json")).unwrap().session, None, "a template load is not on disk");

        // Autosave wrote the open session; deleting that session leaves nothing to reopen.
        app.open_session = Some("Gig".into());
        app.recovery.autosaved = Some("Gig".into());
        app.track_recovery();
        assert_eq!(Marker::read(&dir.join("recover.json")).unwrap().session.as_deref(), Some("Gig"));
        app.open_session = None;
        app.track_recovery();
        assert_eq!(Marker::read(&dir.join("recover.json")).unwrap().session, None, "the deleted session is not reopened");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn notice_app() -> DemoApp {
        let mut app = DemoApp::new_with_sink(None);
        app.recovery.notice = Some(Notice {
            report: Some("crash-20261121-121014-3028061.txt".into()),
            restored: Some("'Gig' at Encore · Outro".into()),
        });
        app
    }

    /// One frame at `t` seconds; every text it painted, with its rect.
    fn frame(app: &mut DemoApp, ctx: &egui::Context, size: Vec2, t: f64, events: Vec<egui::Event>) -> Vec<(String, Rect)> {
        fn walk(s: &egui::Shape, acc: &mut Vec<(String, Rect)>) {
            match s {
                egui::Shape::Text(t) => acc.push((t.galley.text().to_string(), t.galley.rect.translate(t.pos.to_vec2()))),
                egui::Shape::Vec(v) => v.iter().for_each(|x| walk(x, acc)),
                _ => {}
            }
        }
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, size));
        raw.time = Some(t); // past the popup fade-in (rule 101)
        raw.events = events;
        let mut out = ctx.run_ui(raw, |ui| app.frame_ui(ui));
        out.textures_delta.clear(); // rule 11
        let mut acc = Vec::new();
        out.shapes.iter().for_each(|cs| walk(&cs.shape, &mut acc));
        acc
    }

    #[test]
    fn the_crash_notice_fits_small_windows_and_dismiss_closes_it() {
        const NOTICE: [&str; 5] = ["kabelsalat restarted after a crash", "The report stays", "Open folder", "Report on GitHub", "Dismiss"];
        for size in [Vec2::new(760.0, 520.0), Vec2::new(1600.0, 1000.0)] {
            let mut app = notice_app();
            let ctx = egui::Context::default();
            theme::install_fonts(&ctx);
            let mut texts = Vec::new();
            for i in 0..4 {
                texts = frame(&mut app, &ctx, size, i as f64, Vec::new());
            }
            let shown: Vec<&(String, Rect)> = texts.iter().filter(|(s, _)| NOTICE.iter().any(|n| s.starts_with(n))).collect();
            assert_eq!(shown.len(), NOTICE.len(), "{size:?}: {shown:?}");
            let screen = Rect::from_min_size(Pos2::ZERO, size);
            for (i, (s, r)) in shown.iter().enumerate() {
                assert!(screen.contains_rect(*r), "{size:?}: {s:?} {r:?} leaves the window");
                for (s2, r2) in &shown[i + 1..] {
                    let o = r.intersect(*r2);
                    assert!(o.width() <= 1.0 || o.height() <= 1.0, "{size:?}: {s:?} overlaps {s2:?}");
                }
            }
            let at = shown.iter().find(|(s, _)| s == "Dismiss").unwrap().1.center();
            frame(&mut app, &ctx, size, 4.0, vec![egui::Event::PointerMoved(at)]);
            assert!(app.crash_notice_covers(at) && matches!(app.hover, Some(Hit::Background)), "{size:?}: the rack under the notice takes the pointer");
            frame(&mut app, &ctx, size, 4.1, vec![egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: true, modifiers: Default::default() }]);
            frame(&mut app, &ctx, size, 4.2, vec![egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: false, modifiers: Default::default() }]);
            assert!(app.recovery.notice.is_none(), "{size:?}: Dismiss left the notice up");
            let after = frame(&mut app, &ctx, size, 4.3, Vec::new());
            assert!(!after.iter().any(|(s, _)| s.starts_with(NOTICE[0])), "{size:?}: the notice is still painted");
        }
    }

    #[test]
    #[ignore = "writes PNGs: KABELSALAT_SHOTS=<dir> cargo test --bin kabelsalat crash_notice_shots -- --ignored"]
    fn crash_notice_shots() {
        let dir = std::path::PathBuf::from(std::env::var("KABELSALAT_SHOTS").expect("KABELSALAT_SHOTS=<dir>"));
        std::fs::create_dir_all(&dir).unwrap();
        for (w, h) in [(1280.0, 800.0), (760.0, 520.0)] {
            let mut app = notice_app();
            crate::ui::offline_shots::shot(&mut app, Vec2::new(w, h), &dir.join(format!("crash-notice-{w}x{h}.png")));
        }
    }

    #[test]
    fn the_issue_link_carries_the_report_and_fits_github() {
        let report = "kabelsalat crash report\ncause: SIGSEGV (signal 11), fault address 0x0\n".to_string() + &"  #1  kabelsalat+0x1234\n".repeat(600);
        let url = issue_url(&report);
        assert!(url.starts_with("https://github.com/ssalentin/kabelsalat/issues/new?title=Crash%3A%20SIGSEGV%20%28signal%2011%29"), "{url}");
        assert!(url.len() <= ISSUE_URL_MAX, "{}", url.len());
        assert!(url.contains("kabelsalat%2B0x1234") && url.contains("report%20cut%20to%20fit") && url.ends_with("%60%60%60%0A"));
    }
}
