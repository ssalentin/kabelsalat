//! Vital patches for Vitalium, the DISTRHO port of Vital: where they live,
//! how one is checked before the plugin sees it, and how a `.vitalbank`
//! gets in. The patch itself is the plugin's state (`Lv2Plugin::restore_state`).

use crate::sounds::{Load, Sound};
use std::path::{Path, PathBuf};

pub const URI: &str = "urn:distrho:vitalium";
pub const PATCH_EXTENSION: &str = "vital";
pub const BANK_EXTENSION: &str = "vitalbank";

/// Vitalium's data folder (`LoadSave::getDataDirectory` on Linux).
pub fn data_dir() -> PathBuf {
    match std::env::var("XDG_DATA_HOME") {
        Ok(xdg) if !xdg.trim().is_empty() => PathBuf::from(xdg).join("Vitalium"),
        _ => crate::catalog::home_dir().join(".local/share/vitalium"),
    }
}

/// One sound per `.vital` file in the folders Vitalium reads presets from:
/// `<data>/Presets` and `<data>/<bank>/Presets`, searched to any depth.
/// The category is the bank folder plus any folders inside `Presets`.
pub fn patches(data: &Path) -> Vec<Sound> {
    fn walk(dir: &Path, category: &str, out: &mut Vec<Sound>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for path in entries.flatten().map(|e| e.path()) {
            if path.is_dir() {
                let sub = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                walk(&path, &if category.is_empty() { sub } else { format!("{category}/{sub}") }, out);
            } else if path.extension().is_some_and(|x| x.eq_ignore_ascii_case(PATCH_EXTENSION)) {
                let name = path.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                out.push(Sound { category: category.to_string(), name, preferred: false, load: Load::VitalPatch(path) });
            }
        }
    }
    let mut sounds = Vec::new();
    walk(&data.join("Presets"), "", &mut sounds);
    for bank in std::fs::read_dir(data).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
        let name = bank.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        walk(&bank.join("Presets"), &name, &mut sounds);
    }
    sounds.sort_by_key(|s| (s.category.to_lowercase(), s.name.to_lowercase()));
    sounds
}

/// Is this a patch Vitalium can load? A patch it cannot parse makes it open
/// a modal alert box, which waits for a message loop no host here runs: the
/// thread that restored it hangs for good. So every part `jsonToState`
/// reads must be there with the type it reads it as (`LoadSave::load*` in
/// Vitalium's load_save.cpp).
pub fn check_patch(data: &[u8]) -> Result<(), String> {
    use serde_json::Value;
    let text = std::str::from_utf8(data).map_err(|_| "not a Vital patch (not text)".to_string())?;
    let json: Value = serde_json::from_str(text.trim_end_matches('\0')).map_err(|e| format!("not a Vital patch: {e}"))?;
    json.get("synth_version").and_then(Value::as_str).ok_or("not a Vital patch (no synth_version)")?;
    for text_field in ["preset_name", "author", "comments"] {
        if json.get(text_field).is_some_and(|v| !v.is_string()) {
            return Err(format!("broken Vital patch ({text_field} is not text)"));
        }
    }
    let settings = json.get("settings").and_then(Value::as_object).ok_or("not a Vital patch (no settings)")?;
    let part = |name: &str, ok: fn(&Value) -> bool| match settings.get(name) {
        None => Err(format!("incomplete Vital patch (no {name})")),
        Some(v) if !ok(v) => Err(format!("broken Vital patch ({name})")),
        Some(_) => Ok(()),
    };
    part("sample", Value::is_object)?;
    part("wavetables", Value::is_array)?;
    part("lfos", Value::is_array)?;
    part("modulations", |v| v.as_array().is_some_and(|mods| mods.iter().all(|m| {
        m.get("source").is_some_and(Value::is_string) && m.get("destination").is_some_and(Value::is_string)
    })))
}

