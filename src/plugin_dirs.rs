//! Custom plugin search folders (#414).
//!
//! kabelsalat looks for plugins in the standard folders of each format and
//! in `CLAP_PATH` / `VST3_PATH` / `LV2_PATH`. The folders here come on top:
//! the user adds them in Settings → Plugins (or an agent does, with the
//! `*_plugin_folder` tools), they are kept in `config.json` under
//! [`CONFIG_FIELD`], and every scan and every plugin load reads them through
//! [`custom`].
//!
//! The list is one value per process. Each process reads it at start
//! ([`init`]): from [`ENV`] when its parent handed it down (plugin workers),
//! else from the config file. Tests run as threads of one process, so there
//! the value is per thread (rule 183) and never comes from the user's config
//! (rule 37).

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// The field of `config.json`: `{"clap": [...], "vst3": [...], "lv2": [...]}`.
pub const CONFIG_FIELD: &str = "plugin_dirs";
/// A parent process hands its folders to a child in this variable (the same
/// JSON as the config field).
pub const ENV: &str = "KABELSALAT_PLUGIN_DIRS";
/// Folders per format. A search path is short; this bounds a config file
/// gone wrong and the Settings page's height.
pub const MAX_PER_FORMAT: usize = 16;

/// A plugin format with search folders of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Clap,
    Vst3,
    Lv2,
}

impl Format {
    pub const ALL: [Format; 3] = [Format::Clap, Format::Vst3, Format::Lv2];

    /// The name in config, env and tool arguments.
    pub fn key(self) -> &'static str {
        match self {
            Format::Clap => "clap",
            Format::Vst3 => "vst3",
            Format::Lv2 => "lv2",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Format::Clap => "CLAP",
            Format::Vst3 => "VST3",
            Format::Lv2 => "LV2",
        }
    }

    /// The environment variable that adds folders for this format.
    pub fn env_var(self) -> &'static str {
        match self {
            Format::Clap => "CLAP_PATH",
            Format::Vst3 => "VST3_PATH",
            Format::Lv2 => "LV2_PATH",
        }
    }

    /// The folders that are searched without any setting, as users read them.
    pub fn standard(self) -> &'static [&'static str] {
        match self {
            Format::Clap => &["~/.clap", "/usr/lib/clap", "/usr/local/lib/clap"],
            Format::Vst3 => &["~/.vst3", "/usr/lib/vst3", "/usr/local/lib/vst3"],
            Format::Lv2 => &["~/.lv2", "/usr/lib/lv2", "/usr/local/lib/lv2"],
        }
    }

    pub fn parse(s: &str) -> Option<Format> {
        let s = s.trim().to_ascii_lowercase();
        Format::ALL.into_iter().find(|f| f.key() == s)
    }
}

/// The custom search folders of every format, in the order they are searched.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginDirs {
    pub clap: Vec<PathBuf>,
    pub vst3: Vec<PathBuf>,
    pub lv2: Vec<PathBuf>,
}

impl PluginDirs {
    pub const EMPTY: PluginDirs = PluginDirs { clap: Vec::new(), vst3: Vec::new(), lv2: Vec::new() };

    pub fn of(&self, format: Format) -> &Vec<PathBuf> {
        match format {
            Format::Clap => &self.clap,
            Format::Vst3 => &self.vst3,
            Format::Lv2 => &self.lv2,
        }
    }

    fn of_mut(&mut self, format: Format) -> &mut Vec<PathBuf> {
        match format {
            Format::Clap => &mut self.clap,
            Format::Vst3 => &mut self.vst3,
            Format::Lv2 => &mut self.lv2,
        }
    }

    pub fn len(&self) -> usize {
        self.clap.len() + self.vst3.len() + self.lv2.len()
    }

    /// Read the config field. Whatever is not a usable folder is left out:
    /// a hand-edited file never stops the app from starting.
    pub fn from_json(v: &Value) -> PluginDirs {
        let mut dirs = PluginDirs::default();
        for f in Format::ALL {
            let Some(list) = v.get(f.key()).and_then(Value::as_array) else { continue };
            for raw in list.iter().filter_map(Value::as_str) {
                let _ = dirs.add(f, raw);
            }
        }
        dirs
    }

