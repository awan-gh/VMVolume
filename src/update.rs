//! Self-update from GitHub Releases. The release asset is the bare VMVolume.exe: download next to the
//! running exe, swap names (Windows lets a running exe be renamed), relaunch, quit.
use std::{io::Read, path::PathBuf};

pub const REPO: &str = "awan-gh/VMVolume";
const ASSET: &str = "VMVolume.exe";

pub struct Release { pub version: String, pub url: String }

fn parse_ver(s: &str) -> Option<(u32, u32, u32)> {
    let mut it = s.trim_start_matches('v').split(['.', '-']).map(|p| p.parse::<u32>().ok());
    Some((it.next()??, it.next()??, it.next().flatten().unwrap_or(0)))
}

pub fn is_newer(remote: &str, local: &str) -> bool {
    matches!((parse_ver(remote), parse_ver(local)), (Some(r), Some(l)) if r > l)
}

/// Latest release if it is newer than this build.
pub fn check() -> Result<Option<Release>, String> {
    let body = ureq::get(&format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .set("User-Agent", "VMVolume-updater")
        .set("Accept", "application/vnd.github+json")
        .timeout(std::time::Duration::from_secs(15))
        .call().map_err(|e| match e { ureq::Error::Status(404, _) => "no releases published yet".to_string(), e => e.to_string() })?
        .into_string().map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let tag = v["tag_name"].as_str().ok_or("release has no tag")?.to_string();
    if !is_newer(&tag, env!("CARGO_PKG_VERSION")) { return Ok(None); }
    let url = v["assets"].as_array().into_iter().flatten()
        .find(|a| a["name"].as_str() == Some(ASSET))
        .and_then(|a| a["browser_download_url"].as_str())
        .ok_or("release has no VMVolume.exe")?.to_string();
    Ok(Some(Release { version: tag.trim_start_matches('v').to_string(), url }))
}

fn paths() -> (PathBuf, PathBuf, PathBuf) {
    let exe = std::env::current_exe().unwrap_or_default();
    (exe.with_file_name("VMVolume.new.exe"), exe.with_file_name("VMVolume.old.exe"), exe)
}

/// Download and swap the exe in place. After Ok, call sys::relaunch() and quit.
pub fn install(r: &Release) -> Result<(), String> {
    let (new, old, exe) = paths();
    let resp = ureq::get(&r.url).set("User-Agent", "VMVolume-updater").timeout(std::time::Duration::from_secs(120)).call().map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    resp.into_reader().take(64 << 20).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() < 100_000 || &bytes[..2] != b"MZ" { return Err("download is not a valid exe".into()); }
    std::fs::write(&new, &bytes).map_err(|e| format!("cannot write next to the exe: {e}"))?;
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).map_err(|e| e.to_string())?;
    if let Err(e) = std::fs::rename(&new, &exe) { let _ = std::fs::rename(&old, &exe); return Err(e.to_string()); }
    Ok(())
}

/// Remove leftovers from a previous update.
pub fn cleanup() {
    let (new, old, _) = paths();
    let _ = std::fs::remove_file(old);
    let _ = std::fs::remove_file(new);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn versions() {
        assert!(is_newer("v0.2.1", "0.2.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("v0.2.0", "0.2.0"));
        assert!(!is_newer("v0.1.9", "0.2.0"));
        assert!(!is_newer("garbage", "0.2.0"));
    }
}
