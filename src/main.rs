//! VMVolume: follows the Windows volume (and mute) of the default playback device and mirrors it onto
//! Voicemeeter strip/bus gain, so a physical volume knob drives Voicemeeter.
//! Event driven (IAudioEndpointVolume callback); the only timer is a 2 s housekeeping tick.
#![windows_subsystem = "windows"]

mod devices;
mod settings;
mod sys;
mod ui;
mod update;

use settings::{gain_for, Settings};
use std::{
    cell::RefCell,
    ffi::CString,
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering},
    time::{Duration, Instant},
};
use windows::{
    core::*,
    Win32::{
        Foundation::*,
        Media::Audio::{Endpoints::*, *},
        System::{Com::*, LibraryLoader::GetModuleHandleW, Registry::*},
        UI::{Shell::*, WindowsAndMessaging::*},
    },
};
use windows_core::implement;

pub const APP: &str = "VMVolume";
const WM_TRAY: u32 = WM_APP + 1;
const WM_VOL: u32 = WM_APP + 2; // new value is in VOL_* atomics
const WM_REBIND: u32 = WM_APP + 3; // default playback device changed
const WM_DEVEVENT: u32 = WM_APP + 4; // any audio device added/removed/state change
const WM_SHOW_SETTINGS: u32 = WM_APP + 5; // sent by a second launch
const WM_AUDIO_RESTARTED: u32 = WM_APP + 6; // Windows audio service came back: rebuild our audio hooks
const TIMER_TICK: usize = 1;
const ID_SETTINGS: usize = 10;
const ID_QUIT: usize = 11;
const ID_RESTART_VM: usize = 12;
const ID_RESTART_AUDIO: usize = 13;
const ID_RESTART_ENGINE: usize = 14;
const TIMER_ENGINE: usize = 2; // debounce for device-change engine restarts
const ENGINE_DEBOUNCE_MS: u32 = 2500;
const UPDATE_EVERY: Duration = Duration::from_secs(6 * 3600);

static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);
// Latest Windows volume from the COM callback thread; the UI thread picks it up (never lost even if a message is).
static VOL_SCALAR: AtomicU32 = AtomicU32::new(0);
static VOL_MUTED: AtomicBool = AtomicBool::new(false);

fn post(msg: u32) {
    let h = HWND(MAIN_HWND.load(Ordering::Relaxed) as _);
    unsafe { let _ = PostMessageW(Some(h), msg, WPARAM(0), LPARAM(0)); }
}

/// Append a line to %APPDATA%\VMVolumemvolume.log (reset when it passes 256 KB).
pub fn log(msg: &str) {
    use std::io::Write;
    let p = settings::dir().join("vmvolume.log");
    if std::fs::metadata(&p).map(|m| m.len() > 256 * 1024).unwrap_or(false) { let _ = std::fs::remove_file(&p); }
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) { let _ = writeln!(f, "{t} {msg}"); }
}

pub fn trim_memory() {
    unsafe { let _ = windows::Win32::System::ProcessStatus::K32EmptyWorkingSet(windows::Win32::System::Threading::GetCurrentProcess()); }
}