/// Put a `.vitalbank` (a zip of `<bank>/Presets/…` and friends) or a single
/// `.vital` patch where Vitalium finds it. Returns what was added.
pub fn import(file: &Path, data: &Path) -> Result<String, String> {
    let ext = file.extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if ext == PATCH_EXTENSION {
        let bytes = std::fs::read(file).map_err(|e| format!("{name}: {e}"))?;
        check_patch(&bytes).map_err(|e| format!("{name}: {e}"))?;
        let dir = data.join("User").join("Presets");
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        std::fs::write(dir.join(&name), bytes).map_err(|e| format!("{name}: {e}"))?;
        return Ok(format!("added the Vital patch {}", file.file_stem().unwrap_or_default().to_string_lossy()));
    }
    let zip_ok = ext == BANK_EXTENSION || (ext == "zip" && name.to_lowercase().ends_with(".vitalbank.zip"));
    if !zip_ok {
        return Err(format!("{name} is not a Vital bank or patch"));
    }
    let reader = std::fs::File::open(file).map_err(|e| format!("{name}: {e}"))?;
    let mut zip = zip::ZipArchive::new(reader).map_err(|e| format!("{name}: {e}"))?;
    let mut patches = 0;
    let mut banks: Vec<String> = Vec::new();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| format!("{name}: {e}"))?;
        // `enclosed_name` refuses absolute paths and `..` (zip slip).
        let Some(rel) = entry.enclosed_name() else { continue };
        let out = data.join(&rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let mut f = std::fs::File::create(&out).map_err(|e| format!("{}: {e}", out.display()))?;
        std::io::copy(&mut entry, &mut f).map_err(|e| format!("{}: {e}", out.display()))?;
        if rel.extension().is_some_and(|x| x.eq_ignore_ascii_case(PATCH_EXTENSION)) {
            patches += 1;
            if let Some(bank) = rel.components().next().map(|c| c.as_os_str().to_string_lossy().into_owned()) {
                if !banks.contains(&bank) { banks.push(bank); }
            }
        }
    }
    if patches == 0 {
        return Err(format!("{name} holds no Vital patches"));
    }
    Ok(format!("added {patches} Vital patches ({})", banks.join(", ")))
}

