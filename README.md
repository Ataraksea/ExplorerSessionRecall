# <img src="assets/icon-256.png" alt="" width="96" height="96" align="middle"> Explorer Session Recall

Your File Explorer windows and tabs, back where you left them. Plus: folders open as tabs instead of piling up new windows.

[![Release](https://github.com/Ataraksea/ExplorerSessionRecall/actions/workflows/release.yml/badge.svg)](https://github.com/Ataraksea/ExplorerSessionRecall/actions/workflows/release.yml)
[![CI](https://github.com/Ataraksea/ExplorerSessionRecall/actions/workflows/ci.yml/badge.svg)](https://github.com/Ataraksea/ExplorerSessionRecall/actions/workflows/ci.yml)

## What it does

Explorer Session Recall is a small app that runs quietly in the background and brings two browser-style features to Windows 11 File Explorer.

**Session restore, like Chrome's "Continue where you left off."**

- When you close your last File Explorer window, sign out, restart, or File Explorer or Windows crashes, it remembers the windows and tabs that were open.
- The next time you open File Explorer, those windows come back: the same folders, the same tab order, the same active tab, and the same window size and position. There are no prompts.
- If you open File Explorer to a specific folder (for example from a shortcut), your session is restored and that folder is added as a tab.
- As in Chrome, closing one window while others stay open removes that window from the session. Only the windows open at the end are restored.

**Tab folding.**

- A folder that would open in a new File Explorer window, for example from a desktop double-click, "Show in folder" in another app, a shortcut, or Ctrl+N, opens as a new tab in your first File Explorer window on that virtual desktop instead.
- If that folder is already open in a tab, that tab is brought to the front.
- When you want a separate window, drag a tab out of the window, or hold **Ctrl+Shift** while you open the folder.

## Requirements

Windows 11 22H2 or later (build 22621.675 or newer, the first build with File Explorer tabs), 64-bit (x64).

## Download and install

1. Go to the [latest release](https://github.com/Ataraksea/ExplorerSessionRecall/releases/latest).
2. Download **ExplorerSessionRecall-Setup-x.y.z.exe** and run it.

It installs for your user account only. You don't need administrator rights, and there is no UAC prompt. When Setup finishes, the app is already running in the background.

Releases are not code-signed yet (see [Code signing policy](#code-signing-policy)), so Windows SmartScreen may show "Windows protected your PC". Click **More info**, then **Run anyway**.

**Upgrading from the old "Explorer Session Restore" build?** Just run Setup. It stops the old version, removes it, and moves your saved session and settings over automatically.

**Prefer no installer?** Download `esr-x.y.z-x64.exe` from the same release and run `esr-x.y.z-x64.exe --install` in a terminal. It does exactly what Setup does.

## Using it

There is nothing to do: the app has no window and starts automatically when you sign in.

To open **Settings**, start **Explorer Session Recall** from the Start menu again. Settings has these options:

- **Restore my File Explorer session**: turn session restore on or off.
- **Start when I sign in**: start the app automatically when you sign in to Windows.
- **Open folders as tabs in my File Explorer window**: turn tab folding on or off.
- **Exit Explorer Session Recall**: stop the app until you start it again or sign in again. Your File Explorer windows stay open.

### Windows' own "Restore previous folder windows at logon"

File Explorer has its own option that reopens folder windows at sign-in. Using both would give you duplicate windows, so when that option is on, the app asks once whether to turn it off for you. If you say no, it won't ask again, unless you click **Ask me about it again** in Settings.

## Uninstall

Open Windows **Settings → Apps → Installed apps**, find **Explorer Session Recall**, and choose **Uninstall**.

This removes the program, the Start menu shortcut, start at sign-in, and all of its data (saved session, settings, and logs). Any File Explorer windows you have open stay open.

## Privacy

Everything stays on your computer. The saved session, settings, and activity log are kept in `%LOCALAPPDATA%\ExplorerSessionRecall`, readable only by your account. The app contains no network code. It never sends anything anywhere and never checks for updates.

## Troubleshooting

- **What happened?** The app keeps a short activity log at `%LOCALAPPDATA%\ExplorerSessionRecall\activity.log`; the previous log is `activity.previous.log` in the same folder. Paste `%LOCALAPPDATA%\ExplorerSessionRecall` into the File Explorer address bar to open the folder.
- **Restart the app.** Open Settings (start Explorer Session Recall from the Start menu), click **Exit Explorer Session Recall**, then start it from the Start menu again.
- **Setup reported an error.** Exit the app from its Settings if it is running and run Setup again. Error code 2 means your version of Windows is not supported (see [Requirements](#requirements)).
- **Setup finished "with warnings".** The app is installed, but a step listed in the message didn't finish (for example the Start menu shortcut). If moving your saved session from the old build failed, the message names the old folder; copy `session.esr` and `settings.ini` from there into `%LOCALAPPDATA%\ExplorerSessionRecall` after exiting the app from its Settings.
- **Still stuck?** [Open an issue](https://github.com/Ataraksea/ExplorerSessionRecall/issues) and attach the activity log if you can. It lists folder names, so check it first if those are private.

## Known limitations

- The app relies on File Explorer behaviour that Microsoft does not document. A Windows update can break it; if that happens, please [open an issue](https://github.com/Ataraksea/ExplorerSessionRecall/issues).
- Windows 11 only. Windows 10 File Explorer has no tabs.
- A restored tab opens on its folder, but without its Back/Forward history or the files that were selected.

## Building from source

You need the Rust toolchain for Windows with the MSVC target (`rustup` default on Windows, plus the Visual Studio C++ build tools and Windows SDK).

```powershell
cargo build --release -p esr-app --bin esr
```

The program is `target\release\esr.exe`. To build the installer as well, install [Inno Setup 6](https://jrsoftware.org/isinfo.php) and run:

```powershell
powershell -ExecutionPolicy Bypass -File installer\build.ps1
```

This writes `ExplorerSessionRecall-Setup-<version>.exe`, `esr-<version>-x64.exe`, and `SHA256SUMS.txt` to `dist\`.

The icon's source is `assets\icon.svg`. `assets\icon.ico` and `assets\icon-256.png` are generated from it by `powershell -ExecutionPolicy Bypass -File assets\make-icon.ps1` and committed, so the build needs no image tools.

## Code signing policy

Code signing for this project is being applied for with SignPath Foundation; until then, releases are unsigned.

Free code signing provided by [SignPath.io](https://signpath.io), certificate by [SignPath Foundation](https://signpath.org).

- Committers and reviewers: [Ataraksea](https://github.com/Ataraksea)
- Approvers: [Ataraksea](https://github.com/Ataraksea)

Privacy: This program will not transfer any information to other networked systems unless specifically requested by the user or the person installing or operating it.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache License, Version 2.0](LICENSE-APACHE), at your option.

---

Not affiliated with or endorsed by Microsoft. Windows and File Explorer are trademarks of Microsoft.