// ---------- Voicemeeter remote API ----------
type FnVoid = unsafe extern "system" fn() -> i32;
type FnScript = unsafe extern "system" fn(*const i8) -> i32;
type FnType = unsafe extern "system" fn(*mut i32) -> i32;
type FnGetS = unsafe extern "system" fn(*const i8, *mut u8) -> i32;
struct Vm {
    _lib: libloading::Library,
    login: FnVoid,
    logout: FnVoid,
    dirty: FnVoid,
    script: FnScript,
    kind: FnType,
    get_s: FnGetS,
}
impl Vm {
    fn load() -> Option<Vm> {
        let mut candidates = vec![PathBuf::from(r"C:\Program Files (x86)\VB\Voicemeeter\VoicemeeterRemote64.dll")];
        if let Some(dir) = voicemeeter_dir() { candidates.insert(0, dir.join("VoicemeeterRemote64.dll")); }
        let lib = candidates.iter().find_map(|p| unsafe { libloading::Library::new(p).ok() })?;
        unsafe {
            let vm = Vm {
                login: *lib.get::<FnVoid>(b"VBVMR_Login\0").ok()?,
                logout: *lib.get::<FnVoid>(b"VBVMR_Logout\0").ok()?,
                dirty: *lib.get::<FnVoid>(b"VBVMR_IsParametersDirty\0").ok()?,
                script: *lib.get::<FnScript>(b"VBVMR_SetParameters\0").ok()?,
                kind: *lib.get::<FnType>(b"VBVMR_GetVoicemeeterType\0").ok()?,
                get_s: *lib.get::<FnGetS>(b"VBVMR_GetParameterStringA\0").ok()?,
                _lib: lib,
            };
            (vm.login)();
            Some(vm)
        }
    }
    /// True when Voicemeeter is running and answering (IsParametersDirty is negative when not).
    fn connected(&self) -> bool { unsafe { (self.dirty)() >= 0 } }
    fn relogin(&self) { unsafe { (self.logout)(); (self.login)(); } }
    fn get_string(&self, param: &str) -> String {
        let c = CString::new(param).unwrap();
        let mut buf = [0u8; 512];
        unsafe { if (self.get_s)(c.as_ptr(), buf.as_mut_ptr()) != 0 { return String::new(); } }
        String::from_utf8_lossy(&buf[..buf.iter().position(|&b| b == 0).unwrap_or(0)]).into_owned()
    }
    fn name(&self) -> &'static str {
        let mut t = 0;
        unsafe { (self.kind)(&mut t); }
        match t { 1 => "Voicemeeter", 2 => "Voicemeeter Banana", 3 => "Voicemeeter Potato", _ => "Voicemeeter" }
    }
    /// One batched script per change, e.g. "Bus[0].Gain=-6.0;Bus[1].Gain=-6.0;".
    fn run(&self, script: &str) -> bool {
        let c = CString::new(script).unwrap();
        unsafe { (self.script)(c.as_ptr()) == 0 }
    }
}
impl Drop for Vm { fn drop(&mut self) { unsafe { (self.logout)(); } } }

/// Install folder from Voicemeeter's uninstall entry.
fn voicemeeter_dir() -> Option<PathBuf> {
    let mut buf = [0u16; 520];
    let mut len = (buf.len() * 2) as u32;
    unsafe {
        RegGetValueW(HKEY_LOCAL_MACHINE, w!(r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\VB:Voicemeeter {17359A74-1236-5467}"),
            w!("UninstallString"), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as _), Some(&mut len)).ok().ok()?;
    }
    let s = String::from_utf16_lossy(&buf[..buf.iter().position(|&c| c == 0)?]);
    PathBuf::from(s.trim_matches('"')).parent().map(|p| p.to_path_buf())
}

// ---------- Windows audio ----------
#[implement(IAudioEndpointVolumeCallback)]
struct VolCb;
impl IAudioEndpointVolumeCallback_Impl for VolCb_Impl {
    fn OnNotify(&self, data: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> Result<()> {
        if let Some(d) = unsafe { data.as_ref() } {
            VOL_SCALAR.store(d.fMasterVolume.to_bits(), Ordering::Relaxed);
            VOL_MUTED.store(d.bMuted.as_bool(), Ordering::Relaxed);
            post(WM_VOL);
        }
        Ok(())
    }
}

#[implement(IMMNotificationClient)]
struct DevCb;
impl IMMNotificationClient_Impl for DevCb_Impl {
    fn OnDeviceStateChanged(&self, id: &PCWSTR, state: DEVICE_STATE) -> Result<()> { dev_event(id, state == DEVICE_STATE_ACTIVE); Ok(()) }
    fn OnDeviceAdded(&self, _: &PCWSTR) -> Result<()> { post(WM_DEVEVENT); Ok(()) } // a state change follows
    fn OnDeviceRemoved(&self, id: &PCWSTR) -> Result<()> { dev_event(id, false); Ok(()) }
    fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, _: &PCWSTR) -> Result<()> {
        if flow == eRender && role == eMultimedia { post(WM_REBIND); }
        Ok(())
    }
    fn OnPropertyValueChanged(&self, _: &PCWSTR, _: &PROPERTYKEY) -> Result<()> { Ok(()) }
}