/// `vital-test <patch.vital> [out.wav]`: Vitalium plays the patch, restored
/// as its state, once without breath and once with a breath swell
/// (aftertouch 0 → 127 → 0), and says how loud each was. A wind patch is
/// nearly silent without breath. The swell is written to `out.wav`.
pub fn run_test(args: &[PathBuf]) -> anyhow::Result<()> {
    let patch = args.first().ok_or_else(|| anyhow::anyhow!("usage: kabelsalat vital-test <patch.vital> [out.wav]"))?;
    let state = std::fs::read(patch)?;
    check_patch(&state).map_err(|e| anyhow::anyhow!(e))?;
    let format = crate::lv2::state_format(URI).expect("Vitalium has a state format");
    let (sr, block) = (48000.0, 256usize);
    let mut plugin = crate::lv2::Lv2Plugin::load(URI, sr, block)?;
    let started = std::time::Instant::now();
    plugin.restore_state(&[(format.key, format.kind, &state)])?;
    let restore_ms = started.elapsed().as_millis();
    let input = vec![0.0f32; block * 2];
    let mut output = vec![0.0f32; block * 2];
    let blocks = (2.0 * sr) as usize / block;
    let mut play = |plugin: &mut crate::lv2::Lv2Plugin, breath: &dyn Fn(f32) -> u8| -> Vec<f32> {
        let mut out = Vec::with_capacity(blocks * block * 2);
        plugin.note_on(60, 100);
        for b in 0..blocks {
            plugin.channel_pressure(breath(b as f32 / blocks as f32));
            plugin.process(&input, &mut output, block);
            out.extend_from_slice(&output);
        }
        plugin.channel_pressure(0);
        plugin.note_off(60);
        for _ in 0..40 { plugin.process(&input, &mut output, block); }
        out
    };
    let rms = |s: &[f32]| (s.iter().map(|x| x * x).sum::<f32>() / s.len().max(1) as f32).sqrt();
    let still = play(&mut plugin, &|_| 0);
    let swell = play(&mut plugin, &|t| ((1.0 - (2.0 * t - 1.0).abs()) * 127.0) as u8);
    crate::lv2::retire(plugin);
    let (quiet, breathed) = (rms(&still), rms(&swell));
    println!("{}: restored in {restore_ms} ms · note 60 for 2 s: rms {quiet:.4} without breath, {breathed:.4} with a breath swell", patch.display());
    anyhow::ensure!(breathed > 1e-3, "the patch is silent even with breath");
    if let Some(wav) = args.get(1) {
        let pcm: Vec<u8> = swell.iter().flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes()).collect();
        let mut file = Vec::with_capacity(44 + pcm.len());
        for (tag, n) in [(&b"RIFF"[..], 36 + pcm.len() as u32)] { file.extend_from_slice(tag); file.extend_from_slice(&n.to_le_bytes()); }
        file.extend_from_slice(b"WAVEfmt ");
        for v in [16u32] { file.extend_from_slice(&v.to_le_bytes()); }
        for v in [1u16, 2] { file.extend_from_slice(&v.to_le_bytes()); }
        for v in [sr as u32, sr as u32 * 4] { file.extend_from_slice(&v.to_le_bytes()); }
        for v in [4u16, 16] { file.extend_from_slice(&v.to_le_bytes()); }
        file.extend_from_slice(b"data");
        file.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
        file.extend_from_slice(&pcm);
        std::fs::write(wav, file)?;
        println!("breath swell written to {}", wav.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch(settings: serde_json::Value) -> Vec<u8> {
        serde_json::json!({"synth_version": "1.0.7", "settings": settings}).to_string().into_bytes()
    }

    #[test]
    fn a_patch_vitalium_would_choke_on_is_refused() {
        let full = serde_json::json!({"modulations": [], "sample": {}, "wavetables": [], "lfos": []});
        assert_eq!(check_patch(&patch(full)), Ok(()));
        let partial = serde_json::json!({"osc_1_transpose": 12.0});
        assert!(check_patch(&patch(partial)).unwrap_err().contains("incomplete"));
        let bad_mod = serde_json::json!({"modulations": [{"source": "lfo_1"}], "sample": {}, "wavetables": [], "lfos": []});
        assert_eq!(check_patch(&patch(bad_mod)), Err("broken Vital patch (modulations)".into()));
        let not_list = serde_json::json!({"modulations": [], "sample": {}, "wavetables": "x", "lfos": []});
        assert_eq!(check_patch(&patch(not_list)), Err("broken Vital patch (wavetables)".into()));
        assert!(check_patch(b"{\"settings\": {}}").is_err());
        assert!(check_patch(b"\x00\x01binary").is_err());
    }

    #[test]
    fn an_imported_bank_lists_where_vitalium_reads_presets() {
        let dir = std::env::temp_dir().join(format!("ksalat-vital-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let data = dir.join("data");
        let bank = dir.join("Winds.vitalbank");
        {
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&bank).unwrap());
            let opts = zip::write::SimpleFileOptions::default();
            let body = patch(serde_json::json!({"modulations": [], "sample": {}, "wavetables": [], "lfos": []}));
            for path in ["Winds/Presets/Woody.vital", "Winds/Presets/Lead/Screamy.vital", "Winds/Wavetables/Breath.vitaltable", "../escape.vital"] {
                zip.start_file(path, opts).unwrap();
                std::io::Write::write_all(&mut zip, &body).unwrap();
            }
            zip.finish().unwrap();
        }
        let said = import(&bank, &data).unwrap();
        assert_eq!(said, "added 2 Vital patches (Winds)", "the escaping entry is skipped");
        assert!(!dir.join("escape.vital").exists());
        let single = dir.join("Own Lead.vital");
        std::fs::write(&single, patch(serde_json::json!({"modulations": [], "sample": {}, "wavetables": [], "lfos": []}))).unwrap();
        import(&single, &data).unwrap();
        let labels: Vec<String> = patches(&data).iter().map(Sound::label).collect();
        assert_eq!(labels, ["User/Own Lead", "Winds/Woody", "Winds/Lead/Screamy"]);
        assert!(import(&dir.join("notes.txt"), &data).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