    pub fn to_json(&self) -> Value {
        let list = |f: Format| -> Vec<String> { self.of(f).iter().map(|p| p.to_string_lossy().into_owned()).collect() };
        json!({ "clap": list(Format::Clap), "vst3": list(Format::Vst3), "lv2": list(Format::Lv2) })
    }

    /// Add the folder `raw` names (see [`normalize`]) at the end of the
    /// format's list.
    pub fn add(&mut self, format: Format, raw: &str) -> Result<PathBuf, String> {
        let path = normalize(raw)?;
        let list = self.of_mut(format);
        if list.contains(&path) {
            return Err(format!("{} is already a {} folder", display(&path), format.label()));
        }
        if list.len() >= MAX_PER_FORMAT {
            return Err(format!("at most {MAX_PER_FORMAT} {} folders", format.label()));
        }
        list.push(path.clone());
        Ok(path)
    }

    /// Point entry `index` of the format's list at another folder, keeping
    /// its place.
    pub fn replace(&mut self, format: Format, index: usize, raw: &str) -> Result<PathBuf, String> {
        let path = normalize(raw)?;
        let list = self.of_mut(format);
        if index >= list.len() {
            return Err("that folder is no longer in the list".into());
        }
        if list.iter().enumerate().any(|(i, p)| i != index && *p == path) {
            return Err(format!("{} is already a {} folder", display(&path), format.label()));
        }
        list[index] = path.clone();
        Ok(path)
    }

    /// Take the folder `raw` names off the format's list. False: not on it.
    pub fn remove(&mut self, format: Format, raw: &str) -> bool {
        let Ok(path) = normalize(raw) else { return false };
        let list = self.of_mut(format);
        let before = list.len();
        list.retain(|p| *p != path);
        list.len() != before
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

/// The folder a user typed, as it is stored: trimmed, without quotes around
/// it, `~` expanded, no trailing slash. It must be absolute (a relative path
/// would mean another folder for every working directory) and hold no `:`
/// (search paths travel between processes colon-separated). It need not
/// exist: a folder on a drive that is not plugged in stays on the list.
pub fn normalize(raw: &str) -> Result<PathBuf, String> {
    let mut s = raw.trim();
    for q in ['"', '\''] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            s = s[1..s.len() - 1].trim();
        }
    }
    if s.is_empty() {
        return Err("enter a folder path".into());
    }
    if s.contains(':') {
        return Err("a folder path cannot contain ':'".into());
    }
    if s.contains('\0') || s.contains('\n') {
        return Err("that is not a folder path".into());
    }
    let expanded = if s == "~" {
        home().ok_or("no home folder to expand ~")?
    } else if let Some(rest) = s.strip_prefix("~/") {
        home().ok_or("no home folder to expand ~")?.join(rest)
    } else {
        PathBuf::from(s)
    };
    if !expanded.is_absolute() {
        return Err("use a full path that starts with / or ~".into());
    }
    // Rebuilding from components drops trailing and doubled slashes.
    let clean: PathBuf = expanded.components().collect();
    Ok(clean)
}

/// A path as the UI and the tools show it: the home folder as `~`.
pub fn display(path: &Path) -> String {
    if let Some(home) = home() {
        if let Ok(rest) = path.strip_prefix(&home) {
            return if rest.as_os_str().is_empty() { "~".into() } else { format!("~/{}", rest.to_string_lossy()) };
        }
    }
    path.to_string_lossy().into_owned()
}

#[cfg(not(test))]
static CUSTOM: std::sync::RwLock<PluginDirs> = std::sync::RwLock::new(PluginDirs::EMPTY);

#[cfg(test)]
thread_local! {
    /// Per test thread (rule 183): one test's folders never show up in the
    /// scan of another.
    static CUSTOM: std::cell::RefCell<PluginDirs> = const { std::cell::RefCell::new(PluginDirs::EMPTY) };
}