fn dev_event(id: &PCWSTR, active: bool) {
    let id = unsafe { id.to_string().unwrap_or_default() };
    devices::EVENTS.lock().unwrap().push((id, active));
    post(WM_DEVEVENT);
}

struct Audio {
    en: IMMDeviceEnumerator,
    vol: Option<IAudioEndpointVolume>,
    cb: IAudioEndpointVolumeCallback,
    devcb: IMMNotificationClient,
}
impl Audio {
    fn new() -> Result<Audio> {
        unsafe {
            let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let devcb: IMMNotificationClient = DevCb.into();
            en.RegisterEndpointNotificationCallback(&devcb)?;
            Ok(Audio { en, vol: None, cb: VolCb.into(), devcb })
        }
    }
    /// (Re)attach to the current default playback device; returns its (volume, muted).
    fn bind(&mut self) -> Option<(f32, bool)> {
        unsafe {
            if let Some(v) = self.vol.take() { let _ = v.UnregisterControlChangeNotify(&self.cb); }
            let dev = self.en.GetDefaultAudioEndpoint(eRender, eMultimedia).ok()?;
            let vol: IAudioEndpointVolume = dev.Activate(CLSCTX_ALL, None).ok()?;
            vol.RegisterControlChangeNotify(&self.cb).ok()?;
            let state = (vol.GetMasterVolumeLevelScalar().ok()?, vol.GetMute().ok()?.as_bool());
            self.vol = Some(vol);
            Some(state)
        }
    }
}
impl Drop for Audio {
    fn drop(&mut self) {
        unsafe {
            if let Some(v) = self.vol.take() { let _ = v.UnregisterControlChangeNotify(&self.cb); }
            let _ = self.en.UnregisterEndpointNotificationCallback(&self.devcb);
        }
    }
}

// ---------- app state ----------
pub struct App {
    pub s: Settings,
    vm: Option<Vm>,
    audio: Audio,
    hwnd: HWND,
    volume: f32,
    muted: bool,
    /// What Voicemeeter last received; None = resend everything.
    pub sent: Option<(f32, bool)>,
    connected: bool,
    vm_name: &'static str,
    last_dev_event: Instant,
    dev_names: devices::Names,
    /// why the pending debounced engine restart was scheduled
    engine_reason: String,
    audiodg_pid: Option<u32>,
    pub audiodg_status: String,
    pub update_status: String,
    last_update_check: Option<Instant>,
    updating: bool,
    started: Instant,
    /// "still running in the tray" note already shown this session
    close_hint_shown: bool,
    /// pinned our tray icon out of the ^ overflow (or gave up)
    tray_promoted: bool,
    /// the tray icon exists (adding fails while the taskbar is not up yet)
    tray_added: bool,
    pub settings_win: Option<ui::SettingsWindow>,
}

thread_local! { static STATE: RefCell<Option<App>> = const { RefCell::new(None) }; }

