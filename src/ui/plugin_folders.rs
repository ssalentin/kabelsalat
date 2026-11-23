//! Settings → Plugins (#414): the custom CLAP, VST3 and LV2 search folders,
//! and the agent tools that list and change them.
//!
//! The folders themselves live in `crate::plugin_dirs`; this file draws
//! them, edits them, and starts the background rescan after a change. No
//! scan and no file access runs on the UI thread here (rules 46, 57, 127):
//! whether a folder exists is asked on a short-lived thread.

use super::*;
use crate::plugin_dirs::{self, Format, PluginDirs};
use std::path::PathBuf;

/// How often the page asks again whether its folders exist (a drive that
/// was plugged in meanwhile).
const EXISTS_EVERY_S: f64 = 3.0;

/// The text field open on the page: a new folder for `format`, or its entry
/// `index` under edit.
pub(super) struct FolderEdit {
    format: Format,
    index: Option<usize>,
    text: String,
    error: Option<String>,
    /// Take the keyboard focus in the next frame it is drawn.
    focus: bool,
}

#[derive(Default)]
pub(super) struct PluginFoldersState {
    pub(super) edit: Option<FolderEdit>,
    /// Whether each custom folder exists, as last asked off the UI thread.
    /// A folder not in here yet is shown without a mark.
    exists: std::collections::HashMap<PathBuf, bool>,
    exists_rx: Option<std::sync::mpsc::Receiver<Vec<(PathBuf, bool)>>>,
    exists_at: Option<f64>,
}

/// A change made on the Plugins page, applied after the frame drew it.
pub(super) enum FolderAction {
    /// New folders, and what was done to get them ("Added CLAP folder ~/x").
    Set(PluginDirs, String),
    Rescan,
}

impl DemoApp {
    /// The rail's line for the Plugins page.
    pub(super) fn plugin_folders_summary() -> String {
        match plugin_dirs::current().len() {
            0 => "standard folders".into(),
            1 => "1 custom folder".into(),
            n => format!("{n} custom folders"),
        }
    }

    /// Make `dirs` the folders this app searches, keep them in the config
    /// file and rescan in the background. Err: the config could not be
    /// written (the folders still apply until the app quits).
    pub(super) fn set_plugin_folders(&mut self, dirs: PluginDirs) -> Result<(), String> {
        let saved = plugin_dirs::save(&dirs);
        plugin_dirs::set(dirs);
        self.plugin_folders.exists_at = None;
        // A scan under way read the old folders: its result is dropped.
        self.plugin_scan = None;
        self.start_plugin_scan(false);
        self.poll_plugin_scan();
        saved.map_err(|e| e.to_string())
    }

    pub(super) fn apply_plugin_folder_action(&mut self, action: FolderAction) {
        match action {
            FolderAction::Set(dirs, what) => {
                self.settings_msg = match self.set_plugin_folders(dirs) {
                    Ok(()) => format!("{what} · rescanning plugins"),
                    Err(e) => format!("{what}, but saving the folders failed: {e}"),
                };
            }
            FolderAction::Rescan => {
                self.refresh_plugins_with(true);
                self.settings_msg = "Rescanning plugins".into();
            }
        }
    }

    /// `list_plugin_folders`, `add_plugin_folder`, `remove_plugin_folder`.
    pub(super) fn plugin_folder_tool(&mut self, name: &str, args: &serde_json::Value) -> String {
        if name == plugin_dirs::LIST_TOOL {
            let exists = &self.plugin_folders.exists;
            let mut v = plugin_dirs::describe(&plugin_dirs::current(), |p| exists.get(p).copied());
            v["scanning"] = serde_json::json!(self.plugin_scan.is_some());
            return v.to_string();
        }
        let mut dirs = plugin_dirs::current();
        match plugin_dirs::tool_edit(&mut dirs, name, args) {
            Ok(what) => match self.set_plugin_folders(dirs) {
                Ok(()) => format!("{what}. Plugins are being rescanned in the background: call list_plugins in a few seconds."),
                Err(e) => format!("{what} for this run, but NOT saved to the config file: {e}"),
            },
            Err(e) => e,
        }
    }

