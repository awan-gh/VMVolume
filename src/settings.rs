use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub gain_min: f32,
    pub gain_max: f32,
    /// Never boost: the top of the knob maps to 0 dB instead of gain_max.
    pub limit_db_gain_to_0: bool,
    /// false = log curve (20*log10, same as Voicemeeter Windows Volume), true = straight line.
    pub linear_volume_scale: bool,
    pub sync_mute: bool,
    /// Undo a jump to 100% right after a device change.
    pub block_volume_spikes: bool,
    /// Voicemeeter targets, e.g. "Bus[0]", "Strip[3]".
    pub targets: Vec<String>,
    /// Master switch for the audiodg.exe tweaks below ("crackle fix").
    pub crackle_fix: bool,
    /// audiodg.exe priority class (Win32 value, e.g. 0x80 = High); 0 = leave alone.
    pub audiodg_priority: u32,
    /// audiodg.exe CPU affinity bitmask; 0 = leave alone.
    pub audiodg_affinity: u64,
    pub start_with_windows: bool,
    /// Start elevated at logon (scheduled task) so the audiodg tweaks can apply.
    pub start_as_admin: bool,
    pub auto_update: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            gain_min: -60.0, gain_max: 12.0, limit_db_gain_to_0: true, linear_volume_scale: false, sync_mute: true,
            block_volume_spikes: true, targets: vec!["Bus[0]".into()], crackle_fix: true, audiodg_priority: 0, audiodg_affinity: 0,
            start_with_windows: true, start_as_admin: false, auto_update: true,
        }
    }
}

pub fn dir() -> PathBuf { PathBuf::from(std::env::var("APPDATA").unwrap_or_default()).join(crate::APP) }
pub fn path() -> PathBuf { dir().join("settings.json") }

impl Settings {
    pub fn load() -> Settings {
        if let Some(t) = std::fs::read_to_string(path()).ok() {
            if let Ok(mut s) = serde_json::from_str::<Settings>(&t) {
                // files from 0.1.0 predate the audiodg options: pick those up from VMWV once
                if !t.contains("audiodg_priority") {
                    if let Some(v) = import_vmwv() { (s.audiodg_priority, s.audiodg_affinity) = (v.audiodg_priority, v.audiodg_affinity); }
                    s.save();
                }
                return s;
            }
            let _ = std::fs::copy(path(), path().with_extension("json.corrupt"));
        }
        let s = import_vmwv().unwrap_or_default();
        s.save();
        s
    }
    /// Write via a temp file so a crash mid-write never leaves a broken settings file.
    pub fn save(&self) {
        let p = path();
        let _ = std::fs::create_dir_all(dir());
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, serde_json::to_string_pretty(self).unwrap()).is_ok() { let _ = std::fs::rename(&tmp, &p); }
    }
    /// Range the knob actually covers: [gain_min, top].
    pub fn top(&self) -> f32 { if self.limit_db_gain_to_0 { self.gain_max.min(0.0) } else { self.gain_max } }
}

/// Windows volume scalar (0..1) to dB.
pub fn gain_for(s: &Settings, scalar: f32) -> f32 {
    let (min, top) = (s.gain_min, s.top());
    let pct = (scalar * 100.0).round().clamp(0.0, 100.0);
    let g = if s.linear_volume_scale { pct * (top - min) / 100.0 + min }
        else if pct == 0.0 { min } else { (20.0 * (pct / 100.0).log10() + top).max(min) };
    ((g * 10.0).round() / 10.0).clamp(min, top.max(min))
}

/// Seed settings from an existing Voicemeeter Windows Volume (Modern) install so switching over is seamless.
fn import_vmwv() -> Option<Settings> {
    let p = PathBuf::from(std::env::var("LOCALAPPDATA").ok()?).join("Voicemeeter Windows Volume").join("settings.json");
    from_vmwv(&std::fs::read_to_string(p).ok()?)
}

pub fn from_vmwv(json: &str) -> Option<Settings> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let d = Settings::default();
    let f = |k: &str, def: f32| v.get(k).and_then(|x| x.as_f64()).map(|x| x as f32).unwrap_or(def);
    let b = |k: &str, def: bool| v.get(k).and_then(|x| x.as_bool()).unwrap_or(def);
    let toggles: Vec<(&str, bool)> = v.get("toggles").and_then(|t| t.as_array()).into_iter().flatten()
        .filter_map(|t| Some((t.get("setting")?.as_str()?, t.get("value")?.as_bool()?))).collect();
    let toggle = |k: &str| toggles.iter().any(|&(n, on)| n == k && on);
    let mut targets = vec![];
    for &(name, on) in &toggles {
        if !on { continue; }
        for kind in ["Strip", "Bus"] {
            if let Some(n) = name.strip_prefix(kind).and_then(|r| r.strip_prefix('_')).and_then(|n| n.parse::<u32>().ok()) {
                targets.push(format!("{kind}[{n}]"));
            }
        }
    }
    let adg = v.get("audiodg");
    let au = |k: &str| adg.and_then(|a| a.get(k)).and_then(|x| x.as_u64()).unwrap_or(0);
    Some(Settings {
        gain_min: f("gain_min", d.gain_min),
        gain_max: f("gain_max", d.gain_max),
        limit_db_gain_to_0: b("limit_db_gain_to_0", d.limit_db_gain_to_0),
        linear_volume_scale: toggle("linear_volume_scale"),
        sync_mute: b("sync_mute", d.sync_mute),
        block_volume_spikes: toggle("apply_volume_fix"),
        targets: if targets.is_empty() { d.targets } else { targets },
        audiodg_priority: au("priority") as u32,
        audiodg_affinity: au("affinity"),
        start_with_windows: b("start_with_windows", d.start_with_windows),
        ..d
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn curve() {
        let s = Settings::default(); // -60..12, limited to 0
        assert_eq!(gain_for(&s, 0.0), -60.0);
        assert_eq!(gain_for(&s, 1.0), 0.0);
        assert_eq!(gain_for(&s, 0.5), -6.0);
        assert_eq!(gain_for(&s, 0.001), -60.0);
        let s = Settings { limit_db_gain_to_0: false, ..s.clone() };
        assert_eq!(gain_for(&s, 1.0), 12.0);
        assert_eq!(gain_for(&s, 0.25), 0.0);
        let s = Settings { linear_volume_scale: true, limit_db_gain_to_0: true, ..s };
        assert_eq!(gain_for(&s, 0.5), -30.0);
    }
    #[test]
    fn imports_vmwv() {
        let j = r#"{"gain_min":-50,"gain_max":12,"limit_db_gain_to_0":true,"sync_mute":false,"audiodg":{"priority":128,"affinity":2},
            "toggles":[{"setting":"Strip_0","value":false},{"setting":"Bus_0","value":true},{"setting":"Bus_7","value":true},{"setting":"apply_volume_fix","value":true}]}"#;
        let s = from_vmwv(j).unwrap();
        assert_eq!(s.targets, vec!["Bus[0]", "Bus[7]"]);
        assert_eq!(s.gain_min, -50.0);
        assert!(!s.sync_mute && s.block_volume_spikes && !s.linear_volume_scale);
        assert_eq!((s.audiodg_priority, s.audiodg_affinity), (128, 2));
    }
    #[test]
    fn old_settings_file_still_loads() {
        let s: Settings = serde_json::from_str(r#"{"gain_min":-40.0,"targets":["Bus[2]"]}"#).unwrap();
        assert_eq!(s.gain_min, -40.0);
        assert!(s.auto_update && s.crackle_fix);
    }
}
