//! Guitarix, the guitar amp suite (issue #92): the LV2 URIs the Guitar rig
//! template wires, in one place. Everything else (detection on the catalog
//! card, main knobs on the faces) reads the constants from here.
//!
//! The Arch package `guitarix` installs all of these under `/usr/lib/lv2` —
//! aarch64-native (rule 114: `file` on `gx_amp.so` says ARM aarch64). Every
//! amp/cab/stomp below is MONO (1 in, 1 out): the engine feeds a mono LV2 the
//! left channel and duplicates its output to both (`Lv2Plugin::process`).
//!
//! Guitarix has no `param:` designations on its control ports, so a unit's
//! main knobs come from the first usable continuous parameters
//! (`main_controls::resolve`, rule 89) — GxAmplifier starts with
//! MasterGain/PreGain/Distortion/Drive.

/// The guitar amplifier: tube preamp + tone stack + power amp, with a
/// matched cabinet impulse built in (`Cabinet` control: 4x12 / 2x12 / 1x12).
pub const AMP: &str = "http://guitarix.sourceforge.net/plugins/gx_amp#GUITARIX";

/// The separate cabinet emulator (its own impulse + bass/treble shaping),
/// for the template's dedicated Cabinet unit after the amp.
pub const CABINET: &str = "http://guitarix.sourceforge.net/plugins/gx_cabinet#CABINET";

/// "Scream" — the tube-screamer overdrive stomp in front of the amp.
pub const SCREAM: &str = "http://guitarix.sourceforge.net/plugins/gx_scream_#_scream_";

/// GxAmplifier's "Drive" control ordinal — its control ports in port order
/// are MasterGain 0, PreGain 1, Distortion 2, Drive 3, … (verified against
/// the installed bundle: `guitarix_amps_process_from_the_first_block`).
/// Drive is the tube stage's gain, the one control that turns the modelled
/// clean channel into a crunch: the Guitar rig template sets it to its
/// maximum, so the rig sounds like an amp from the first chord (the plugin's
/// own default is a nearly clean 0.25).
pub const AMP_DRIVE_CTRL: usize = 3;

/// GxAmplifier's "PreGain" ordinal (control port 1). The stomp's own
/// output is quiet (the screamer at its defaults passes about a quarter of
/// the input level), so the template raises the amp's input gain to keep the
/// tube stage working.
pub const AMP_PREGAIN_CTRL: usize = 1;

/// Every URI of the suite the catalog card detects / the template may wire.
pub const ALL: &[&str] = &[AMP, CABINET, SCREAM];

/// The URIs that make a rack a guitar rig, for the agent's check: the amp
/// (optionally with its cabinet) is the one unit that says "guitar".
pub fn is_amp_uri(uri: &str) -> bool {
    uri == AMP
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_are_the_plugins_the_package_ships() {
        // Read from the installed bundles when present, so the constants
        // cannot drift from what guitarix actually installs (rule 63: the
        // parser is tested against the tool's REAL output).
        let manifest = |bundle: &str| std::fs::read_to_string(format!("/usr/lib/lv2/{bundle}/manifest.ttl"));
        if let Ok(m) = manifest("gx_amp.lv2") {
            assert!(m.contains(AMP), "gx_amp manifest does not name {AMP}");
        }
        if let Ok(m) = manifest("gx_cabinet.lv2") {
            assert!(m.contains(CABINET), "gx_cabinet manifest does not name {CABINET}");
        }
        if let Ok(m) = manifest("gx_scream.lv2") {
            assert!(m.contains(SCREAM), "gx_scream manifest does not name {SCREAM}");
        }
        // Self-consistency even without the package (CI).
        assert!(ALL.contains(&AMP) && is_amp_uri(AMP));
        assert!(!is_amp_uri(SCREAM));
    }
}
