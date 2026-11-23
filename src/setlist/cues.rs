//! Show cues of songs and parts as the tools edit them (#401). The app and
//! headless MCP answer with the same functions.

use serde_json::{json, Value};

use super::Setlist;
use crate::cues::{self, Cue};

/// Cue lines from a tool argument: an array of strings, one string of lines,
/// or `null` for none.
pub fn lines_of(v: &Value) -> Result<Vec<Cue>, String> {
    match v {
        Value::Null => Ok(Vec::new()),
        Value::String(s) => cues::parse_lines(s.lines()),
        Value::Array(a) => {
            let lines: Result<Vec<&str>, String> =
                a.iter().map(|x| x.as_str().ok_or_else(|| "each cue is a line of text like \"pc 1 5\"".to_string())).collect();
            cues::parse_lines(lines?)
        }
        _ => Err("cues is an array of lines like \"pc 1 5\", or null".into()),
    }
}

impl Setlist {
    /// The `get_cues` answer: every song's and part's cue lines.
    pub fn cues_json(&self) -> Value {
        let lines = |c: &[Cue]| c.iter().map(Cue::text).collect::<Vec<_>>();
        let songs: Vec<Value> = self
            .songs
            .iter()
            .enumerate()
            .map(|(i, s)| {
                json!({
                    "song": i, "name": s.name, "cues": lines(&s.cues),
                    "parts": s.parts.iter().enumerate()
                        .map(|(j, p)| json!({"part": j, "name": p.name, "cues": lines(&p.cues)}))
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        json!({"songs": songs, "syntax": "pc <ch> <program> | cc <ch> <controller> <value> | note <ch> <note> [velocity] (optionally @<MIDI out port>) | osc <host>:<port> </address> [args]"})
    }

    /// The `set_cues` tool: replace the cues of a song (`song`) or of one of
    /// its parts (`song` + `part`). Returns the cues now set.
    pub fn set_cues(&mut self, args: &Value) -> Result<Vec<Cue>, String> {
        let song = args.get("song").and_then(Value::as_u64).ok_or("song (0-based index) is required")? as usize;
        let n = self.songs.len();
        let s = self.songs.get_mut(song).ok_or_else(|| format!("no song at index {song} (setlist has {n})"))?;
        let new = lines_of(args.get("cues").unwrap_or(&Value::Null))?;
        match args.get("part").filter(|v| !v.is_null()) {
            None => {
                s.cues = new.clone();
            }
            Some(p) => {
                let part = p.as_u64().ok_or("part is a 0-based index")? as usize;
                let m = s.parts.len();
                let p = s.parts.get_mut(part).ok_or_else(|| format!("song {song} has no part {part} (it has {m})"))?;
                p.cues = new.clone();
            }
        }
        Ok(new)
    }

    /// Replace a song's parts, keeping each part's cues when the new part of
    /// the same name does not bring its own.
    pub fn replace_parts(&mut self, song: usize, mut parts: Vec<super::Part>) {
        let Some(s) = self.songs.get_mut(song) else { return };
        for np in &mut parts {
            if np.cues.is_empty() {
                if let Some(old) = s.parts.iter().find(|o| o.name == np.name) {
                    np.cues = old.cues.clone();
                }
            }
        }
        s.parts = parts;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setlist::{Part, PartRig, Song};

    fn two_songs() -> Setlist {
        let mut sl = Setlist::default();
        for n in ["A", "B"] {
            sl.songs.push(Song::new(n, vec![Part::new("Verse", vec![PartRig::new(0)]), Part::new("Chorus", vec![PartRig::new(0)])]));
        }
        sl
    }

    #[test]
    fn the_tool_sets_song_and_part_cues_reports_them_and_refuses_in_plain_words() {
        let mut sl = two_songs();
        sl.set_cues(&json!({"song": 1, "cues": ["pc 1 5", "osc 10.0.0.9:8000 /go 1"]})).unwrap();
        sl.set_cues(&json!({"song": 1, "part": 1, "cues": "cc 1 20 127\nnote 2 C3"})).unwrap();
        let v = sl.cues_json();
        assert_eq!(v["songs"][1]["cues"], json!(["pc 1 5", "osc 10.0.0.9:8000 /go 1"]));
        assert_eq!(v["songs"][1]["parts"][1]["cues"], json!(["cc 1 20 127", "note 2 C3 100"]));
        assert_eq!(v["songs"][0]["cues"], json!([]));
        for (bad, why) in [
            (json!({"cues": []}), "song"),
            (json!({"song": 7, "cues": []}), "no song at index 7"),
            (json!({"song": 0, "part": 5, "cues": []}), "no part 5"),
            (json!({"song": 0, "cues": ["pc 1 200"]}), "program"),
            (json!({"song": 0, "cues": [3]}), "line of text"),
        ] {
            assert!(sl.set_cues(&bad).unwrap_err().contains(why), "{bad}");
        }
        sl.set_cues(&json!({"song": 1, "cues": null})).unwrap();
        assert!(sl.songs[1].cues.is_empty());
    }

    #[test]
    fn cues_survive_a_session_save_and_a_part_rewrite() {
        let mut sl = two_songs();
        sl.set_cues(&json!({"song": 0, "part": 1, "cues": ["pc 3 9 @desk"]})).unwrap();
        sl.set_cues(&json!({"song": 0, "cues": ["osc 127.0.0.1:9000 /s \"a b\""]})).unwrap();
        let back: Setlist = serde_json::from_str(&serde_json::to_string(&sl).unwrap()).unwrap();
        assert_eq!(back, sl);
        // A setlist saved before cues has none.
        let old: Setlist = serde_json::from_str(r#"{"songs":[{"name":"X","parts":[{"name":"P"}]}]}"#).unwrap();
        assert!(old.songs[0].cues.is_empty() && old.songs[0].parts[0].cues.is_empty());
        // Rewriting the parts keeps what a same-named part had.
        sl.replace_parts(0, vec![Part::new("Chorus", vec![]), Part::new("Bridge", vec![])]);
        assert_eq!(sl.songs[0].parts[0].cues.len(), 1);
        assert!(sl.songs[0].parts[1].cues.is_empty());
        assert_eq!(sl.songs[0].cues.len(), 1);
    }
}
