//! Restart the Voicemeeter engine when the device behind a watched bus connects (or disconnects).
//! Device events arrive on a COM thread; they are queued here and handled on the UI thread.
use std::{collections::HashMap, sync::Mutex};
use windows::{
    core::PCWSTR,
    Win32::{
        Devices::FunctionDiscovery::{PKEY_DeviceInterface_FriendlyName, PKEY_Device_FriendlyName},
        Media::Audio::IMMDeviceEnumerator,
        System::Com::STGM_READ,
    },
};

/// (endpoint id, active now)
pub static EVENTS: Mutex<Vec<(String, bool)>> = Mutex::new(Vec::new());

/// Playback endpoint ids start with {0.0.0., recording ones with {0.0.1.
pub fn is_render(id: &str) -> bool { id.starts_with("{0.0.0.") }

/// Endpoint name ("Speakers (SNOWSKY TINY A)") and adapter name ("SNOWSKY TINY A"), cached by id
/// because a removed device can no longer be looked up.
#[derive(Default)]
pub struct Names(HashMap<String, (String, String)>);

impl Names {
    pub fn get(&mut self, en: &IMMDeviceEnumerator, id: &str) -> Option<(String, String)> {
        if let Some(n) = lookup(en, id) { self.0.insert(id.to_string(), n); }
        self.0.get(id).cloned()
    }
    /// Remember every endpoint that exists now, connected or not.
    pub fn prefill(&mut self, en: &IMMDeviceEnumerator) {
        use windows::Win32::Media::Audio::{eAll, DEVICE_STATE, DEVICE_STATEMASK_ALL};
        unsafe {
            let Ok(col) = en.EnumAudioEndpoints(eAll, DEVICE_STATE(DEVICE_STATEMASK_ALL)) else { return };
            for i in 0..col.GetCount().unwrap_or(0) {
                let Ok(d) = col.Item(i) else { continue };
                let Ok(id) = d.GetId() else { continue };
                let id = id.to_string().unwrap_or_default();
                self.get(en, &id);
            }
        }
    }
}

fn lookup(en: &IMMDeviceEnumerator, id: &str) -> Option<(String, String)> {
    let w: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let d = en.GetDevice(PCWSTR(w.as_ptr())).ok()?;
        let ps = d.OpenPropertyStore(STGM_READ).ok()?;
        let name = ps.GetValue(&PKEY_Device_FriendlyName).map(|v| v.to_string()).unwrap_or_default();
        let iface = ps.GetValue(&PKEY_DeviceInterface_FriendlyName).map(|v| v.to_string()).unwrap_or_default();
        (!name.is_empty()).then_some((name, iface))
    }
}

/// Words too generic to identify a device on their own.
const GENERIC: [&str; 12] = ["speakers", "speaker", "headphones", "microphone", "mic", "line", "usb", "audio", "output", "device", "wdm", "high"];

fn words(s: &str) -> Vec<String> {
    s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() >= 2).map(String::from).collect()
}

/// Does Voicemeeter's bus device name refer to this Windows endpoint? Voicemeeter shows WDM names in full,
/// MME names cut to 31 characters, KS by filter name and ASIO by driver name, so compare distinctive words.
pub fn bus_matches(bus: &str, endpoint: &str, iface: &str) -> bool {
    let bus_l = bus.trim().to_lowercase();
    if bus_l.is_empty() { return false; }
    let ep_l = endpoint.to_lowercase();
    if ep_l == bus_l || (bus_l.len() >= 20 && ep_l.starts_with(&bus_l)) { return true; } // WDM / truncated MME
    let key: Vec<String> = words(bus).into_iter().filter(|w| !GENERIC.contains(&w.as_str())).collect();
    if key.is_empty() { return false; } // e.g. KS "Speakers": too vague to match safely
    let hay = words(&format!("{endpoint} {iface}"));
    key.iter().all(|k| hay.contains(k))
}

#[cfg(test)]
mod tests {
    use super::bus_matches;
    #[test]
    fn matching() {
        // WDM: exact endpoint name
        assert!(bus_matches("Speakers (SNOWSKY TINY A)", "Speakers (SNOWSKY TINY A)", "SNOWSKY TINY A"));
        // MME: cut to 31 chars
        assert!(bus_matches("Speakers (BEHRINGER USB WDM AUD", "Speakers (BEHRINGER USB WDM AUDIO 2.8.40)", "BEHRINGER USB WDM AUDIO 2.8.40"));
        // ASIO driver name vs endpoint
        assert!(bus_matches("BEHRINGER USB AUDIO", "Speakers (BEHRINGER USB WDM AUDIO 2.8.40)", "BEHRINGER USB WDM AUDIO 2.8.40"));
        // KS filter name
        assert!(bus_matches("dBE Headset", "dBE Headset (dbE Kumo GS1X Pro)", "dbE Kumo GS1X Pro"));
        // different devices
        assert!(!bus_matches("Speakers (SNOWSKY TINY A)", "Speakers Realtek (High Definition Audio Device)", "High Definition Audio Device"));
        assert!(!bus_matches("BEHRINGER USB AUDIO", "Speakers (SNOWSKY TINY A)", "SNOWSKY TINY A"));
        // too vague, and empty bus
        assert!(!bus_matches("Speakers", "Speakers (SNOWSKY TINY A)", "SNOWSKY TINY A"));
        assert!(!bus_matches("", "Speakers (SNOWSKY TINY A)", ""));
        // recording device on a hardware input
        assert!(bus_matches("Microphone (Maono AI Microphone)", "Microphone (Maono AI Microphone)", "Maono AI Microphone"));
        assert!(bus_matches("Maono AI Microphone", "Microphone (Maono AI Microphone)", "Maono AI Microphone"));
    }
}