/// The custom folders this process searches.
pub fn current() -> PluginDirs {
    #[cfg(not(test))]
    {
        CUSTOM.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
    #[cfg(test)]
    {
        CUSTOM.with(|c| c.borrow().clone())
    }
}

/// Replace the custom folders of this process. Scans and loads that start
/// after this see them; persisting is [`save`].
pub fn set(dirs: PluginDirs) {
    #[cfg(not(test))]
    {
        *CUSTOM.write().unwrap_or_else(|e| e.into_inner()) = dirs;
    }
    #[cfg(test)]
    {
        CUSTOM.with(|c| *c.borrow_mut() = dirs);
    }
}

/// The custom folders of one format, in search order.
pub fn custom(format: Format) -> Vec<PathBuf> {
    #[cfg(not(test))]
    {
        CUSTOM.read().unwrap_or_else(|e| e.into_inner()).of(format).clone()
    }
    #[cfg(test)]
    {
        CUSTOM.with(|c| c.borrow().of(format).clone())
    }
}

/// Read this process's folders once, at start: what the parent handed down,
/// else the config file.
pub fn init() {
    let from_env = std::env::var(ENV).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok());
    let v = from_env.or_else(|| crate::assistant::load_config_value(CONFIG_FIELD));
    if let Some(v) = v {
        set(PluginDirs::from_json(&v));
    }
}

/// Write the folders to `config.json` (a no-op in tests, rule 37).
pub fn save(dirs: &PluginDirs) -> std::io::Result<()> {
    crate::assistant::save_config_value(CONFIG_FIELD, dirs.to_json())
}

/// What a child process gets in [`ENV`] to search the same folders.
pub fn env_value() -> String {
    current().to_json().to_string()
}

/// The LV2 bundles inside the custom LV2 folders: every subfolder with a
/// `manifest.ttl`, or the folder itself when it is a bundle. lilv reads its
/// search path from `LV2_PATH` alone, so the host loads these one by one.
/// `standard` are the folders lilv searches already; a custom folder among
/// them adds nothing (and loading a bundle twice doubles its manifest).
pub fn lv2_bundles(standard: &[PathBuf]) -> Vec<PathBuf> {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let standard: Vec<PathBuf> = standard.iter().map(|p| canon(p)).collect();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for dir in custom(Format::Lv2) {
        let dir_c = canon(&dir);
        if standard.contains(&dir_c) {
            continue;
        }
        let mut found = Vec::new();
        if dir.join("manifest.ttl").is_file() {
            found.push(dir.clone());
        } else if let Ok(entries) = std::fs::read_dir(&dir) {
            found.extend(entries.flatten().map(|e| e.path()).filter(|p| p.join("manifest.ttl").is_file()));
            found.sort();
        }
        for bundle in found {
            let c = canon(&bundle);
            // A bundle that sits in a standard folder (a symlinked folder,
            // a custom folder inside one) is lilv's already.
            if c.parent().is_some_and(|parent| standard.iter().any(|s| s == parent)) {
                continue;
            }
            if seen.insert(c) {
                out.push(bundle);
            }
        }
    }
    out
}

/// The tools that change the folders, for `assistant::tools()`, the app's
/// `exec_tool` and headless MCP alike (rule 6).
pub const EDIT_TOOLS: [&str; 2] = ["add_plugin_folder", "remove_plugin_folder"];
pub const LIST_TOOL: &str = "list_plugin_folders";