/// Run `f` on the app state; skipped if already borrowed (a window message arriving inside another handler).
pub fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    STATE.with(|a| a.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

impl App {
    pub fn apply(&mut self) {
        let Some(vm) = &self.vm else { return };
        if !self.connected { return; } // the tick re-applies once Voicemeeter answers
        let g = gain_for(&self.s, self.volume);
        // Push mute only on a real change (or when muted at first sync), so buses muted by hand in Voicemeeter stay muted.
        let push_mute = self.s.sync_mute && match self.sent { Some((_, m)) => m != self.muted, None => self.muted };
        if self.sent.map(|(sg, _)| sg) != Some(g) || push_mute {
            let mut script = String::new();
            for t in &self.s.targets { script += &format!("{t}.Gain={g:.1};"); }
            if push_mute { for t in self.s.mute_set() { script += &format!("{t}.Mute={};", self.muted as i32); } }
            if !script.is_empty() { vm.run(&script); }
        }
        let mute_now = if push_mute { self.muted } else { self.sent.map(|(_, m)| m).unwrap_or(self.muted) };
        self.sent = Some((g, mute_now));
    }

    fn tick(&mut self) {
        // Voicemeeter connection
        if self.vm.is_none() { self.vm = Vm::load(); }
        let now = match &self.vm {
            Some(vm) => vm.connected() || { vm.relogin(); vm.connected() },
            None => false,
        };
        if now && !self.connected { self.vm_name = self.vm.as_ref().unwrap().name(); self.connected = true; self.sent = None; log(&format!("connected to {}", self.vm_name)); }
        if !now && self.connected { log("lost Voicemeeter connection"); }
        self.connected = now;
        // catch up on a volume message that was dropped while another handler ran
        self.on_volume(false);
        // audiodg restarts with the audio service: re-apply our tweaks to the new process
        if self.s.crackle_fix && (self.s.audiodg_priority != 0 || self.s.audiodg_affinity != 0) {
            let pid = sys::audiodg_pid();
            if pid != self.audiodg_pid { self.apply_audiodg(); }
        }
        // the taskbar may not exist yet right after logon: keep trying until the icon is there
        if !self.tray_added {
            self.tray_added = tray(self.hwnd, NIM_ADD, &self.status_line().replace('\n', " · "));
            if self.tray_added { log("tray icon added (retry)"); }
        }
        // Windows 11 hides new tray icons under ^: show ours once Explorer has registered it (first minute only)
        if !self.tray_promoted && self.started.elapsed() > Duration::from_secs(4) {
            self.tray_promoted = sys::promote_tray_icon() || self.started.elapsed() > Duration::from_secs(60);
        }
        // updates: first check 30 s after start, then every 6 h
        let due = match self.last_update_check { None => self.started.elapsed() > Duration::from_secs(30), Some(t) => t.elapsed() > UPDATE_EVERY };
        if self.s.auto_update && due { start_update_check(self, false); }
    }

    fn on_volume(&mut self, from_event: bool) {
        let v = f32::from_bits(VOL_SCALAR.load(Ordering::Relaxed));
        let m = VOL_MUTED.load(Ordering::Relaxed);
        if !from_event && v == self.volume && m == self.muted { self.apply(); return; }
        // Windows sometimes slams the volume to 100% when devices appear/disappear: put it back.
        if self.s.block_volume_spikes && v >= 0.995 && self.volume < 0.95 && self.last_dev_event.elapsed() < Duration::from_secs(3) {
            if let Some(ep) = &self.audio.vol { unsafe { let _ = ep.SetMasterVolumeLevelScalar(self.volume, std::ptr::null()); } }
            return;
        }
        self.volume = v;
        self.muted = m;
        self.apply();
        self.refresh_ui();
    }

    fn rebind(&mut self) {
        self.last_dev_event = Instant::now();
        if let Some((v, m)) = self.audio.bind() {
            VOL_SCALAR.store(v.to_bits(), Ordering::Relaxed);
            VOL_MUTED.store(m, Ordering::Relaxed);
            self.volume = v;
            self.muted = m;
            self.apply();
        }
        self.refresh_ui();
    }

    pub fn apply_audiodg(&mut self) {
        self.audiodg_pid = sys::audiodg_pid();
        if !self.s.crackle_fix { self.audiodg_status = "crackle fix is off".into(); self.refresh_ui(); return; }
        self.audiodg_status = match sys::apply_audiodg(self.s.audiodg_priority, self.s.audiodg_affinity) { Ok(s) => s, Err(e) => e };
        log(&format!("audiodg: {}", self.audiodg_status));
        self.refresh_ui();
    }

    /// Turning the crackle fix off puts audiodg back to defaults right away.
    pub fn reset_audiodg(&mut self) {
        self.audiodg_status = match sys::reset_audiodg() { Ok(s) => s, Err(e) => e };
        log(&format!("audiodg: {}", self.audiodg_status));
        self.refresh_ui();
    }

    /// Close Voicemeeter cleanly (it saves its settings on shutdown), then start it again as the normal user.
    fn restart_voicemeeter(&mut self) {
        const EXES: [&str; 6] = ["voicemeeter8x64.exe", "voicemeeter8.exe", "voicemeeterpro_x64.exe", "voicemeeterpro.exe", "voicemeeter_x64.exe", "voicemeeter.exe"];
        let running = sys::find_process(&EXES);
        let path = running.as_ref().map(|(_, p)| p.clone()).or_else(|| {
            let dir = voicemeeter_dir()?;
            EXES.iter().map(|e| dir.join(e)).find(|p| p.exists())
        });
        let Some(path) = path else { log("restart Voicemeeter: not installed"); return };
        if let (Some(vm), true) = (&self.vm, running.is_some()) { vm.run("Command.Shutdown=1;"); }
        log(&format!("restarting Voicemeeter ({})", path.display()));
        std::thread::spawn(move || {
            if let Some((pid, _)) = running { sys::wait_or_kill(pid, 8); }
            std::thread::sleep(Duration::from_millis(500));
            sys::launch_unelevated(&path);
        });
    }

    pub fn restart_engine(&mut self, why: &str) {
        let Some(vm) = &self.vm else { return };
        if !self.connected { return; }
        vm.run("Command.Restart=1;");
        self.sent = None; // push the knob position again once the engine is back
        log(&format!("restarted Voicemeeter engine ({why})"));
    }

    /// A device connected or disconnected: if it sits on a watched slot (playback devices on A1..A5,
    /// recording devices on hardware inputs 1..5), restart the engine. Debounced: one plug fires several events.
    fn on_device_events(&mut self) {
        self.last_dev_event = Instant::now();
        let events: Vec<(String, bool)> = std::mem::take(&mut *devices::EVENTS.lock().unwrap());
        if events.is_empty() || !self.connected { return; }
        let Some(vm) = &self.vm else { return };
        let mut watched: Vec<(bool, String, String)> = vec![]; // (playback?, label, Voicemeeter device name)
        for (i, _) in self.s.engine_restart_buses.iter().enumerate().filter(|(_, &on)| on) {
            watched.push((true, format!("A{}", i + 1), vm.get_string(&format!("Bus[{i}].device.name"))));
        }
        for (i, _) in self.s.engine_restart_strips.iter().enumerate().filter(|(_, &on)| on) {
            watched.push((false, format!("input {}", i + 1), vm.get_string(&format!("Strip[{i}].device.name"))));
        }
        if watched.is_empty() { return; }
        for (id, active) in events {
            if !active && !self.s.engine_restart_on_disconnect { continue; }
            let render = devices::is_render(&id);
            let Some((name, iface)) = self.dev_names.get(&self.audio.en, &id) else { continue };
            if let Some((_, label, _)) = watched.iter().find(|(r, _, dev)| *r == render && devices::bus_matches(dev, &name, &iface)) {
                self.engine_reason = format!("{label} device {name} {}", if active { "connected" } else { "disconnected" });
                unsafe { SetTimer(Some(self.hwnd), TIMER_ENGINE, ENGINE_DEBOUNCE_MS, None); } // re-arming restarts the wait
            }
        }
    }

    fn restart_audio_service(&mut self) {
        log("restarting Windows audio service");
        std::thread::spawn(|| {
            let ok = sys::restart_audio_service();
            log(&format!("Windows audio service restart: {}", if ok { "done" } else { "failed or cancelled" }));
            std::thread::sleep(Duration::from_secs(2));
            post(WM_AUDIO_RESTARTED);
        });
    }

    /// The audio service restart invalidates our endpoint hooks: build them again.
    fn rebuild_audio(&mut self) {
        if let Ok(a) = Audio::new() { self.audio = a; }
        self.rebind();
    }

    /// Closing the settings window keeps the app running: say so the first time.
    pub fn on_settings_closed(&mut self) {
        if self.close_hint_shown { return; }
        self.close_hint_shown = true;
        balloon(self.hwnd, "VMVolume is still running", "It keeps syncing your volume from the tray. Right-click the tray icon to quit.");
    }

    pub fn status_line(&self) -> String {
        let conn = if self.connected { format!("Connected to {}", self.vm_name) } else { "Waiting for Voicemeeter…".into() };
        let admin = if sys::is_admin() { " · administrator" } else { "" };
        format!("{conn}{admin}\nWindows volume {}% → {:.1} dB{}", (self.volume * 100.0).round(), gain_for(&self.s, self.volume), if self.muted { " (muted)" } else { "" })
    }

    fn refresh_ui(&self) {
        tray(self.hwnd, NIM_MODIFY, &self.status_line().replace('\n', " · "));
        if let Some(w) = &self.settings_win { ui::refresh(w, self); }
    }
}

/// Check GitHub in the background; install right away if newer (auto mode or "Check now").
pub fn start_update_check(a: &mut App, manual: bool) {
    if a.updating { return; }
    a.updating = true;
    a.last_update_check = Some(Instant::now());
    a.update_status = "Checking for updates…".into();
    a.refresh_ui();
    std::thread::spawn(move || {
        let result = update::check().and_then(|r| match r {
            None => Ok(None),
            Some(rel) => update::install(&rel).map(|_| Some(rel.version)),
        });
        let _ = slint::invoke_from_event_loop(move || {
            with_app(|a| {
                a.updating = false;
                log(&format!("update check: {}", match &result { Ok(None) => "up to date".into(), Ok(Some(v)) => format!("installed {v}"), Err(e) => e.clone() }));
                let when = "just now";
                a.update_status = match &result {
                    Ok(None) => format!("Up to date ({}), checked {when}", env!("CARGO_PKG_VERSION")),
                    Ok(Some(v)) => format!("Updated to {v}, restarting…"),
                    Err(e) if manual => format!("Update check failed: {e}"),
                    Err(e) => format!("Last check failed: {e}"),
                };
                a.refresh_ui();
            });
            if matches!(result, Ok(Some(_))) && sys::relaunch() { slint::quit_event_loop().ok(); }
        });
    });
}

// ---------- tray + window ----------
/// Small notification from the tray icon.
fn balloon(h: HWND, title: &str, text: &str) {
    let mut d = NOTIFYICONDATAW { cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32, hWnd: h, uID: 1, ..Default::default() };
    d.uFlags = NIF_INFO;
    d.dwInfoFlags = NIIF_INFO;
    let t: Vec<u16> = title.encode_utf16().take(d.szInfoTitle.len() - 1).collect();
    d.szInfoTitle[..t.len()].copy_from_slice(&t);
    let b: Vec<u16> = text.encode_utf16().take(d.szInfo.len() - 1).collect();
    d.szInfo[..b.len()].copy_from_slice(&b);
    unsafe { let _ = Shell_NotifyIconW(NIM_MODIFY, &d); }
}

fn tray(h: HWND, op: NOTIFY_ICON_MESSAGE, tip: &str) -> bool {
    let mut d = NOTIFYICONDATAW { cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32, hWnd: h, uID: 1, ..Default::default() };
    d.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    d.uCallbackMessage = WM_TRAY;
    d.hIcon = unsafe {
        let inst = GetModuleHandleW(None).unwrap_or_default();
        LoadImageW(Some(inst.into()), PCWSTR(1 as _), IMAGE_ICON, GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON), LR_DEFAULTCOLOR)
            .map(|h| HICON(h.0)).unwrap_or_else(|_| LoadIconW(None, IDI_APPLICATION).unwrap_or_default())
    };
    let w: Vec<u16> = tip.encode_utf16().take(d.szTip.len() - 1).collect();
    d.szTip[..w.len()].copy_from_slice(&w);
    unsafe { Shell_NotifyIconW(op, &d).as_bool() }
}

