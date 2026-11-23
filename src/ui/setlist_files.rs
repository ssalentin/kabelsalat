//! Saved setlists (`<name>.ksalat-setlist`) in a user-configurable folder:
//! bar in the Setlist stage, Save/Load modals, folder setting in Settings →
//! Session, and assistant/MCP parity.

use super::*;
use crate::session::{
    delete_setlist, list_setlists, normalize_folder_path, save_setlists_dir, SavedSetlist,
};
use std::path::{Path, PathBuf};

/// Inline folder path editor shared by the Setlist modals and Settings → Session.
#[derive(Clone, Debug)]
pub(super) struct FolderPrompt {
    pub(super) text: String,
    pub(super) error: Option<String>,
    pub(super) focus: bool,
}

/// Modal state for "Save setlist".
#[derive(Clone, Debug)]
pub(super) struct SaveSetlistDialog {
    pub(super) name: String,
    pub(super) error: Option<String>,
    pub(super) focus: bool,
    pub(super) folder: Option<FolderPrompt>,
}

/// Modal state for "Load setlist".
#[derive(Clone, Debug)]
pub(super) struct LoadSetlistDialog {
    pub(super) confirm_delete: Option<String>,
    pub(super) folder: Option<FolderPrompt>,
    pub(super) error: Option<String>,
}

/// Two-step confirmation for deleting/overwriting a setlist in the Library rail or popover.
#[derive(Clone, PartialEq, Eq)]
pub(super) enum SetlistConfirm {
    Delete(String),
    Overwrite(String),
}

enum FolderRowEvent {
    None,
    Applied(PathBuf),
    Cancelled,
}

impl DemoApp {
    /// Default setlist storage dir (isolated per test instance under `cfg(test)`, rules 37/169).
    #[cfg(test)]
    pub(super) fn default_setlists_dir_for_app() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let base = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join("test-setlists").join(format!(
            "{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    #[cfg(not(test))]
    pub(super) fn default_setlists_dir_for_app() -> PathBuf {
        crate::session::default_setlists_dir()
    }

    /// Active folder where saved setlists (`.ksalat-setlist`) are stored and listed.
    pub(super) fn effective_setlists_dir(&self) -> &Path {
        self.setlists_dir_custom
            .as_deref()
            .unwrap_or(&self.setlists_dir_default)
    }

    pub(super) fn setlists_dir_is_custom(&self) -> bool {
        self.setlists_dir_custom.is_some()
    }

    /// Update the configured setlists folder (`None` resets to default) and persist to `config.json`.
    pub(super) fn set_setlists_dir(&mut self, dir: Option<PathBuf>) -> Result<String, String> {
        let saved = save_setlists_dir(dir.as_deref());
        self.setlists_dir_custom = dir;
        let shown = crate::plugin_dirs::display(self.effective_setlists_dir());
        let prefix = if self.setlists_dir_custom.is_some() {
            format!("Setlists folder set to {shown}")
        } else {
            format!("Setlists folder reset to {shown}")
        };
        match saved {
            Ok(()) => Ok(prefix),
            Err(e) => Err(format!("{prefix}, but saving the folder failed: {e}")),
        }
    }

    pub(super) fn apply_setlists_dir_setting(&mut self, dir: Option<PathBuf>) {
        self.settings_msg = match self.set_setlists_dir(dir) {
            Ok(msg) | Err(msg) => msg,
        };
    }

    /// Snapshot the current gig (setlist + all rigs, states, knobs, mix, MIDI routes)
    /// and save it as `<name>.ksalat-setlist` inside `dir`.
    pub(super) fn save_setlist_to_dir(&mut self, dir: &Path, name: &str) -> anyhow::Result<PathBuf> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            anyhow::bail!("The setlist needs a name before it can be saved.");
        }
        let session = self.capture_session(trimmed)?;
        let saved = SavedSetlist::new(trimmed, session);
        let path = SavedSetlist::package_path(dir, trimmed)?;
        saved.save(&path)?;
        self.open_setlist = Some(saved.name);
        Ok(path)
    }

    /// Load `<name>.ksalat-setlist` from `dir` and apply it to the live rack.
    pub(super) fn load_setlist_from_dir(&mut self, dir: &Path, name: &str) -> anyhow::Result<()> {
        let path = SavedSetlist::package_path(dir, name)?;
        let saved = SavedSetlist::load(&path)?;
        self.apply_session(&saved.session)?;
        self.open_setlist = Some(saved.name);
        self.open_session = None;
        self.autosave_rebase();
        Ok(())
    }