/// Apply `add_plugin_folder` / `remove_plugin_folder` to `dirs`. Ok: what
/// was done, as a sentence start ("Added CLAP folder ~/x"). The caller makes
/// the result this process's folders, saves it and rescans.
pub fn tool_edit(dirs: &mut PluginDirs, tool: &str, args: &Value) -> Result<String, String> {
    let format = args
        .get("format")
        .and_then(Value::as_str)
        .and_then(Format::parse)
        .ok_or_else(|| format!("{tool} needs 'format': clap, vst3 or lv2"))?;
    let raw = args
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{tool} needs 'path': the folder, absolute or starting with ~"))?;
    match tool {
        "add_plugin_folder" => {
            let path = dirs.add(format, raw).map_err(|e| format!("{tool}: {e}"))?;
            Ok(format!("Added {} folder {}", format.label(), display(&path)))
        }
        "remove_plugin_folder" => {
            if !dirs.remove(format, raw) {
                let have: Vec<String> = dirs.of(format).iter().map(|p| display(p)).collect();
                return Err(format!(
                    "{tool}: {raw} is not a custom {} folder (custom: {}); standard folders and ${} cannot be removed here",
                    format.label(),
                    if have.is_empty() { "none".to_string() } else { have.join(", ") },
                    format.env_var(),
                ));
            }
            Ok(format!("Removed {} folder {}", format.label(), display(&normalize(raw).unwrap_or_else(|_| PathBuf::from(raw)))))
        }
        other => Err(format!("{other} is not a plugin folder tool")),
    }
}

