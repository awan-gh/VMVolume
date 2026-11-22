# VMVolume

Make your keyboard's volume knob (or any Windows volume control) drive [Voicemeeter](https://vb-audio.com/Voicemeeter/).

VMVolume watches the Windows volume and mute of your default playback device and mirrors them onto the
Voicemeeter strips and buses you pick. It is a small Rust rewrite of
[Voicemeeter Windows Volume](https://github.com/Frosthaven/voicemeeter-windows-volume) and its
[Modern](https://github.com/heide-oficial/Voicemeeter-Windows-Volume-Modern) fork.

- **One exe, no installer.** Download `VMVolume.exe` from [Releases](../../releases) and run it. It lives in the tray.
- **Event driven.** Reacts to Windows volume callbacks instead of polling, and sends one batched command to Voicemeeter per change.
- **Same volume curve** as Voicemeeter Windows Volume (logarithmic by default, linear optional), with a 0 dB cap option.
- **Imports your Voicemeeter Windows Volume settings** on first run.
- **Sync mute**, and blocks the sudden jump to 100% Windows sometimes does when a device connects.
- **audiodg.exe priority and CPU affinity** (needs administrator; can start elevated at logon via a scheduled task).
- **Start with Windows** and **automatic updates** from GitHub Releases (the exe replaces itself).

## Use

1. Run `VMVolume.exe`. Put it anywhere you like (for example `%LOCALAPPDATA%\Programs\VMVolume`); updates replace it in place.
2. Click the tray icon to open the settings and tick the strips and buses the knob should control.
3. Set your Windows default playback device to a Voicemeeter input, as usual.

Settings are stored in `%APPDATA%\VMVolume\settings.json`.

## Build

```powershell
cargo build --release   # target\release\VMVolume.exe
cargo test
```

Requires Windows and Voicemeeter (Standard, Banana or Potato). The Voicemeeter remote API DLL is loaded from the
Voicemeeter install folder at runtime.

## License

MIT. Voicemeeter is a product of VB-Audio; this project is not affiliated with VB-Audio.