    pub(super) fn open_save_setlist_dialog(&mut self) {
        let seed = self
            .open_setlist
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| self.open_session.as_deref().filter(|s| !s.trim().is_empty()))
            .or_else(|| {
                let s = self.session_name.trim();
                (!s.is_empty() && s != "Untitled").then_some(s)
            })
            .unwrap_or("My Setlist")
            .to_string();
        self.save_setlist_dialog = Some(SaveSetlistDialog {
            name: seed,
            error: None,
            focus: true,
            folder: None,
        });
    }

    pub(super) fn open_load_setlist_dialog(&mut self) {
        self.load_setlist_dialog = Some(LoadSetlistDialog {
            confirm_delete: None,
            folder: None,
            error: None,
        });
    }

    pub(super) fn open_setlist_folder_edit(&mut self) {
        self.setlist_folder_edit = Some(FolderPrompt {
            text: crate::plugin_dirs::display(self.effective_setlists_dir()),
            error: None,
            focus: true,
        });
    }

    /// Compact save/load/folder strip shown in the Setlist stage below the header.
    pub(super) fn setlist_file_bar(&mut self, ui: &mut egui::Ui) {
        let mut open_save = false;
        let mut open_load = false;
        let mut open_folder = false;
        let folder_disp = crate::plugin_dirs::display(self.effective_setlists_dir());
        let folder_full = self.effective_setlists_dir().display().to_string();
        let open_name = self.open_setlist.clone();

        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 4.0);
            if theme::wrapped_chip_button(ui, "Save setlist…", false)
                .on_hover_text("Save this setlist and all its rigs to disk (.ksalat-setlist)")
                .clicked()
            {
                open_save = true;
            }
            if theme::wrapped_chip_button(ui, "Load setlist…", false)
                .on_hover_text("Load a saved setlist from the setlists folder")
                .clicked()
            {
                open_load = true;
            }
            if let Some(name) = &open_name {
                let tag = format!("Saved · {name}");
                let max_w = (ui.max_rect().width() - 24.0).clamp(60.0, 220.0);
                let g = Self::ellipsized(ui.painter(), &tag, theme::medium_font(11.5), theme::LIVE_TEAL, max_w);
                let (r, resp) = ui.allocate_exact_size(egui::vec2(g.size().x, 22.0), egui::Sense::hover());
                ui.painter().galley(Pos2::new(r.left(), r.center().y - g.size().y * 0.5), g, theme::LIVE_TEAL);
                resp.on_hover_text(format!("Active saved setlist: {name}"));
            }
            let folder_btn_w = theme::chip_button_width(ui, "Folder…");
            let max_folder_w = (ui.max_rect().width() - folder_btn_w - 16.0).clamp(60.0, 260.0);
            let folder_lbl = format!("in {folder_disp}");
            let fg = Self::ellipsized(ui.painter(), &folder_lbl, FontId::proportional(11.0), theme::INK_2, max_folder_w);
            let (fr, fresp) = ui.allocate_exact_size(egui::vec2(fg.size().x, 22.0), egui::Sense::hover());
            ui.painter().galley(Pos2::new(fr.left(), fr.center().y - fg.size().y * 0.5), fg, theme::INK_2);
            fresp.on_hover_text(format!("Setlists folder: {folder_full}"));
            if theme::wrapped_chip_button(ui, "Folder…", false)
                .on_hover_text("Change where saved setlists are stored (Settings → Session)")
                .clicked()
            {
                open_folder = true;
            }
        });

        if open_save {
            self.open_save_setlist_dialog();
        }
        if open_load {
            self.open_load_setlist_dialog();
        }
        if open_folder {
            self.stage_view = StageView::Settings;
            self.settings_page = SettingsPage::Session;
            self.open_setlist_folder_edit();
        }
    }

    fn draw_folder_inline_editor(
        ui: &mut egui::Ui,
        prompt: &mut FolderPrompt,
        id_salt: &str,
        save_label: &str,
    ) -> FolderRowEvent {
        let gap = ui.spacing().item_spacing.x;
        let field_w = (ui.available_width()
            - theme::primary_button_width(ui, save_label)
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
                        egui::TextEdit::singleline(&mut prompt.text)
                            .id_salt(id_salt)
                            .frame(egui::Frame::NONE)
                            .desired_width(field_w)
                            .hint_text("/path/to/setlists"),
                    )
                })
                .inner;
            if std::mem::take(&mut prompt.focus) {
                field.request_focus();
            }
            if field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                save = true;
            }
            if theme::primary_button(ui, save_label).clicked() {
                save = true;
            }
            if theme::chip_button(ui, "Cancel", false).clicked() {
                cancel = true;
            }
        });
        if let Some(err) = &prompt.error {
            ui.add(egui::Label::new(egui::RichText::new(err).size(11.5).color(theme::CLIP_BRIGHT)).wrap());
        }
        if cancel {
            return FolderRowEvent::Cancelled;
        }
        if save {
            match normalize_folder_path(&prompt.text) {
                Ok(path) => return FolderRowEvent::Applied(path),
                Err(mut e) => {
                    if let Some(first) = e.get_mut(0..1) {
                        first.make_ascii_uppercase();
                    }
                    prompt.error = Some(format!("{e}."));
                    prompt.focus = true;
                }
            }
        }
        FolderRowEvent::None
    }

    fn draw_modal_folder_section(
        &mut self,
        ui: &mut egui::Ui,
        folder: &mut Option<FolderPrompt>,
        id_salt: &str,
    ) {
        let dir = self.effective_setlists_dir().to_path_buf();
        let disp = crate::plugin_dirs::display(&dir);
        let is_custom = self.setlists_dir_is_custom();
        let mut start_edit = false;
        let mut reset_folder = false;

        ui.horizontal(|ui| {
            ui.set_min_height(24.0);
            let gap = ui.spacing().item_spacing.x;
            let reset_w = if is_custom { theme::chip_button_width(ui, "Reset") + gap } else { 0.0 };
            let change_w = theme::chip_button_width(ui, "Change…");
            let label_w = ui.painter().layout_no_wrap("Folder".into(), FontId::proportional(11.5), theme::INK_2).size().x + gap;
            let path_w = (ui.available_width() - label_w - reset_w - change_w - gap - 4.0).max(40.0);
            ui.label(egui::RichText::new("Folder").size(11.5).color(theme::INK_2));
            let g = Self::ellipsized(ui.painter(), &disp, FontId::proportional(11.5), theme::INK, path_w);
            let (r, resp) = ui.allocate_exact_size(egui::vec2(path_w, 22.0), egui::Sense::hover());
            ui.painter().galley(Pos2::new(r.left(), r.center().y - g.size().y * 0.5), g, theme::INK);
            resp.on_hover_text(dir.to_string_lossy());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::chip_button(ui, "Change…", folder.is_some()).clicked() {
                    start_edit = true;
                }
                if is_custom && theme::chip_button(ui, "Reset", false).clicked() {
                    reset_folder = true;
                }
            });
        });

        if start_edit {
            if folder.is_some() {
                *folder = None;
            } else {
                *folder = Some(FolderPrompt {
                    text: disp,
                    error: None,
                    focus: true,
                });
            }
        }
        if reset_folder {
            *folder = None;
            if let Err(e) = self.set_setlists_dir(None) {
                self.session_msg = e;
            }
        }
        if let Some(prompt) = folder.as_mut() {
            ui.add_space(4.0);
            match Self::draw_folder_inline_editor(ui, prompt, id_salt, "Apply") {
                FolderRowEvent::Applied(path) => {
                    *folder = None;
                    if let Err(e) = self.set_setlists_dir(Some(path)) {
                        self.session_msg = e;
                    }
                }
                FolderRowEvent::Cancelled => *folder = None,
                FolderRowEvent::None => {}
            }
        }
    }

    /// "Save setlist" modal dialog: Enter saves, Esc closes folder edit first then dialog.
    pub(super) fn save_setlist_modal(&mut self, ctx: &egui::Context) {
        let Some(mut dlg) = self.save_setlist_dialog.take() else { return };
        let mut decision: Option<bool> = None;
        let esc = ctx.input(|i| i.key_pressed(Key::Escape));
        let modal_w = (ctx.content_rect().width() - 48.0).clamp(280.0, 420.0);

        let modal = egui::Modal::new(egui::Id::unique("save_setlist_modal")).show(ctx, |ui| {
            ui.set_width(modal_w);
            ui.label(egui::RichText::new("Save setlist").font(theme::medium_font(16.0)).color(theme::INK));
            ui.add_space(4.0);
            ui.add(
                egui::Label::new(
                    egui::RichText::new("Saves songs, parts, sets, show controls and all loaded rigs, sounds, mix and MIDI routes.")
                        .size(12.0)
                        .color(theme::INK_2),
                )
                .wrap(),
            );
            ui.add_space(8.0);
            let field_w = (modal_w - 16.0).max(160.0);
            let edit = ui.add(
                egui::TextEdit::singleline(&mut dlg.name)
                    .id_salt("save_setlist_text")
                    .desired_width(field_w)
                    .char_limit(64)
                    .hint_text("Setlist name…"),
            );
            if std::mem::take(&mut dlg.focus) {
                edit.request_focus();
            }
            if dlg.folder.is_none() && edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                decision = Some(true);
            }
            if edit.changed() {
                dlg.error = None;
            }
            let trimmed = dlg.name.trim().to_string();
            let dir_now = self.effective_setlists_dir().to_path_buf();
            let exists = !trimmed.is_empty()
                && SavedSetlist::package_path(&dir_now, &trimmed).is_ok_and(|p| p.exists());
            if exists {
                ui.add_space(4.0);
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!("'{trimmed}' already exists here — saving will replace it."))
                            .size(11.5)
                            .color(theme::CABLE_AMBER),
                    )
                    .wrap(),
                );
            }
            if let Some(err) = &dlg.error {
                ui.add_space(4.0);
                ui.add(egui::Label::new(egui::RichText::new(err).size(11.5).color(theme::CLIP_BRIGHT)).wrap());
            }
            ui.add_space(10.0);
            Self::stage_hairline(ui);
            ui.add_space(8.0);
            self.draw_modal_folder_section(ui, &mut dlg.folder, "save_setlist_folder_text");
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if theme::primary_button(ui, "Save setlist").clicked() {
                    decision = Some(true);
                }
                ui.add_space(8.0);
                if theme::chip_button(ui, "Cancel", false).clicked() {
                    decision = Some(false);
                }
            });
        });
        if modal.should_close() && decision.is_none() {
            if esc && dlg.folder.is_some() {
                dlg.folder = None;
                dlg.focus = true;
            } else {
                decision = Some(false);
            }
        }
        match decision {
            Some(true) => {
                let dir = self.effective_setlists_dir().to_path_buf();
                let name = dlg.name.trim().to_string();
                match self.save_setlist_to_dir(&dir, &name) {
                    Ok(_) => {
                        let msg = format!("Saved setlist '{name}'");
                        self.session_msg = msg.clone();
                        self.notify_patch(&msg);
                    }
                    Err(e) => {
                        dlg.error = Some(format!("{e}"));
                        dlg.focus = true;
                        self.save_setlist_dialog = Some(dlg);
                    }
                }
            }
            Some(false) => {}
            None => self.save_setlist_dialog = Some(dlg),
        }
    }

    /// "Load setlist" modal dialog: list saved setlists, delete with confirm, change folder, load.
    pub(super) fn load_setlist_modal(&mut self, ctx: &egui::Context) {
        let Some(mut dlg) = self.load_setlist_dialog.take() else { return };
        let mut close = false;
        let mut load_name: Option<String> = None;
        let mut delete_name: Option<String> = None;
        let esc = ctx.input(|i| i.key_pressed(Key::Escape));

        let avail_h = ctx.content_rect().height();
        let list_h = (avail_h * 0.40).clamp(110.0, 260.0);
        let modal_w = (ctx.content_rect().width() - 48.0).clamp(280.0, 440.0);

        let modal = egui::Modal::new(egui::Id::unique("load_setlist_modal")).show(ctx, |ui| {
            ui.set_width(modal_w);
            ui.label(egui::RichText::new("Load setlist").font(theme::medium_font(16.0)).color(theme::INK));
            ui.add_space(6.0);
            self.draw_modal_folder_section(ui, &mut dlg.folder, "load_setlist_folder_text");
            let dir = self.effective_setlists_dir().to_path_buf();
            let names = list_setlists(&dir);
            let open_name = self.open_setlist.clone();

            ui.add_space(10.0);
            theme::section_label(ui, &format!("Saved setlists ({})", names.len()));
            ui.add_space(6.0);
            Self::stage_hairline(ui);
            egui::ScrollArea::vertical()
                .id_salt("load_setlist_scroll")
                .min_scrolled_height(list_h.min(110.0))
                .max_height(list_h)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if names.is_empty() {
                        ui.add_space(10.0);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new("No saved setlists in this folder yet. Use Save setlist… to save one.")
                                    .size(12.5)
                                    .color(theme::INK_2),
                            )
                            .wrap(),
                        );
                        return;
                    }
                    for name in &names {
                        ui.add_space(6.0);
                        let is_open = open_name.as_deref() == Some(name.as_str());
                        ui.horizontal(|ui| {
                            ui.set_min_height(26.0);
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                if dlg.confirm_delete.as_deref() == Some(name.as_str()) {
                                    if theme::chip_button(ui, "Cancel", false).clicked() {
                                        dlg.confirm_delete = None;
                                    }
                                    if theme::danger_button(ui, "Confirm?", true).clicked() {
                                        delete_name = Some(name.clone());
                                        dlg.confirm_delete = None;
                                    }
                                } else {
                                    if theme::danger_button(ui, "Delete", false).clicked() {
                                        dlg.confirm_delete = Some(name.clone());
                                    }
                                    if theme::primary_button(ui, "Load").clicked() {
                                        load_name = Some(name.clone());
                                    }
                                }
                                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(name)
                                                .font(theme::medium_font(13.5))
                                                .color(if is_open { theme::LIVE_TEAL } else { theme::INK }),
                                        )
                                        .truncate(),
                                    );
                                });
                            });
                        });
                        ui.add_space(6.0);
                        Self::stage_hairline(ui);
                    }
                });
            if let Some(err) = &dlg.error {
                ui.add_space(6.0);
                ui.add(egui::Label::new(egui::RichText::new(err).size(11.5).color(theme::CLIP_BRIGHT)).wrap());
            }
            ui.add_space(10.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::chip_button(ui, "Close", false).clicked() {
                    close = true;
                }
            });
        });

        if modal.should_close() {
            if esc && dlg.confirm_delete.is_some() {
                dlg.confirm_delete = None;
            } else if esc && dlg.folder.is_some() {
                dlg.folder = None;
            } else {
                close = true;
            }
        }
        if let Some(name) = delete_name {
            let dir = self.effective_setlists_dir().to_path_buf();
            match delete_setlist(&dir, &name) {
                Ok(()) => {
                    if self.open_setlist.as_deref() == Some(name.as_str()) {
                        self.open_setlist = None;
                    }
                    dlg.error = None;
                    self.session_msg = format!("Deleted setlist '{name}'");
                }
                Err(e) => dlg.error = Some(format!("Delete failed: {e}")),
            }
        }
        if let Some(name) = load_name {
            if self.rack_locked {
                self.notify_rack_locked();
                dlg.error = Some("Rack is locked for editing — unlock it first (Ctrl+L).".into());
                self.load_setlist_dialog = Some(dlg);
                return;
            }
            self.push_user_undo();
            let dir = self.effective_setlists_dir().to_path_buf();
            match self.load_setlist_from_dir(&dir, &name) {
                Ok(()) => {
                    let msg = format!("Loaded setlist '{name}'");
                    self.session_msg = msg.clone();
                    self.notify_patch(&msg);
                    return;
                }
                Err(e) => {
                    dlg.error = Some(format!("Load failed: {e}"));
                    self.load_setlist_dialog = Some(dlg);
                    return;
                }
            }
        }
        if !close {
            self.load_setlist_dialog = Some(dlg);
        }
    }

    /// Setlists folder configuration row in Settings → Session.
    pub(super) fn settings_setlist_folder_row(&mut self, ui: &mut egui::Ui, actions: &mut Vec<SettingsAction>) {
        let is_custom = self.setlists_dir_is_custom();
        let editing = self.setlist_folder_edit.is_some();
        let mut start_edit = false;
        let mut reset = false;

        Self::settings_row(
            ui,
            "Setlists folder",
            "Where saved setlists (.ksalat-setlist) are stored. Default: ~/kabelsalat-setlists (override with KABELSALAT_SETLISTS_DIR).",
            |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if theme::chip_button(ui, "Change", editing).clicked() {
                    start_edit = true;
                }
                if is_custom && theme::chip_button(ui, "Reset", false).clicked() {
                    reset = true;
                }
            },
        );
        ui.add_space(4.0);
        let disp = crate::plugin_dirs::display(self.effective_setlists_dir());
        let status_txt = if is_custom {
            format!("{disp} · custom")
        } else {
            format!("{disp} · default")
        };
        let status_col = if is_custom { theme::CREAM } else { theme::CREAM_DIM };
        let path_w = ui.available_width().max(40.0);
        let g = Self::ellipsized(ui.painter(), &status_txt, FontId::proportional(12.0), status_col, path_w);
        let (r, resp) = ui.allocate_exact_size(egui::vec2(path_w, 22.0), egui::Sense::hover());
        ui.painter().galley(Pos2::new(r.left(), r.center().y - g.size().y * 0.5), g, status_col);
        resp.on_hover_text(self.effective_setlists_dir().to_string_lossy());

        if start_edit {
            if self.setlist_folder_edit.is_some() {
                self.setlist_folder_edit = None;
            } else {
                self.open_setlist_folder_edit();
            }
        }
        if reset {
            self.setlist_folder_edit = None;
            actions.push(SettingsAction::SetSetlistsDir(None));
        }
        if let Some(prompt) = self.setlist_folder_edit.as_mut() {
            ui.add_space(4.0);
            match Self::draw_folder_inline_editor(ui, prompt, "settings_setlists_folder_text", "Save") {
                FolderRowEvent::Applied(path) => {
                    self.setlist_folder_edit = None;
                    actions.push(SettingsAction::SetSetlistsDir(Some(path)));
                }
                FolderRowEvent::Cancelled => self.setlist_folder_edit = None,
                FolderRowEvent::None => {}
            }
        }
    }

    /// `save_setlist`, `load_setlist`, `list_setlists`, `set_setlist_folder` for Assistant/MCP.
    pub(super) fn setlist_file_tool(
        &mut self,
        name: &str,
        args: &serde_json::Value,
        origin: AgentOrigin,
        now: f32,
    ) -> String {
        let dir = self.effective_setlists_dir().to_path_buf();
        let default_dir = self.setlists_dir_default.clone();
        let folder_meta = |app: &Self, cur: &Path| {
            serde_json::json!({
                "folder": crate::plugin_dirs::display(cur),
                "folder_path": cur.to_string_lossy(),
                "default_folder": crate::plugin_dirs::display(&default_dir),
                "is_custom": app.setlists_dir_is_custom(),
                "open_setlist": app.open_setlist,
            })
        };
        match name {
            "save_setlist" => {
                let Some(title) = args.get("name").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()) else {
                    return "error: save_setlist needs 'name'".into();
                };
                match self.save_setlist_to_dir(&dir, title) {
                    Ok(p) => {
                        self.agent_activity.record(origin, format!("Save setlist → {title}"), vec![], now);
                        let mut v = folder_meta(self, &dir);
                        v["saved"] = serde_json::json!(title);
                        v["path"] = serde_json::json!(p.to_string_lossy());
                        v["songs"] = serde_json::json!(self.setlist.songs.len());
                        v["rigs"] = serde_json::json!(self.rig_count());
                        v.to_string()
                    }
                    Err(e) => format!("error: save_setlist failed: {e}"),
                }
            }
            "load_setlist" => {
                let Some(title) = args.get("name").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()) else {
                    return "error: load_setlist needs 'name'".into();
                };
                match self.load_setlist_from_dir(&dir, title) {
                    Ok(()) => {
                        self.agent_activity.record(origin, format!("Load setlist ← {title}"), vec![], now);
                        let mut v = folder_meta(self, &dir);
                        v["loaded"] = serde_json::json!(self.open_setlist.clone().unwrap_or_else(|| title.to_string()));
                        v["songs"] = serde_json::json!(self.setlist.songs.len());
                        v["rigs"] = serde_json::json!(self.rig_count());
                        v["setlist"] = self.setlist_json();
                        v.to_string()
                    }
                    Err(e) => format!("error: load_setlist failed: {e}"),
                }
            }
            "list_setlists" => {
                let setlists = list_setlists(&dir);
                let mut v = folder_meta(self, &dir);
                v["setlists"] = serde_json::json!(setlists);
                v.to_string()
            }
            "set_setlist_folder" => {
                let raw = match args.get("path") {
                    None | Some(serde_json::Value::Null) => None,
                    Some(v) => match v.as_str() {
                        Some(s) if s.trim().is_empty() => None,
                        Some(s) => match normalize_folder_path(s) {
                            Ok(p) => Some(p),
                            Err(e) => return format!("error: {e}"),
                        },
                        None => return "error: path must be a folder path string or null".into(),
                    },
                };
                match self.set_setlists_dir(raw) {
                    Ok(msg) => {
                        let cur = self.effective_setlists_dir().to_path_buf();
                        let setlists = list_setlists(&cur);
                        self.agent_activity.record(origin, "Setlists folder", vec![], now);
                        let mut v = folder_meta(self, &cur);
                        v["result"] = serde_json::json!(msg);
                        v["setlists"] = serde_json::json!(setlists);
                        v.to_string()
                    }
                    Err(msg) => format!("error: {msg}"),
                }
            }
            _ => format!("error: unknown setlist tool '{name}'"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::settings_and_dropdown_tests::{
        assert_settings_text_layout, click_at, key, open_settings_page, run_frame,
    };
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ksalat-ui-setlists-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn type_text(app: &mut DemoApp, ctx: &egui::Context, size: Vec2, text: &str) {
        run_frame(app, ctx, size, vec![egui::Event::Text(text.into())]);
    }

    fn find_text_center(texts: &[(String, Rect)], label: &str) -> Pos2 {
        texts
            .iter()
            .find(|(t, _)| t == label)
            .unwrap_or_else(|| panic!("text {label:?} not found in {texts:?}"))
            .1
            .center()
    }

    fn warm_frames(app: &mut DemoApp, ctx: &egui::Context, size: Vec2) -> Vec<(String, Rect)> {
        // Rule 212/222: settle 3 frames after ui_scale so Area/ScrollArea coords match clicks.
        let mut texts = Vec::new();
        for _ in 0..3 {
            (texts, _, _) = run_frame(app, ctx, size, vec![]);
        }
        texts
    }

    fn two_rigs() -> (DemoApp, egui::Context) {
        let mut app = DemoApp::new_with_sink(None);
        app.load_multi_rig_demo();
        let ctx = egui::Context::default();
        warm_frames(&mut app, &ctx, Vec2::new(1280.0, 800.0));
        assert_eq!(app.rig_count(), 2);
        (app, ctx)
    }

    #[test]
    fn save_and_load_setlist_round_trip_through_modals_and_folder_change() {
        let alt_dir = scratch("modal-alt");
        let (mut app, _) = two_rigs();
        let _ = app.rename_rig(0, "Stage Piano");
        let _ = app.rename_rig(1, "Brass");
        app.exec_tool("set_setlist_song", &serde_json::json!({
            "name": "Opening Song",
            "bpm": 128.0,
            "parts": r#"[{"name":"Verse","rigs":[{"rig":0}]},{"name":"Chorus","rigs":[{"rig":0},{"rig":1}]}]"#
        }));
        app.stage_view = StageView::Setlist;

        let ctx = egui::Context::default();
        let size = Vec2::new(1280.0, 800.0);
        let texts = warm_frames(&mut app, &ctx, size);

        // Open Save setlist modal via the Setlist stage button.
        click_at(&mut app, &ctx, size, find_text_center(&texts, "Save setlist…"));
        let texts = warm_frames(&mut app, &ctx, size);
        assert!(app.save_setlist_dialog.is_some(), "Save setlist modal opened");
        assert!(texts.iter().any(|(t, _)| t == "Save setlist"), "{texts:?}");

        // Replace text with "Summer Tour" and save with Enter.
        run_frame(&mut app, &ctx, size, vec![egui::Event::Key {
            key: Key::A, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::COMMAND,
        }]);
        type_text(&mut app, &ctx, size, "Summer Tour");
        run_frame(&mut app, &ctx, size, key(Key::Enter));
        let texts = warm_frames(&mut app, &ctx, size);
        assert!(app.save_setlist_dialog.is_none(), "modal closed after save");
        assert_eq!(app.open_setlist.as_deref(), Some("Summer Tour"));
        assert!(texts.iter().any(|(t, _)| t == "Saved · Summer Tour"), "{texts:?}");
        assert_eq!(list_setlists(app.effective_setlists_dir()), vec!["Summer Tour".to_string()]);

        // Esc closes Load modal cleanly without navigating away from Setlist.
        click_at(&mut app, &ctx, size, find_text_center(&texts, "Load setlist…"));
        let texts = warm_frames(&mut app, &ctx, size);
        assert!(app.load_setlist_dialog.is_some());
        assert!(texts.iter().any(|(t, _)| t == "Summer Tour"), "{texts:?}");
        run_frame(&mut app, &ctx, size, key(Key::Escape));
        run_frame(&mut app, &ctx, size, vec![]);
        assert!(app.load_setlist_dialog.is_none(), "Esc closes load modal (rule 213)");
        assert_eq!(app.stage_view, StageView::Setlist, "Esc stays in Setlist");

        // Switch folder inside Load modal -> empty there, then Reset back to original folder.
        click_at(&mut app, &ctx, size, find_text_center(&texts, "Load setlist…"));
        let texts = warm_frames(&mut app, &ctx, size);
        click_at(&mut app, &ctx, size, find_text_center(&texts, "Change…"));
        run_frame(&mut app, &ctx, size, vec![]);
        run_frame(&mut app, &ctx, size, vec![egui::Event::Key {
            key: Key::A, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::COMMAND,
        }]);
        type_text(&mut app, &ctx, size, alt_dir.to_str().unwrap());
        run_frame(&mut app, &ctx, size, key(Key::Enter));
        let texts = warm_frames(&mut app, &ctx, size);
        assert_eq!(app.effective_setlists_dir(), alt_dir.as_path());
        let joined: String = texts.iter().map(|(t, _)| t.as_str()).collect();
        assert!(joined.contains("SAVED SETLISTS (0)"), "{joined}");
        assert!(texts.iter().any(|(t, _)| t.starts_with("No saved setlists in this folder")), "{texts:?}");

        click_at(&mut app, &ctx, size, find_text_center(&texts, "Reset"));
        let texts = warm_frames(&mut app, &ctx, size);
        assert!(!app.setlists_dir_is_custom());
        let joined: String = texts.iter().map(|(t, _)| t.as_str()).collect();
        assert!(joined.contains("SAVED SETLISTS (1)"), "{joined}");
        assert!(texts.iter().any(|(t, _)| t == "Summer Tour"), "{texts:?}");
        run_frame(&mut app, &ctx, size, key(Key::Escape));
        run_frame(&mut app, &ctx, size, vec![]);

        // Modify rack and setlist, then reload "Summer Tour" through the Load modal; undo brings back modified state.
        app.exec_tool("set_setlist_song", &serde_json::json!({"name": "Temporary Song"}));
        assert_eq!(app.setlist.songs.len(), 2);
        let texts = warm_frames(&mut app, &ctx, size);
        click_at(&mut app, &ctx, size, find_text_center(&texts, "Load setlist…"));
        let texts = warm_frames(&mut app, &ctx, size);
        click_at(&mut app, &ctx, size, find_text_center(&texts, "Load"));
        let _ = warm_frames(&mut app, &ctx, size);
        assert!(app.load_setlist_dialog.is_none());
        assert_eq!(app.setlist.songs.len(), 1);
        assert_eq!(app.setlist.songs[0].name, "Opening Song");
        assert_eq!(app.rig_name(0), "Stage Piano");
        assert_eq!(app.rig_name(1), "Brass");

        // Ctrl+Z undoes loading the saved setlist.
        run_frame(&mut app, &ctx, size, vec![egui::Event::Key {
            key: Key::Z, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::COMMAND,
        }]);
        assert_eq!(app.setlist.songs.len(), 2);

        // When rack is locked: Save still works, Load is refused.
        app.set_rack_locked(true);
        let dir = app.effective_setlists_dir().to_path_buf();
        assert!(app.save_setlist_to_dir(&dir, "Locked Copy").is_ok());
        let texts = warm_frames(&mut app, &ctx, size);
        click_at(&mut app, &ctx, size, find_text_center(&texts, "Load setlist…"));
        let texts = warm_frames(&mut app, &ctx, size);
        click_at(&mut app, &ctx, size, find_text_center(&texts, "Load"));
        let _ = warm_frames(&mut app, &ctx, size);
        assert!(app.load_setlist_dialog.is_some(), "locked rack blocks loading setlist");

        let _ = std::fs::remove_dir_all(&app.setlists_dir_default);
        let _ = std::fs::remove_dir_all(&alt_dir);
    }

    #[test]
    fn settings_session_setlist_folder_edit_and_layout() {
        let custom = scratch("settings-folder");
        for size in [
            Vec2::new(781.0, 950.0),
            Vec2::new(760.0, 520.0),
            Vec2::new(781.0, 468.0),
            Vec2::new(1280.0, 900.0),
        ] {
            let mut app = DemoApp::new_with_sink(None);
            app.stage_view = StageView::Settings;
            let ctx = egui::Context::default();
            let (texts, _, _) = open_settings_page(&mut app, &ctx, size, "Session");
            assert!(texts.iter().any(|(t, _)| t == "Setlists folder"), "{size:?}: {texts:?}");
            assert_settings_text_layout(&app, &texts, size);

            // Open Change editor, test relative path refusal, then valid path and Reset.
            click_at(&mut app, &ctx, size, find_text_center(&texts, "Change"));
            let _ = run_frame(&mut app, &ctx, size, vec![]);
            run_frame(&mut app, &ctx, size, vec![egui::Event::Key {
                key: Key::A, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::COMMAND,
            }]);
            type_text(&mut app, &ctx, size, "relative/setlists");
            let (texts, _, _) = run_frame(&mut app, &ctx, size, vec![]);
            click_at(&mut app, &ctx, size, find_text_center(&texts, "Save"));
            let (texts, _, _) = run_frame(&mut app, &ctx, size, vec![]);
            assert!(texts.iter().any(|(t, _)| t == "Use a full path that starts with / or ~."), "{size:?}: {texts:?}");
            assert_settings_text_layout(&app, &texts, size);
            assert!(!app.setlists_dir_is_custom());

            run_frame(&mut app, &ctx, size, vec![egui::Event::Key {
                key: Key::A, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::COMMAND,
            }]);
            type_text(&mut app, &ctx, size, &format!("{}/", custom.display()));
            run_frame(&mut app, &ctx, size, key(Key::Enter));
            let (texts, _, _) = run_frame(&mut app, &ctx, size, vec![]);
            assert_eq!(app.setlists_dir_custom.as_deref(), Some(custom.as_path()));
            assert!(texts.iter().any(|(t, _)| t == "Reset"), "{size:?}: Reset button shown");
            assert_settings_text_layout(&app, &texts, size);

            click_at(&mut app, &ctx, size, find_text_center(&texts, "Reset"));
            let (texts, _, _) = run_frame(&mut app, &ctx, size, vec![]);
            assert!(!app.setlists_dir_is_custom());
            assert_settings_text_layout(&app, &texts, size);
        }
        let _ = std::fs::remove_dir_all(&custom);
    }

    #[test]
    fn library_rail_and_popover_setlists_tab_flow() {
        let mut app = DemoApp::new_with_sink(None);
        app.exec_tool("set_setlist_song", &serde_json::json!({"name": "Club Opener"}));
        let dir = app.effective_setlists_dir().to_path_buf();
        app.save_setlist_to_dir(&dir, "Club Show").unwrap();

        let ctx = egui::Context::default();
        let size = Vec2::new(1280.0, 800.0);

        // Left Rail → Library → SETLISTS lists "Club Show" and fits inside nav_panel_rect.
        app.nav_open = true;
        app.rail_tab = RailTab::Library;
        app.library_tab = LibraryTab::Setlists;
        let texts = warm_frames(&mut app, &ctx, size);
        assert!(texts.iter().any(|(t, _)| t == "Club Show"), "rail lists saved setlist: {texts:?}");
        for (t, r) in &texts {
            if r.intersects(app.nav_panel_rect.shrink(1.0)) && r.right() > app.nav_panel_rect.right() + 0.5 {
                panic!("rail text {t:?} {r:?} overflows {:?}", app.nav_panel_rect);
            }
        }

        // Top-bar Session popover → SETLISTS tab lists "Club Show".
        app.nav_open = false;
        app.session_popover_open = true;
        app.session_popover_tab = LibraryTab::Setlists;
        let texts = warm_frames(&mut app, &ctx, size);
        assert!(texts.iter().any(|(t, _)| t == "Club Show"), "popover lists saved setlist: {texts:?}");

        let _ = std::fs::remove_dir_all(&app.setlists_dir_default);
    }

    #[test]
    fn setlist_file_tools_parity_and_exec() {
        let tools = crate::assistant::tools();
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["function"]["name"].as_str())
            .collect();
        for t in ["save_setlist", "load_setlist", "list_setlists", "set_setlist_folder"] {
            assert!(names.contains(&t), "assistant::tools() missing {t}");
            assert!(UI_BRIDGE_TOOLS.contains(&t), "UI_BRIDGE_TOOLS missing {t}");
        }
        assert!(tool_is_read_only("list_setlists"));
        assert!(!tool_is_read_only("save_setlist") && tool_changes_only_system("save_setlist"));
        assert!(!tool_is_read_only("set_setlist_folder") && tool_changes_only_system("set_setlist_folder"));
        assert!(!tool_is_read_only("load_setlist") && !tool_changes_only_system("load_setlist"));

        let custom = scratch("tool-folder");
        let (mut app, _) = two_rigs();
        app.exec_tool("set_setlist_song", &serde_json::json!({"name": "Act One"}));

        let set_dir: serde_json::Value = serde_json::from_str(
            &app.exec_tool("set_setlist_folder", &serde_json::json!({"path": custom.to_str().unwrap()})),
        )
        .unwrap();
        assert_eq!(set_dir["is_custom"], true);
        assert_eq!(set_dir["folder_path"], custom.to_str().unwrap());

        let saved: serde_json::Value = serde_json::from_str(
            &app.exec_tool("save_setlist", &serde_json::json!({"name": "Festival Gig"})),
        )
        .unwrap();
        assert_eq!(saved["saved"], "Festival Gig");
        assert_eq!(saved["songs"], 1);
        assert_eq!(saved["rigs"], 2);

        let listed: serde_json::Value = serde_json::from_str(
            &app.exec_tool("list_setlists", &serde_json::json!({})),
        )
        .unwrap();
        assert_eq!(listed["setlists"], serde_json::json!(["Festival Gig"]));
        assert_eq!(listed["open_setlist"], "Festival Gig");

        app.exec_tool("set_setlist_song", &serde_json::json!({"name": "Extra"}));
        assert_eq!(app.setlist.songs.len(), 2);

        // Locked rack allows save/folder/list and refuses load_setlist.
        app.set_rack_locked(true);
        let refused = app.exec_tool("load_setlist", &serde_json::json!({"name": "Festival Gig"}));
        assert!(refused.contains("locked"), "{refused}");
        assert!(app.exec_tool("list_setlists", &serde_json::json!({})).contains("Festival Gig"));
        app.set_rack_locked(false);

        let loaded: serde_json::Value = serde_json::from_str(
            &app.exec_tool("load_setlist", &serde_json::json!({"name": "Festival Gig"})),
        )
        .unwrap();
        assert_eq!(loaded["loaded"], "Festival Gig");
        assert_eq!(app.setlist.songs.len(), 1);

        let reset: serde_json::Value = serde_json::from_str(
            &app.exec_tool("set_setlist_folder", &serde_json::json!({"path": serde_json::Value::Null})),
        )
        .unwrap();
        assert_eq!(reset["is_custom"], false);
        assert_eq!(reset["setlists"], serde_json::json!([]));

        let _ = std::fs::remove_dir_all(&custom);
        let _ = std::fs::remove_dir_all(&app.setlists_dir_default);
    }

    #[test]
    #[ignore = "writes PNGs: KABELSALAT_SHOTS=<dir> cargo test --bin kabelsalat setlist_save_shots -- --ignored"]
    fn setlist_save_shots() {
        let dir = PathBuf::from(std::env::var("KABELSALAT_SHOTS").expect("KABELSALAT_SHOTS=<dir>"));
        std::fs::create_dir_all(&dir).unwrap();

        let make_app = || {
            let (mut app, _) = two_rigs();
            let _ = app.rename_rig(0, "Piano");
            let _ = app.rename_rig(1, "Warm Pad");
            let _ = app.exec_tool("set_setlist_song", &serde_json::json!({
                "name": "Opening Night",
                "bpm": 118.0,
                "time_signature": {"numerator": 4, "denominator": 4},
                "parts": [
                    {"name": "Verse", "rigs": [{"rig": 0, "key_lo": 36, "key_hi": 96, "level": 0.9}]},
                    {"name": "Chorus", "rigs": [{"rig": 0}, {"rig": 1, "key_lo": 60, "key_hi": 96}]}
                ]
            }));
            let _ = app.exec_tool("set_setlist_song", &serde_json::json!({
                "name": "Midnight Waltz",
                "bpm": 76.0,
                "time_signature": {"numerator": 3, "denominator": 4},
                "parts": [{"name": "Theme", "rigs": [{"rig": 0}, {"rig": 1}]}]
            }));
            let _ = app.exec_tool("select_setlist_part", &serde_json::json!({"song": 0, "part": 1}));
            app
        };

        // 1. Setlist view with file bar (1280x800 and 781x950) — fresh app per shot (rule 148).
        for (w, h, fname) in [
            (1280.0, 800.0, "setlist-filebar-1280x800.png"),
            (781.0, 950.0, "setlist-filebar-781x950.png"),
        ] {
            let mut app = make_app();
            let sdir = app.effective_setlists_dir().to_path_buf();
            app.save_setlist_to_dir(&sdir, "Autumn Tour 2026").unwrap();
            app.stage_view = StageView::Setlist;
            super::super::offline_shots::shot(&mut app, Vec2::new(w, h), &dir.join(fname));
            let _ = std::fs::remove_dir_all(&sdir);
        }

        // 2. Save setlist modal.
        {
            let mut app = make_app();
            let sdir = app.effective_setlists_dir().to_path_buf();
            app.save_setlist_to_dir(&sdir, "Autumn Tour 2026").unwrap();
            app.stage_view = StageView::Setlist;
            app.open_save_setlist_dialog();
            super::super::offline_shots::shot(&mut app, Vec2::new(1280.0, 800.0), &dir.join("setlist-save-modal.png"));
            let _ = std::fs::remove_dir_all(&sdir);
        }

        // 3. Load setlist modal with saved items.
        {
            let mut app = make_app();
            let sdir = app.effective_setlists_dir().to_path_buf();
            app.save_setlist_to_dir(&sdir, "Jazz Trio Club").unwrap();
            app.save_setlist_to_dir(&sdir, "Autumn Tour 2026").unwrap();
            app.stage_view = StageView::Setlist;
            app.open_load_setlist_dialog();
            super::super::offline_shots::shot(&mut app, Vec2::new(1280.0, 800.0), &dir.join("setlist-load-modal.png"));
            let _ = std::fs::remove_dir_all(&sdir);
        }

        // 4. Settings → Session (1280x800 and 781x950).
        for (w, h, fname) in [
            (1280.0, 800.0, "settings-session-setlists-1280x800.png"),
            (781.0, 950.0, "settings-session-setlists-781x950.png"),
        ] {
            let mut app = make_app();
            app.stage_view = StageView::Settings;
            app.settings_page = SettingsPage::Session;
            super::super::offline_shots::shot(&mut app, Vec2::new(w, h), &dir.join(fname));
        }

        // 5. Left Rail Library → Setlists.
        {
            let mut app = make_app();
            let sdir = app.effective_setlists_dir().to_path_buf();
            app.save_setlist_to_dir(&sdir, "Jazz Trio Club").unwrap();
            app.save_setlist_to_dir(&sdir, "Autumn Tour 2026").unwrap();
            app.nav_open = true;
            app.rail_tab = RailTab::Library;
            app.library_tab = LibraryTab::Setlists;
            super::super::offline_shots::shot(&mut app, Vec2::new(1280.0, 800.0), &dir.join("rail-library-setlists.png"));
            let _ = std::fs::remove_dir_all(&sdir);
        }

        // 6. Top-bar Session popover → Setlists.
        {
            let mut app = make_app();
            let sdir = app.effective_setlists_dir().to_path_buf();
            app.save_setlist_to_dir(&sdir, "Jazz Trio Club").unwrap();
            app.save_setlist_to_dir(&sdir, "Autumn Tour 2026").unwrap();
            app.session_popover_open = true;
            app.session_popover_tab = LibraryTab::Setlists;
            super::super::offline_shots::shot(&mut app, Vec2::new(1280.0, 800.0), &dir.join("popover-setlists.png"));
            let _ = std::fs::remove_dir_all(&sdir);
        }
    }
}