fn menu(h: HWND) -> usize {
    unsafe {
        let m = CreatePopupMenu().unwrap();
        let _ = AppendMenuW(m, MF_STRING, ID_SETTINGS, w!("Settings"));
        let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(m, MF_STRING, ID_RESTART_ENGINE, w!("Restart Voicemeeter engine"));
        let _ = AppendMenuW(m, MF_STRING, ID_RESTART_VM, w!("Restart Voicemeeter"));
        let _ = AppendMenuW(m, MF_STRING, ID_RESTART_AUDIO, w!("Restart Windows audio"));
        let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(m, MF_STRING, ID_QUIT, w!("Quit"));
        let _ = SetMenuDefaultItem(m, ID_SETTINGS as u32, 0);
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(h);
        let id = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, Some(0), h, None).0 as usize;
        let _ = DestroyMenu(m);
        id
    }
}

static TASKBAR_CREATED: std::sync::OnceLock<u32> = std::sync::OnceLock::new();

extern "system" fn wndproc(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_VOL => { with_app(|a| a.on_volume(true)); LRESULT(0) }
            WM_DEVEVENT => { with_app(|a| a.on_device_events()); LRESULT(0) }
            WM_TIMER if w.0 == TIMER_ENGINE => {
                let _ = KillTimer(Some(h), TIMER_ENGINE);
                with_app(|a| { let why = std::mem::take(&mut a.engine_reason); a.restart_engine(&why); });
                LRESULT(0)
            }
            WM_REBIND => { with_app(|a| a.rebind()); LRESULT(0) }
            WM_AUDIO_RESTARTED => { with_app(|a| a.rebuild_audio()); LRESULT(0) }
            WM_SHOW_SETTINGS => { let _ = slint::invoke_from_event_loop(ui::open); LRESULT(0) }
            WM_TIMER if w.0 == TIMER_TICK => { with_app(|a| a.tick()); LRESULT(0) }
            WM_TRAY => {
                match l.0 as u32 & 0xFFFF {
                    // the menu runs its own modal loop: never hold the app state across it
                    WM_RBUTTONUP => match menu(h) {
                        ID_SETTINGS => { let _ = slint::invoke_from_event_loop(ui::open); }
                        ID_QUIT => { slint::quit_event_loop().ok(); }
                        ID_RESTART_ENGINE => { with_app(|a| a.restart_engine("tray menu")); }
                        ID_RESTART_VM => { with_app(|a| a.restart_voicemeeter()); }
                        ID_RESTART_AUDIO => { with_app(|a| a.restart_audio_service()); }
                        _ => {}
                    },
                    WM_LBUTTONDBLCLK | WM_LBUTTONUP => { let _ = slint::invoke_from_event_loop(ui::open); }
                    _ => {}
                }
                LRESULT(0)
            }
            _ if Some(&msg) == TASKBAR_CREATED.get() => { // Explorer restarted
                let tip = with_app(|a| a.status_line().replace('\n', " · ")).unwrap_or_default();
                let ok = tray(h, NIM_ADD, &tip);
                with_app(|a| a.tray_added = ok);
                LRESULT(0)
            }
            _ => DefWindowProcW(h, msg, w, l),
        }
    }
}

