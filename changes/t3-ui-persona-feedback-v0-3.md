### Added
- **Show mode status pills, next part & F2 shortcut:** Show mode (`F2`) now previews the upcoming part in teal, surfaces top `REC`, `TRACK`, `CLICK`, and crash-recovery pills, scales cleanly down to `760×520`, and restores Show mode after crash recovery.
- **Setlist backing-track picker & part controls:** songs can attach `.wav` tracks via **Pick WAV…** (scanning Sessions/Audio) or drag-and-drop; parts now support **↑**, **↓**, **Dup**, and **Clear knobs**; pre-show failing rows link directly (`Matrix →`, `Mixer →`, `Install →`, `Rack →`, `Settings →`, `Fix track →`); **Ctrl+T** and clickable `TUNE` on Audio In toggle the tuner.
- **Retrospective chip & Plugins clarity:** a dedicated **Retro** save chip sits next to `REC`, **MIDI FX** is a top-level Plugins catalog category alongside clearer **Layer in rig** / **Install** prompts, and Mixer pins `Kabelsalat` + `Master out` when 4+ rigs overflow.

### Changed
- **Safer live shortcuts, undo & delete confirmations:** moved knob reset to **Shift+R** (selected unit or active rig, synced to engine with undo), assigned **C** to Assistant and **F12** cleanly to Setlist, extended **Ctrl+Z** undo to manual cable, template, setlist, rig, and bus edits, and added two-step confirms before deleting songs, parts, aux buses, or rig presets.
- **On-stage UX & contrast polish:** upgraded dark-surface warnings/learn prompts to legible high-contrast amber (`ACCENT_BRIGHT`), showed patch feedback notices inline in non-Rack stage headers, added Enter-to-save and clickable `openrouter.ai/keys` link in the Assistant onboarding bar, and showed hardware device names in **Sets & levels**.

### Fixed
- **Rack-lock & modal Escape gaps:** blocked `F4`–`F8`, Left Rail, Matrix, and Mixer mutations while the rack is locked, closed param/dialog leaks on lock, fixed drums laptop keys mapping to GM C2 (MIDI 36), and kept `Escape` closing open rig modals cleanly while clearing two-step confirmations first.
