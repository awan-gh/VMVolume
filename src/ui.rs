//! Settings window glue. Every edit is read back into Settings, saved, and applied immediately.
use crate::{settings::{gain_for, Settings}, sys, with_app, App};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::rc::Rc;

slint::include_modules!();

/// ComboBox index -> Win32 priority class.
const PRIORITY: [u32; 5] = [0, 0x20, 0x8000, 0x80, 0x100];

fn bools(v: Vec<bool>) -> ModelRc<bool> { ModelRc::from(Rc::new(VecModel::from(v))) }
fn read_bools(m: ModelRc<bool>) -> Vec<bool> { m.iter().collect() }

pub fn curve_preview(s: &Settings) -> String {
    [100, 75, 50, 25, 10].iter().map(|&p| format!("{p}% → {:.1} dB", gain_for(s, p as f32 / 100.0))).collect::<Vec<_>>().join("  ·  ")
}

pub fn open() {
    if let Some(Some(w)) = with_app(|a| a.settings_win.as_ref().map(|w| w.as_weak())) {
        if let Some(w) = w.upgrade() { let _ = w.show(); return; }
    }
    let w = SettingsWindow::new().expect("settings window");
    with_app(|a| fill(&w, a));

    let weak = w.as_weak();
    w.on_changed(move || { if let Some(w) = weak.upgrade() { with_app(|a| read_back(&w, a)); } });
    w.on_restart_as_admin(|| { if sys::relaunch_as_admin() { slint::quit_event_loop().ok(); } });
    w.on_check_update(|| { with_app(|a| crate::start_update_check(a, true)); });
    let weak = w.as_weak();
    w.on_reset_affinity(move || {
        if let Some(w) = weak.upgrade() {
            w.set_cores(bools(vec![false; sys::cpu_count()]));
            with_app(|a| read_back(&w, a));
        }
    });
    let weak = w.as_weak();
    w.on_engine_select_all(move |on| {
        if let Some(w) = weak.upgrade() {
            w.set_engine_buses(bools(vec![on; 5]));
            w.set_engine_strips(bools(vec![on; 5]));
            with_app(|a| read_back(&w, a));
        }
    });
    // Drop the window on close (after the callback returns) so its memory goes back.
    w.window().on_close_requested(|| {
        slint::Timer::single_shot(std::time::Duration::ZERO, || { with_app(|a| a.settings_win = None); crate::trim_memory(); });
        slint::CloseRequestResponse::HideWindow
    });
    let _ = w.show();
    with_app(|a| a.settings_win = Some(w));
}

fn fill(w: &SettingsWindow, a: &App) {
    let s = &a.s;
    w.set_version(env!("CARGO_PKG_VERSION").into());
    w.set_gain_min(s.gain_min);
    w.set_gain_max(s.gain_max);
    w.set_limit_0(s.limit_db_gain_to_0);
    w.set_linear(s.linear_volume_scale);
    w.set_sync_mute(s.sync_mute);
    w.set_mute_strips(bools((0..8).map(|i| s.mute_set().contains(&format!("Strip[{i}]"))).collect()));
    w.set_mute_buses(bools((0..8).map(|i| s.mute_set().contains(&format!("Bus[{i}]"))).collect()));
    w.set_block_spikes(s.block_volume_spikes);
    w.set_strips(bools((0..8).map(|i| s.targets.contains(&format!("Strip[{i}]"))).collect()));
    w.set_buses(bools((0..8).map(|i| s.targets.contains(&format!("Bus[{i}]"))).collect()));
    w.set_crackle_fix(s.crackle_fix);
    w.set_engine_buses(bools((0..5).map(|i| s.engine_restart_buses.get(i).copied().unwrap_or(false)).collect()));
    w.set_engine_strips(bools((0..5).map(|i| s.engine_restart_strips.get(i).copied().unwrap_or(false)).collect()));
    w.set_engine_on_disconnect(s.engine_restart_on_disconnect);
    w.set_priority(PRIORITY.iter().position(|&p| p == s.audiodg_priority).unwrap_or(0) as i32);
    let n = sys::cpu_count();
    w.set_core_labels(ModelRc::from(Rc::new(VecModel::from((0..n).map(|i| SharedString::from(format!("CPU {i}"))).collect::<Vec<_>>()))));
    w.set_cores(bools((0..n).map(|i| s.audiodg_affinity >> i & 1 == 1).collect()));
    w.set_is_admin(sys::is_admin());
    w.set_start_with_windows(s.start_with_windows);
    w.set_start_as_admin(s.start_as_admin);
    w.set_auto_update(s.auto_update);
    refresh(w, a);
}

