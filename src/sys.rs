//! Windows plumbing: audiodg tweaks, start-with-Windows (plain or elevated), elevation, single instance.
use windows::{
    core::*,
    Win32::{
        Foundation::*,
        Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY},
        System::{Diagnostics::ToolHelp::*, Registry::*, Threading::*},
        UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_HIDE},
    },
};

pub fn is_admin() -> bool {
    unsafe {
        let mut tok = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok).is_err() { return false; }
        let mut e = TOKEN_ELEVATION::default();
        let mut len = 0;
        let ok = GetTokenInformation(tok, TokenElevation, Some(&mut e as *mut _ as _), std::mem::size_of::<TOKEN_ELEVATION>() as u32, &mut len).is_ok();
        let _ = CloseHandle(tok);
        ok && e.TokenIsElevated != 0
    }
}

/// Holds the single-instance mutex for the process lifetime. With `takeover`, waits up to 10 s
/// for the previous instance (which is restarting us elevated or after an update) to exit.
pub fn single_instance(takeover: bool) -> Option<HANDLE> {
    for _ in 0..if takeover { 50 } else { 1 } {
        unsafe {
            let h = CreateMutexW(None, true, w!("Local\\VMVolume.single")).ok()?;
            if GetLastError() != ERROR_ALREADY_EXISTS { return Some(h); }
            let _ = CloseHandle(h);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    None
}

fn shell(verb: PCWSTR, file: &str, args: &str) -> bool {
    let f: Vec<u16> = file.encode_utf16().chain(Some(0)).collect();
    let a: Vec<u16> = args.encode_utf16().chain(Some(0)).collect();
    unsafe { ShellExecuteW(None, verb, PCWSTR(f.as_ptr()), PCWSTR(a.as_ptr()), None, SW_HIDE).0 as isize > 32 }
}

fn exe() -> String { std::env::current_exe().unwrap_or_default().display().to_string() }

/// Relaunch this exe elevated (UAC prompt); caller quits on success.
pub fn relaunch_as_admin() -> bool { shell(w!("runas"), &exe(), "--takeover") }
/// Relaunch normally (after an update); caller quits on success.
pub fn relaunch() -> bool { std::process::Command::new(exe()).arg("--takeover").spawn().is_ok() }

// ---------- audiodg ----------
pub fn audiodg_pid() -> Option<u32> {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut found = None;
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let n = String::from_utf16_lossy(&e.szExeFile[..e.szExeFile.iter().position(|&c| c == 0).unwrap_or(0)]);
                if n.eq_ignore_ascii_case("audiodg.exe") { found = Some(e.th32ProcessID); break; }
                if Process32NextW(snap, &mut e).is_err() { break; }
            }
        }
        let _ = CloseHandle(snap);
        found
    }
}

/// Apply priority/affinity to audiodg.exe. Err text is shown in the settings window.
pub fn apply_audiodg(priority: u32, affinity: u64) -> std::result::Result<String, String> {
    if priority == 0 && affinity == 0 { return Ok("not changed".into()); }
    let pid = audiodg_pid().ok_or("audiodg.exe is not running")?;
    unsafe {
        let h = OpenProcess(PROCESS_SET_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
            .map_err(|_| "needs administrator: turn on \"Run as administrator\" below".to_string())?;
        let mut err = None;
        if priority != 0 && SetPriorityClass(h, PROCESS_CREATION_FLAGS(priority)).is_err() { err = Some("could not set priority"); }
        if affinity != 0 && SetProcessAffinityMask(h, affinity as usize).is_err() { err = Some("could not set affinity (no such core?)"); }
        let _ = CloseHandle(h);
        match err { Some(e) => Err(e.into()), None => Ok(format!("applied to audiodg.exe (pid {pid})")) }
    }
}

pub fn cpu_count() -> usize { std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(64) }

// ---------- start with Windows ----------
const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const TASK: &str = "VMVolume";

fn set_run_key(on: bool) {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, Some(0), KEY_SET_VALUE, &mut key).is_err() { return; }
        if on {
            let v: Vec<u16> = format!("\"{}\"", exe()).encode_utf16().chain(Some(0)).collect();
            let _ = RegSetValueExW(key, w!("VMVolume"), Some(0), REG_SZ, Some(std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 2)));
        } else {
            let _ = RegDeleteValueW(key, w!("VMVolume"));
        }
        let _ = RegCloseKey(key);
    }
}

fn task_exists() -> bool {
    std::process::Command::new("schtasks").args(["/Query", "/TN", TASK]).creation_flags_hidden().status().map(|s| s.success()).unwrap_or(false)
}

/// Logon task XML: elevated, no 72 h time limit, runs on battery.
fn task_xml() -> String {
    format!(r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>{user}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit><Priority>5</Priority>
  </Settings>
  <Actions Context="Author"><Exec><Command>{exe}</Command></Exec></Actions>
</Task>"#, user = std::env::var("USERNAME").unwrap_or_default(), exe = exe())
}

/// Make the startup entry match the settings. Creating/removing the elevated task shows one UAC prompt
/// unless we are already elevated.
pub fn apply_startup(on: bool, admin: bool) {
    let want_task = on && admin;
    set_run_key(on && !admin);
    let has_task = task_exists();
    if want_task {
        let xml = std::env::temp_dir().join("vmvolume-task.xml");
        let utf16: Vec<u8> = std::iter::once(0xFEFFu16).chain(task_xml().encode_utf16()).flat_map(|c| c.to_le_bytes()).collect();
        if std::fs::write(&xml, utf16).is_err() { return; }
        let args = format!("/Create /F /TN {TASK} /XML \"{}\"", xml.display());
        if is_admin() { let _ = std::process::Command::new("schtasks").raw_args(&args).creation_flags_hidden().status(); }
        else { shell(w!("runas"), "schtasks.exe", &args); }
    } else if has_task {
        let args = format!("/Delete /F /TN {TASK}");
        if is_admin() { let _ = std::process::Command::new("schtasks").raw_args(&args).creation_flags_hidden().status(); }
        else { shell(w!("runas"), "schtasks.exe", &args); }
    }
}

trait CmdExt { fn creation_flags_hidden(&mut self) -> &mut Self; fn raw_args(&mut self, a: &str) -> &mut Self; }
impl CmdExt for std::process::Command {
    fn creation_flags_hidden(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt;
        self.creation_flags(0x0800_0000).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()) // CREATE_NO_WINDOW
    }
    fn raw_args(&mut self, a: &str) -> &mut Self { use std::os::windows::process::CommandExt; self.raw_arg(a) }
}