/// What the tools answer: per format, the custom folders (with whether each
/// exists, as far as `exists` knows), the folders named by the environment
/// variable, and the standard ones.
pub fn describe(dirs: &PluginDirs, exists: impl Fn(&Path) -> Option<bool>) -> Value {
    let mut out = serde_json::Map::new();
    for f in Format::ALL {
        let custom: Vec<Value> = dirs
            .of(f)
            .iter()
            .map(|p| json!({ "path": p.to_string_lossy(), "exists": exists(p) }))
            .collect();
        let env: Vec<String> = std::env::var(f.env_var())
            .map(|v| v.split(':').filter(|s| !s.is_empty()).map(str::to_string).collect())
            .unwrap_or_default();
        out.insert(
            f.key().into(),
            json!({ "custom": custom, "env_var": f.env_var(), "env": env, "standard": f.standard() }),
        );
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_typed_folder_is_stored_absolute_expanded_and_without_a_trailing_slash() {
        let home = home().expect("HOME");
        assert_eq!(normalize("  /opt/plugins/clap/ ").unwrap(), PathBuf::from("/opt/plugins/clap"));
        assert_eq!(normalize("\"/opt/my plugins\"").unwrap(), PathBuf::from("/opt/my plugins"));
        assert_eq!(normalize("~/studio/vst3").unwrap(), home.join("studio/vst3"));
        assert_eq!(normalize("~").unwrap(), home);
        assert_eq!(normalize("/opt//plugins///lv2").unwrap(), PathBuf::from("/opt/plugins/lv2"));
        for bad in ["", "   ", "plugins/clap", "./clap", "/a:/b", "''"] {
            assert!(normalize(bad).is_err(), "{bad:?} is refused");
        }
        assert_eq!(display(&home.join("studio/vst3")), "~/studio/vst3");
        assert_eq!(display(Path::new("/opt/plugins")), "/opt/plugins");
    }

    #[test]
    fn folders_are_added_edited_and_removed_per_format_without_duplicates() {
        let mut d = PluginDirs::default();
        d.add(Format::Clap, "/opt/a").unwrap();
        d.add(Format::Clap, "/opt/b/").unwrap();
        d.add(Format::Lv2, "/opt/a").unwrap();
        assert!(d.add(Format::Clap, "/opt/a/").unwrap_err().contains("already"), "the same folder twice");
        assert_eq!(d.clap, [PathBuf::from("/opt/a"), PathBuf::from("/opt/b")]);
        assert_eq!(d.lv2, [PathBuf::from("/opt/a")], "formats keep lists of their own");

        assert!(d.replace(Format::Clap, 0, "/opt/b").unwrap_err().contains("already"));
        d.replace(Format::Clap, 0, "/opt/c").unwrap();
        d.replace(Format::Clap, 1, "/opt/b").expect("an edit that keeps the folder is no duplicate");
        assert_eq!(d.clap, [PathBuf::from("/opt/c"), PathBuf::from("/opt/b")], "an edit keeps the place");
        assert!(d.replace(Format::Vst3, 0, "/opt/x").is_err());

        assert!(d.remove(Format::Clap, "/opt/c/"));
        assert!(!d.remove(Format::Clap, "/opt/c"));
        assert_eq!(d.clap, [PathBuf::from("/opt/b")]);

        let mut full = PluginDirs::default();
        for i in 0..MAX_PER_FORMAT {
            full.add(Format::Vst3, &format!("/opt/{i}")).unwrap();
        }
        assert!(full.add(Format::Vst3, "/opt/more").unwrap_err().contains("at most"));
    }

    #[test]
    fn the_folder_tools_add_remove_and_say_what_is_wrong() {
        let mut d = PluginDirs::default();
        let msg = tool_edit(&mut d, "add_plugin_folder", &json!({"format": "LV2", "path": "/opt/lv2/"})).unwrap();
        assert_eq!(msg, "Added LV2 folder /opt/lv2");
        assert_eq!(d.lv2, [PathBuf::from("/opt/lv2")]);
        assert!(tool_edit(&mut d, "add_plugin_folder", &json!({"format": "vst2", "path": "/x"})).unwrap_err().contains("clap, vst3 or lv2"));
        assert!(tool_edit(&mut d, "add_plugin_folder", &json!({"format": "clap"})).unwrap_err().contains("'path'"));
        assert!(tool_edit(&mut d, "add_plugin_folder", &json!({"format": "clap", "path": "rel"})).unwrap_err().contains("full path"));
        let err = tool_edit(&mut d, "remove_plugin_folder", &json!({"format": "lv2", "path": "/usr/lib/lv2"})).unwrap_err();
        assert!(err.contains("not a custom LV2 folder") && err.contains("/opt/lv2") && err.contains("$LV2_PATH"), "{err}");
        assert_eq!(tool_edit(&mut d, "remove_plugin_folder", &json!({"format": "lv2", "path": "/opt/lv2"})).unwrap(), "Removed LV2 folder /opt/lv2");
        assert!(d.lv2.is_empty());

        d.add(Format::Clap, "/opt/clap").unwrap();
        let v = describe(&d, |_| Some(false));
        assert_eq!(v["clap"]["custom"], json!([{"path": "/opt/clap", "exists": false}]));
        assert_eq!(v["clap"]["env_var"], "CLAP_PATH");
        assert_eq!(v["vst3"]["custom"], json!([]));
        assert!(v["lv2"]["standard"].as_array().unwrap().iter().any(|s| s == "/usr/lib/lv2"));
    }

    /// The CLAP and VST3 scans read the custom folders after the standard
    /// ones, and a bundle placed there is what the scan enumerates.
    #[test]
    fn clap_and_vst3_scans_search_the_custom_folders_after_the_standard_ones() {
        let root = std::env::temp_dir().join(format!("ksalat-plugin-dirs-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (c, v) = (root.join("claps"), root.join("vst3s"));
        std::fs::create_dir_all(&c).unwrap();
        std::fs::create_dir_all(v.join("Probe.vst3")).unwrap();
        std::fs::write(c.join("Probe.clap"), b"x").unwrap();
        assert!(!crate::host::clap_search_dirs().contains(&c));
        let mut d = PluginDirs::default();
        d.add(Format::Clap, c.to_str().unwrap()).unwrap();
        d.add(Format::Vst3, v.to_str().unwrap()).unwrap();
        set(d);
        let clap = crate::host::clap_search_dirs();
        assert_eq!(clap.last(), Some(&c), "after the standard folders");
        assert!(clap.iter().position(|p| p.ends_with(".clap")) < clap.iter().position(|p| *p == c));
        assert_eq!(crate::vst3::scan::vst3_search_dirs().last(), Some(&v));
        let found = crate::vst3::scan::vst3_bundle_paths_in(&[v.clone()]);
        assert_eq!(found, [v.join("Probe.vst3")]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_config_field_round_trips_and_a_broken_one_loads_what_it_can() {
        let mut d = PluginDirs::default();
        d.add(Format::Clap, "/opt/clap").unwrap();
        d.add(Format::Vst3, "/opt/vst3").unwrap();
        d.add(Format::Lv2, "/opt/lv2 bundles").unwrap();
        let v = d.to_json();
        assert_eq!(v, json!({"clap": ["/opt/clap"], "vst3": ["/opt/vst3"], "lv2": ["/opt/lv2 bundles"]}));
        assert_eq!(PluginDirs::from_json(&v), d);

        let broken = json!({"clap": ["/opt/clap", 7, "relative", "/opt/clap"], "vst3": "oops", "other": ["/x"]});
        let got = PluginDirs::from_json(&broken);
        assert_eq!(got.clap, [PathBuf::from("/opt/clap")]);
        assert!(got.vst3.is_empty() && got.lv2.is_empty());
        assert_eq!(PluginDirs::from_json(&json!(null)), PluginDirs::default());
    }

    /// The config file keeps its other fields when the folders are written
    /// (a scratch file: tests never touch the user's config, rule 37).
    #[test]
    fn saving_the_folders_keeps_the_other_config_fields() {
        let dir = std::env::temp_dir().join(format!("ksalat-plugin-dirs-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"buffer_frames": 128, "plugin_dirs": {"clap": ["/old"]}}"#).unwrap();
        let mut d = PluginDirs::default();
        d.add(Format::Lv2, "/opt/lv2").unwrap();
        crate::assistant::save_config_values_to(&path, &[(CONFIG_FIELD, d.to_json())]).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["buffer_frames"], 128);
        assert_eq!(PluginDirs::from_json(&v[CONFIG_FIELD]), d);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_folders_of_one_test_thread_are_not_another_threads() {
        let mut d = PluginDirs::default();
        d.add(Format::Clap, "/opt/only-here").unwrap();
        set(d.clone());
        assert_eq!(current(), d);
        assert_eq!(custom(Format::Clap), d.clap);
        let other = std::thread::spawn(current).join().unwrap();
        assert_eq!(other, PluginDirs::default());
        assert_eq!(PluginDirs::from_json(&serde_json::from_str(&env_value()).unwrap()), d);
    }

    #[test]
    fn lv2_bundles_are_the_manifest_folders_inside_a_custom_folder() {
        let root = std::env::temp_dir().join(format!("ksalat-plugin-dirs-lv2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let custom_dir = root.join("custom");
        let std_dir = root.join("standard");
        for b in ["b.lv2", "a.lv2"] {
            std::fs::create_dir_all(custom_dir.join(b)).unwrap();
            std::fs::write(custom_dir.join(b).join("manifest.ttl"), "").unwrap();
        }
        std::fs::create_dir_all(custom_dir.join("notes")).unwrap();
        std::fs::create_dir_all(std_dir.join("s.lv2")).unwrap();
        std::fs::write(std_dir.join("s.lv2/manifest.ttl"), "").unwrap();
        let single = root.join("single.lv2");
        std::fs::create_dir_all(&single).unwrap();
        std::fs::write(single.join("manifest.ttl"), "").unwrap();

        let mut d = PluginDirs::default();
        for p in [&custom_dir, &std_dir, &single, &root.join("missing")] {
            d.add(Format::Lv2, p.to_str().unwrap()).unwrap();
        }
        // The same folder under a second spelling adds no bundle twice.
        std::os::unix::fs::symlink(&custom_dir, root.join("link")).unwrap();
        d.add(Format::Lv2, root.join("link").to_str().unwrap()).unwrap();
        set(d);
        let got = lv2_bundles(&[std_dir.clone()]);
        assert_eq!(got, [custom_dir.join("a.lv2"), custom_dir.join("b.lv2"), single], "sorted, no standard folder, no doubles");
        let _ = std::fs::remove_dir_all(&root);
    }
}