/// Live text: connection, current knob position, audiodg and update status.
pub fn refresh(w: &SettingsWindow, a: &App) {
    w.set_status(a.status_line().into());
    w.set_curve_preview(curve_preview(&a.s).into());
    w.set_audiodg_status(a.audiodg_status.clone().into());
    w.set_update_status(a.update_status.clone().into());
}

fn s_crackle_turned_off(old: &Settings, new: &Settings) -> bool { old.crackle_fix && !new.crackle_fix }

fn read_back(w: &SettingsWindow, a: &mut App) {
    let old = a.s.clone();
    let mut s = old.clone();
    s.gain_min = w.get_gain_min().round();
    s.gain_max = w.get_gain_max().round().max(s.gain_min);
    s.limit_db_gain_to_0 = w.get_limit_0();
    s.linear_volume_scale = w.get_linear();
    s.sync_mute = w.get_sync_mute();
    let mute: Vec<String> = read_bools(w.get_mute_strips()).iter().enumerate().filter(|(_, &on)| on).map(|(i, _)| format!("Strip[{i}]"))
        .chain(read_bools(w.get_mute_buses()).iter().enumerate().filter(|(_, &on)| on).map(|(i, _)| format!("Bus[{i}]")))
        .collect();
    // keep following the volume targets until the mute list is actually made different
    s.mute_targets = if old.mute_targets.is_none() && mute == s.targets { None } else { Some(mute) };
    s.block_volume_spikes = w.get_block_spikes();
    s.targets = read_bools(w.get_strips()).iter().enumerate().filter(|(_, &on)| on).map(|(i, _)| format!("Strip[{i}]"))
        .chain(read_bools(w.get_buses()).iter().enumerate().filter(|(_, &on)| on).map(|(i, _)| format!("Bus[{i}]")))
        .collect();
    s.crackle_fix = w.get_crackle_fix();
    s.engine_restart_buses = read_bools(w.get_engine_buses());
    s.engine_restart_strips = read_bools(w.get_engine_strips());
    s.engine_restart_on_disconnect = w.get_engine_on_disconnect();
    s.audiodg_priority = PRIORITY[w.get_priority().clamp(0, 4) as usize];
    s.audiodg_affinity = read_bools(w.get_cores()).iter().enumerate().filter(|(_, &on)| on).fold(0u64, |m, (i, _)| m | 1 << i);
    s.start_with_windows = w.get_start_with_windows();
    s.start_as_admin = w.get_start_as_admin();
    s.auto_update = w.get_auto_update();
    if s == old { return; }

    if (s.start_with_windows, s.start_as_admin) != (old.start_with_windows, old.start_as_admin) {
        sys::apply_startup(s.start_with_windows, s.start_as_admin);
    }
    let adg_changed = (s.crackle_fix, s.audiodg_priority, s.audiodg_affinity) != (old.crackle_fix, old.audiodg_priority, old.audiodg_affinity);
    s.save();
    a.s = s;
    if s_crackle_turned_off(&old, &a.s) { a.reset_audiodg(); } else if adg_changed { a.apply_audiodg(); }
    a.sent = None; // targets or curve may have changed: push the current knob position again
    a.apply();
    refresh(w, a);
}