fn main() -> Result<()> {
    let takeover = std::env::args().any(|a| a == "--takeover");
    let Some(_mutex) = sys::single_instance(takeover) else {
        // already running: launching the exe again opens its settings
        unsafe {
            if let Ok(h) = FindWindowW(w!("VMVolumeWnd"), None) { let _ = PostMessageW(Some(h), WM_SHOW_SETTINGS, WPARAM(0), LPARAM(0)); }
        }
        return Ok(());
    };
    update::cleanup();
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let inst = GetModuleHandleW(None)?;
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: inst.into(), lpszClassName: w!("VMVolumeWnd"), ..Default::default() };
        RegisterClassW(&wc);
        // A hidden top-level window (not message-only) so it receives the TaskbarCreated broadcast.
        let h = CreateWindowExW(Default::default(), w!("VMVolumeWnd"), w!("VMVolume"), WS_OVERLAPPED, 0, 0, 0, 0, None, None, Some(inst.into()), None)?;
        MAIN_HWND.store(h.0 as isize, Ordering::Relaxed);
        // let a non-elevated second launch reach an elevated instance
        let _ = ChangeWindowMessageFilterEx(h, WM_SHOW_SETTINGS, MSGFLT_ALLOW, None);
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
        let _ = TASKBAR_CREATED.set(taskbar_created);
        // Elevated: Windows drops Explorer's TaskbarCreated broadcast unless we allow it, and then the icon
        // never comes back after logon (taskbar not ready yet) or an Explorer restart.
        let _ = ChangeWindowMessageFilterEx(h, taskbar_created, MSGFLT_ALLOW, None);

        let s = Settings::load();
        log(&format!("start {} admin={} crackle_fix={} priority={:#x} affinity={:#x} audiodg_pid={:?}", env!("CARGO_PKG_VERSION"), sys::is_admin(), s.crackle_fix, s.audiodg_priority, s.audiodg_affinity, sys::audiodg_pid()));
        // keep the plain startup entry pointing at this exe if it moved (the elevated task is only touched from settings)
        if s.start_with_windows && !s.start_as_admin {
            sys::apply_startup(true, false);
        }
        // Right after logon the audio service can still be starting: retry instead of quitting.
        let mut audio = loop {
            match Audio::new() { Ok(a) => break a, Err(_) => std::thread::sleep(Duration::from_secs(2)) }
        };
        let (volume, muted) = audio.bind().unwrap_or((1.0, false));
        VOL_SCALAR.store(volume.to_bits(), Ordering::Relaxed);
        VOL_MUTED.store(muted, Ordering::Relaxed);
        let app = App {
            s, vm: Vm::load(), audio, hwnd: h, volume, muted, sent: None, connected: false, vm_name: "Voicemeeter",
            last_dev_event: Instant::now(), dev_names: devices::Names::default(), engine_reason: String::new(), audiodg_pid: None, audiodg_status: "not changed".into(), update_status: String::new(),
            last_update_check: None, updating: false, started: Instant::now(), settings_win: None, close_hint_shown: false, tray_promoted: false, tray_added: false,
        };
        STATE.with(|st| *st.borrow_mut() = Some(app));
        with_app(|a| { let en = a.audio.en.clone(); a.dev_names.prefill(&en); });
        with_app(|a| {
            a.tray_added = tray(h, NIM_ADD, "VMVolume");
            log(&format!("tray icon added at start: {}", a.tray_added));
            a.tick();
            a.refresh_ui();
        });
        SetTimer(Some(h), TIMER_TICK, 2000, None);
        if std::env::args().any(|a| a == "--settings") { ui::open(); }
        slint::Timer::single_shot(Duration::from_secs(5), trim_memory);

        slint::run_event_loop_until_quit().ok();

        let _ = KillTimer(Some(h), TIMER_TICK);
        tray(h, NIM_DELETE, "");
        STATE.with(|st| *st.borrow_mut() = None); // unregister callbacks, log out of Voicemeeter
        CoUninitialize();
    }
    Ok(())
}