    /// Ask, off the UI thread, which custom folders exist; adopt an answer.
    fn poll_plugin_folder_exists(&mut self, now: f64) {
        let st = &mut self.plugin_folders;
        if let Some(rx) = &st.exists_rx {
            match rx.try_recv() {
                Ok(list) => {
                    st.exists = list.into_iter().collect();
                    st.exists_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => st.exists_rx = None,
            }
        }
        if st.exists_at.is_some_and(|t| now - t < EXISTS_EVERY_S) {
            return;
        }
        st.exists_at = Some(now);
        let dirs = plugin_dirs::current();
        let paths: Vec<PathBuf> = Format::ALL.into_iter().flat_map(|f| dirs.of(f).clone()).collect();
        if paths.is_empty() {
            st.exists.clear();
            return;
        }
        let check = move || paths.into_iter().map(|p| { let there = p.is_dir(); (p, there) }).collect::<Vec<_>>();
        if cfg!(test) {
            st.exists = check().into_iter().collect();
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        if std::thread::Builder::new()
            .name("kabelsalat-plugin-folders".into())
            .spawn(move || { let _ = tx.send(check()); })
            .is_ok()
        {
            st.exists_rx = Some(rx);
        }
    }

    pub(super) fn settings_plugins_page(&mut self, ui: &mut egui::Ui, actions: &mut Vec<SettingsAction>) {
        self.poll_plugin_folder_exists(ui.input(|i| i.time));
        let dirs = plugin_dirs::current();
        let status = if self.plugin_scan.is_some() {
            "Scanning in the background. The Plugins view fills in when it is done.".to_string()
        } else if self.plugins_loaded {
            format!(
                "Found {} CLAP, {} VST3 and {} LV2 plugins.",
                self.plugins_clap.len(),
                self.plugins_vst3.len(),
                self.plugins_lv2.len(),
            )
        } else {
            "Scanned when the Plugins view opens or a folder changes.".to_string()
        };
        Self::settings_row(ui, "Installed plugins", &status, |ui| {
            if theme::chip_button(ui, "Rescan", false).clicked() {
                actions.push(SettingsAction::Plugins(FolderAction::Rescan));
            }
        });
        for format in Format::ALL {
            Self::settings_gap(ui);
            self.plugin_folder_section(ui, format, dirs.of(format), actions);
        }
    }

    fn plugin_folder_section(&mut self, ui: &mut egui::Ui, format: Format, list: &[PathBuf], actions: &mut Vec<SettingsAction>) {
        let open = self.plugin_folders.edit.as_ref().filter(|e| e.format == format).map(|e| e.index);
        let adding = open == Some(None);
        let full = list.len() >= plugin_dirs::MAX_PER_FORMAT;
        let mut start: Option<Option<usize>> = None;
        let mut remove: Option<usize> = None;
        Self::settings_row(
            ui,
            &format!("{} folders", format.label()),
            &format!("Always searched: {} and ${}.", format.standard().join(", "), format.env_var()),
            |ui| {
                ui.add_enabled_ui(!adding && !full, |ui| {
                    if theme::chip_button(ui, "Add folder", false).clicked() {
                        start = Some(None);
                    }
                });
            },
        );
        ui.add_space(6.0);
        if list.is_empty() && !adding {
            ui.label(egui::RichText::new("No custom folders.").size(11.5).color(theme::CREAM_DIM));
        }
        for (i, path) in list.iter().enumerate() {
            if open == Some(Some(i)) {
                self.plugin_folder_edit_row(ui, actions);
                continue;
            }
            let missing = self.plugin_folders.exists.get(path) == Some(&false);
            ui.horizontal(|ui| {
                ui.set_min_height(26.0); // rule 117
                let gap = ui.spacing().item_spacing.x;
                let note = egui::RichText::new("not found").size(11.5).color(theme::ACCENT_BRIGHT);
                let note_w = if missing {
                    ui.painter().layout_no_wrap("not found".into(), FontId::proportional(11.5), theme::ACCENT_BRIGHT).size().x + gap
                } else {
                    0.0
                };
                // Rule 100: the path takes what the buttons leave.
                let buttons = theme::chip_button_width(ui, "Edit") + theme::chip_button_width(ui, "Remove") + gap * 2.0 + 4.0;
                let path_w = (ui.available_width() - buttons - note_w).max(40.0);
                let shown = plugin_dirs::display(path);
                let col = if missing { theme::CREAM_DIM } else { theme::CREAM };
                let g = Self::ellipsized(ui.painter(), &shown, FontId::proportional(12.5), col, path_w);
                let (r, resp) = ui.allocate_exact_size(egui::vec2(path_w, 26.0), egui::Sense::hover());
                ui.painter().galley(Pos2::new(r.left(), r.center().y - g.size().y * 0.5), g, col);
                resp.on_hover_text(path.to_string_lossy());
                if missing {
                    ui.label(note).on_hover_text("This folder does not exist right now. It stays on the list and is searched once it is there.");
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::danger_button(ui, "Remove", false).clicked() {
                        remove = Some(i);
                    }
                    if theme::chip_button(ui, "Edit", false).clicked() {
                        start = Some(Some(i));
                    }
                });
            });
        }
        if adding {
            self.plugin_folder_edit_row(ui, actions);
        }
        if let Some(i) = remove {
            let mut dirs = plugin_dirs::current();
            let path = list[i].clone();
            if dirs.remove(format, &path.to_string_lossy()) {
                self.plugin_folders.edit = None;
                let what = format!("Removed {} folder {}", format.label(), plugin_dirs::display(&path));
                actions.push(SettingsAction::Plugins(FolderAction::Set(dirs, what)));
            }
        } else if let Some(index) = start {
            let text = index.map(|i| plugin_dirs::display(&list[i])).unwrap_or_default();
            self.plugin_folders.edit = Some(FolderEdit { format, index, text, error: None, focus: true });
        }
    }

    /// The open text field with Save and Cancel, and under it why the last
    /// Save was refused.
    fn plugin_folder_edit_row(&mut self, ui: &mut egui::Ui, actions: &mut Vec<SettingsAction>) {
        let Some(edit) = self.plugin_folders.edit.as_mut() else { return };
        let gap = ui.spacing().item_spacing.x;
        let field_w = (ui.available_width()
            - theme::primary_button_width(ui, "Save")
            - theme::chip_button_width(ui, "Cancel")
            - gap * 2.0
            - RECESSED_INPUT_CHROME)
            .max(60.0);
        let (mut save, mut cancel) = (false, false);
        ui.horizontal(|ui| {
            let field = egui::Frame::new()
                .fill(theme::PAPER_SINK)
                .stroke(Stroke::new(1.0, theme::HAIRLINE))
                .corner_radius(2.0)
                .inner_margin(egui::Margin::symmetric(8, 5))
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut edit.text)
                            .id_salt(("settings_plugin_folder", edit.format.key()))
                            .frame(egui::Frame::NONE)
                            .desired_width(field_w)
                            .hint_text("/path/to/plugins"),
                    )
                })
                .inner;
            if std::mem::take(&mut edit.focus) {
                field.request_focus();
            }
            if field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                save = true;
            }
            if theme::primary_button(ui, "Save").clicked() {
                save = true;
            }
            if theme::chip_button(ui, "Cancel", false).clicked() {
                cancel = true;
            }
        });
        if let Some(err) = &edit.error {
            ui.add(egui::Label::new(egui::RichText::new(err).size(11.5).color(theme::CLIP_BRIGHT)).wrap());
        }
        if cancel {
            self.plugin_folders.edit = None;
        } else if save {
            let mut dirs = plugin_dirs::current();
            let done = match edit.index {
                None => dirs.add(edit.format, &edit.text).map(|p| format!("Added {} folder {}", edit.format.label(), plugin_dirs::display(&p))),
                Some(i) => dirs.replace(edit.format, i, &edit.text).map(|p| format!("{} folder set to {}", edit.format.label(), plugin_dirs::display(&p))),
            };
            match done {
                Ok(what) => {
                    self.plugin_folders.edit = None;
                    actions.push(SettingsAction::Plugins(FolderAction::Set(dirs, what)));
                }
                Err(e) => {
                    let mut e = e;
                    if let Some(first) = e.get_mut(0..1) {
                        first.make_ascii_uppercase();
                    }
                    edit.error = Some(format!("{e}."));
                    edit.focus = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::settings_and_dropdown_tests::{assert_settings_text_layout, click_at, key, open_settings_page, run_frame};
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ksalat-plugin-folders-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn type_text(app: &mut DemoApp, ctx: &egui::Context, size: Vec2, text: &str) {
        run_frame(app, ctx, size, vec![egui::Event::Text(text.into())]);
    }

    /// The centre of the `n`th text `label` from the top.
    fn nth(texts: &[(String, Rect)], label: &str, n: usize) -> Pos2 {
        let mut hits: Vec<&Rect> = texts.iter().filter(|(t, _)| t == label).map(|(_, r)| r).collect();
        hits.sort_by(|a, b| a.top().total_cmp(&b.top()));
        hits.get(n).unwrap_or_else(|| panic!("no {label:?} #{n} in {texts:?}")).center()
    }

    /// An LV2 bundle folder `<dir>/<name>.lv2` that declares one plugin with
    /// a binary of this machine (lilv reads only the Turtle files).
    fn lv2_bundle(dir: &std::path::Path, name: &str, uri: &str) {
        let bundle = dir.join(format!("{name}.lv2"));
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(
            bundle.join("manifest.ttl"),
            format!(
                "@prefix lv2: <http://lv2plug.in/ns/lv2core#> .\n@prefix doap: <http://usefulinc.com/ns/doap#> .\n\
                 <{uri}> a lv2:Plugin ; doap:name \"Folder Probe {name}\" ; lv2:binary <probe.so> .\n"
            ),
        )
        .unwrap();
        std::os::unix::fs::symlink(std::env::current_exe().unwrap(), bundle.join("probe.so")).unwrap();
    }

    /// The whole way a user goes, through real frames: open Settings →
    /// Plugins, add an LV2 folder by typing it, and the rescan that follows
    /// finds the plugin in it; a bad path is refused with a reason; an edit
    /// moves the entry; Remove takes it off and the plugin is gone again.
    #[test]
    fn a_folder_added_in_settings_is_scanned_and_edit_and_remove_follow() {
        let root = scratch("flow");
        let (first, second) = (root.join("first"), root.join("second"));
        let uri_a = format!("urn:kabelsalat:test:plugin-folders:a{}", std::process::id());
        let uri_b = format!("urn:kabelsalat:test:plugin-folders:b{}", std::process::id());
        lv2_bundle(&first, "a", &uri_a);
        lv2_bundle(&second, "b", &uri_b);

        let size = Vec2::new(781.0, 950.0);
        let mut app = DemoApp::new_with_sink(None);
        app.stage_view = StageView::Settings;
        let ctx = egui::Context::default();
        let (texts, _, _) = open_settings_page(&mut app, &ctx, size, "Plugins");
        assert_eq!(app.settings_page, SettingsPage::Plugins);
        for want in ["Installed plugins", "CLAP folders", "VST3 folders", "LV2 folders"] {
            assert!(texts.iter().any(|(t, _)| t == want), "{want:?} on the page: {texts:?}");
        }
        assert_eq!(texts.iter().filter(|(t, _)| t == "No custom folders.").count(), 3);
        assert!(!app.plugins_loaded, "opening the page scans nothing");

        // Add folder (the third: LV2), a relative path first.
        click_at(&mut app, &ctx, size, nth(&texts, "Add folder", 2));
        run_frame(&mut app, &ctx, size, vec![]);
        type_text(&mut app, &ctx, size, "plugins/lv2");
        let (texts, _, _) = run_frame(&mut app, &ctx, size, vec![]);
        click_at(&mut app, &ctx, size, nth(&texts, "Save", 0));
        let (texts, _, _) = run_frame(&mut app, &ctx, size, vec![]);
        assert!(texts.iter().any(|(t, _)| t == "Use a full path that starts with / or ~."), "the refusal says why: {texts:?}");
        assert_settings_text_layout(&app, &texts, size);
        assert!(plugin_dirs::current().lv2.is_empty() && !app.plugins_loaded);

        // Replace it with the real folder and press Enter.
        run_frame(&mut app, &ctx, size, vec![egui::Event::Key {
            key: Key::A, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::COMMAND,
        }]);
        type_text(&mut app, &ctx, size, &format!("{}/", first.display()));
        run_frame(&mut app, &ctx, size, key(Key::Enter));
        let (texts, _, _) = run_frame(&mut app, &ctx, size, vec![]);
        assert_eq!(plugin_dirs::current().lv2, [first.clone()], "stored without the trailing slash");
        assert!(app.plugin_folders.edit.is_none());
        assert!(app.plugins_loaded, "the change rescans");
        assert!(app.plugins_lv2.iter().any(|p| p.uri == uri_a), "the scan finds the plugin of the custom folder");
        assert!(app.settings_msg.starts_with("Added LV2 folder") && app.settings_msg.ends_with("rescanning plugins"), "{}", app.settings_msg);
        assert!(texts.iter().any(|(t, _)| *t == plugin_dirs::display(&first)), "the folder is listed: {texts:?}");
        assert_eq!(texts.iter().filter(|(t, _)| t == "No custom folders.").count(), 2);
        assert_settings_text_layout(&app, &texts, size);

        // Edit: the field holds the folder; point it at the second one.
        click_at(&mut app, &ctx, size, nth(&texts, "Edit", 0));
        let (texts, _, _) = run_frame(&mut app, &ctx, size, vec![]);
        assert!(texts.iter().any(|(t, _)| *t == plugin_dirs::display(&first)), "the edit starts from the folder: {texts:?}");
        run_frame(&mut app, &ctx, size, vec![egui::Event::Key {
            key: Key::A, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::COMMAND,
        }]);
        type_text(&mut app, &ctx, size, second.to_str().unwrap());
        let (texts, _, _) = run_frame(&mut app, &ctx, size, vec![]);
        click_at(&mut app, &ctx, size, nth(&texts, "Save", 0));
        let (texts, _, _) = run_frame(&mut app, &ctx, size, vec![]);
        assert_eq!(plugin_dirs::current().lv2, [second.clone()]);
        assert!(app.plugins_lv2.iter().any(|p| p.uri == uri_b) && app.plugins_lv2.iter().all(|p| p.uri != uri_a), "the rescan follows the edit");

        // Cancel leaves the list alone.
        click_at(&mut app, &ctx, size, nth(&texts, "Edit", 0));
        let (texts, _, _) = run_frame(&mut app, &ctx, size, vec![]);
        click_at(&mut app, &ctx, size, nth(&texts, "Cancel", 0));
        let (texts, _, _) = run_frame(&mut app, &ctx, size, vec![]);
        assert!(app.plugin_folders.edit.is_none());
        assert_eq!(plugin_dirs::current().lv2, [second.clone()]);

        // Remove.
        click_at(&mut app, &ctx, size, nth(&texts, "Remove", 0));
        let (texts, _, _) = run_frame(&mut app, &ctx, size, vec![]);
        assert!(plugin_dirs::current().lv2.is_empty());
        assert!(app.plugins_lv2.iter().all(|p| p.uri != uri_b), "a removed folder's plugins leave the list");
        assert_eq!(texts.iter().filter(|(t, _)| t == "No custom folders.").count(), 3);
        assert!(app.settings_msg.starts_with("Removed LV2 folder"), "{}", app.settings_msg);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Rule 100: with long paths in every list, a missing folder, an open
    /// editor and a refusal under it, every text stays in the sheet and
    /// nothing overlaps, at the default window size and at the small ones.
    #[test]
    fn the_plugins_page_fits_small_windows_with_long_paths_and_an_open_editor() {
        let root = scratch("fit");
        let long = root.join("a studio drive with a long name/plugins that were collected over many years/and one more level");
        std::fs::create_dir_all(&long).unwrap();
        let mut dirs = PluginDirs::default();
        for f in Format::ALL {
            dirs.add(f, long.join(f.key()).to_str().unwrap()).unwrap();
            dirs.add(f, &format!("/mnt/unplugged-drive/{}", f.key())).unwrap();
        }
        std::fs::create_dir_all(long.join("clap")).unwrap();
        for size in [Vec2::new(781.0, 950.0), Vec2::new(760.0, 520.0), Vec2::new(781.0, 468.0), Vec2::new(1280.0, 900.0)] {
            plugin_dirs::set(dirs.clone());
            let mut app = DemoApp::new_with_sink(None);
            app.stage_view = StageView::Settings;
            let ctx = egui::Context::default();
            let (texts, _, _) = open_settings_page(&mut app, &ctx, size, "Plugins");
            assert_settings_text_layout(&app, &texts, size);
            assert!(texts.iter().any(|(t, _)| t == "not found"), "{size:?}: the missing folder is marked: {texts:?}");
            assert!(texts.iter().any(|(t, _)| t == "Edit") && texts.iter().any(|(t, _)| t == "Remove"), "{size:?}");
            for (t, r) in texts.iter().filter(|(t, _)| ["Edit", "Cancel", "Add folder", "Rescan"].contains(&t.as_str())) {
                assert!(r.height() < 20.0, "{size:?}: button text {t:?} wrapped: {r:?}");
            }

            // The editor of the first CLAP folder, refused as a duplicate.
            app.plugin_folders.edit = Some(FolderEdit {
                format: Format::Clap,
                index: Some(0),
                text: format!("{}/", plugin_dirs::display(&dirs.clap[1])),
                error: None,
                focus: true,
            });
            run_frame(&mut app, &ctx, size, vec![]);
            let (texts, _, _) = run_frame(&mut app, &ctx, size, vec![]);
            click_at(&mut app, &ctx, size, nth(&texts, "Save", 0));
            let (texts, _, _) = run_frame(&mut app, &ctx, size, vec![]);
            assert!(app.plugin_folders.edit.as_ref().is_some_and(|e| e.error.is_some()), "{size:?}: a duplicate is refused");
            assert!(texts.iter().any(|(t, _)| t.ends_with("is already a CLAP folder.")), "{size:?}: {texts:?}");
            assert_settings_text_layout(&app, &texts, size);
            assert_eq!(plugin_dirs::current(), dirs, "{size:?}: a refused edit changes nothing");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Rules 6, 23, 66: the three tools are in the assistant's table, served
    /// over the UI bridge, and do what the page does.
    #[test]
    fn the_folder_tools_are_offered_served_and_rescan() {
        let names: Vec<String> = crate::assistant::tools()
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["function"]["name"].as_str().map(str::to_string))
            .collect();
        for tool in plugin_dirs::EDIT_TOOLS.into_iter().chain([plugin_dirs::LIST_TOOL]) {
            assert!(names.iter().any(|n| n == tool), "assistant tools miss {tool}");
            assert!(UI_BRIDGE_TOOLS.contains(&tool), "UI_BRIDGE_TOOLS misses {tool}");
        }
        assert!(tool_is_read_only(plugin_dirs::LIST_TOOL));
        for tool in plugin_dirs::EDIT_TOOLS {
            assert!(!tool_is_read_only(tool) && tool_changes_only_system(tool), "{tool} changes the machine, not the patch");
        }

        let root = scratch("tools");
        let uri = format!("urn:kabelsalat:test:plugin-folders:tool{}", std::process::id());
        lv2_bundle(&root, "t", &uri);
        let mut app = DemoApp::new_with_sink(None);
        let path = root.to_str().unwrap();

        let out = app.exec_tool("add_plugin_folder", &serde_json::json!({"format": "lv2", "path": path}));
        assert!(out.starts_with("Added LV2 folder") && out.contains("list_plugins"), "{out}");
        assert_eq!(plugin_dirs::current().lv2, [root.clone()]);
        assert!(app.plugins_lv2.iter().any(|p| p.uri == uri), "the tool's change rescans");

        let listed: serde_json::Value = serde_json::from_str(&app.exec_tool("list_plugin_folders", &serde_json::json!({}))).unwrap();
        assert_eq!(listed["lv2"]["custom"][0]["path"], path);
        assert_eq!(listed["clap"]["custom"], serde_json::json!([]));
        assert_eq!(listed["scanning"], false);

        let twice = app.exec_tool("add_plugin_folder", &serde_json::json!({"format": "lv2", "path": path}));
        assert!(twice.contains("already"), "{twice}");
        let bad = app.exec_tool("add_plugin_folder", &serde_json::json!({"format": "vst", "path": path}));
        assert!(bad.contains("clap, vst3 or lv2"), "{bad}");

        let out = app.exec_tool("remove_plugin_folder", &serde_json::json!({"format": "lv2", "path": path}));
        assert!(out.starts_with("Removed LV2 folder"), "{out}");
        assert!(plugin_dirs::current().lv2.is_empty());
        assert!(app.plugins_lv2.iter().all(|p| p.uri != uri));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `KABELSALAT_SHOTS=<dir> cargo test --bin kabelsalat plugin_folder_shots -- --ignored`
    #[test]
    #[ignore]
    fn plugin_folder_shots() {
        let Some(out) = std::env::var_os("KABELSALAT_SHOTS").map(PathBuf::from) else { return };
        std::fs::create_dir_all(&out).unwrap();
        let root = scratch("shots");
        std::fs::create_dir_all(root.join("clap")).unwrap();
        for (w, h, name, state) in [
            (781.0, 950.0, "plugin-folders-empty-781x950.png", 0),
            (781.0, 950.0, "plugin-folders-781x950.png", 1),
            (760.0, 520.0, "plugin-folders-760x520.png", 1),
            (1280.0, 800.0, "plugin-folders-1280x800.png", 1),
            (781.0, 950.0, "plugin-folders-editing-781x950.png", 2),
            (1280.0, 800.0, "plugin-folders-editing-1280x800.png", 2),
        ] {
            let mut dirs = PluginDirs::default();
            if state > 0 {
                dirs.add(Format::Clap, "~/studio/clap").unwrap();
                dirs.add(Format::Clap, root.join("clap").to_str().unwrap()).unwrap();
                dirs.add(Format::Vst3, "/mnt/samples-drive/plugins/vst3").unwrap();
                dirs.add(Format::Lv2, "/opt/audio/a folder with a long name for collected plugins/lv2").unwrap();
            }
            plugin_dirs::set(dirs);
            let mut app = DemoApp::new_with_sink(None);
            app.stage_view = StageView::Settings;
            app.settings_page = SettingsPage::Plugins;
            if state == 2 {
                app.plugin_folders.edit = Some(FolderEdit {
                    format: Format::Vst3,
                    index: None,
                    text: "plugins/vst3".into(),
                    error: Some("Use a full path that starts with / or ~.".into()),
                    focus: false,
                });
                app.settings_msg = "Added CLAP folder ~/studio/clap · rescanning plugins".into();
            }
            super::super::offline_shots::shot(&mut app, Vec2::new(w, h), &out.join(name));
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
