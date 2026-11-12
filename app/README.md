# Brepsi Brain — desktop shell (Tauri 2)

A window shell for the local [Brepsi Brain](../README.md) dashboard (the
`memory-base` project): system tray, server autostart, desktop shortcut. The
dashboard itself (Rust HTTP server, SPA with embedded static files) is served by
the same exe.

## How it works

- The window immediately shows the embedded splash screen (`dist/index.html`), so the WebView error page never appears. The screen fonts live in `dist/fonts`.
- On a background thread: if nothing listens on `127.0.0.1:8765`, the shell starts `BrepsiBrain.exe serve --port 8765` as a hidden process and waits for the port for up to 60 seconds. When the port opens, the window switches to the dashboard. On failure the screen shows the reason and the launch is retried with a 2–30 second delay.
- Only one copy runs: a second launch (shortcut, autostart) just shows the already open window, including one hidden in the tray.
- If the port is already taken (for example, a manually started server), the shell just connects to it and does **not** stop that process on exit.
- The title-bar close button hides the window to the tray instead of quitting; the real exit is the "Quit" item in the tray.
- On exit only the server started by the shell is stopped (`taskkill /T`).

### Tray

The menu is built from `GET /api/tray` at startup, after sync and once a minute if the content changed: rebuilding an open menu would close it under the cursor.

- Top: status, record and source counts, last run and next run time.
- **Open database** — show the window (also on left click on the icon).
- **Sync now** — `POST /api/sync`; the result appears in the icon tooltip.
- **Recent cards** — opens a record in the window (`#record/<id>`).
- **Sources · N** — records and cards per source.
- **Settings** — "Model and provider…" (`#settings`), "Models and hardware…" (`#models`), "Run at sign-in" (`SecondBrain` value in `HKCU\…\Run`, changed only by the owner's click), "Open log", "Open database folder".
- **Stop the local model** — appears only while the local engine is running; calls `POST /api/local/stop` and frees VRAM.
- **Quit** — stop the server (if it is ours) and exit the process.

## Environment variables

| Variable | Purpose | Default |
|---|---|---|
| `BREPSI_BRAIN_ROOT` | Data/root folder | working directory if it holds `data/` or `config.toml`, else the folder next to the exe |

## Layout

```
app/
  package.json          # pnpm script "tauri", devDep @tauri-apps/cli
  dist/index.html       # splash (the window then loads the served URL)
  src-tauri/
    Cargo.toml          # crate brepsi_brain, binary BrepsiBrain
    tauri.conf.json     # window 1440x900 -> 127.0.0.1:8765, trayIcon, bundle off
    capabilities/       # default.json: core:default + core:event:default; window-controls.json: frameless window from the remote dashboard origin; app-commands.json: folder picker command from the same origin
    icons/              # icon.png (64x64) + icon.ico (PNG-in-ICO)
    src/main.rs         # all shell logic
  tools/
    make_shortcut.ps1   # creates the desktop shortcut
    smoke_test.ps1      # exe + /api/stats + port release smoke test
```

### Folder picker

The Models page asks the shell for a directory with the `pick_folder` Tauri
command (`rfd::FileDialog`). It is available only in the desktop window; in a
plain browser the page falls back to typing the path by hand.

## Rebuilding

```bash
cd app
pnpm install          # first time
pnpm tauri build      # release: src-tauri\target\release\BrepsiBrain.exe
```

The first build with a cold cargo cache is slow (10–25 min), later builds take minutes.

## Desktop shortcut

Created by the script (after a build):

```powershell
pwsh -NoProfile -File app\tools\make_shortcut.ps1
```

It writes `Brepsi Brain.lnk` to the desktop (`WScript.Shell`): target is the exe,
working directory is the project root (where `config.toml` and `data/` live),
icon `src-tauri\icons\icon.ico`.

## Troubleshooting

- **Port 8765 is taken / the app does not start.** Something already listens there:
  another `BrepsiBrain.exe` or a manually started server. Check with
  `netstat -ano | findstr :8765`. Stop all copies with
  `taskkill /F /IM BrepsiBrain.exe`, then stop the process holding the port with
  `taskkill /PID <pid> /F`.
- **The server survived a hard kill of the shell.** Kill the listener tree:
  `Get-Process BrepsiBrain | Stop-Process -Force`, then check
  `netstat -ano | findstr :8765`.
- **The window "does not close".** That is by design: the close button hides it to
  the tray (near the clock). Exit only through "Quit" in the tray menu.
- **The server did not come up within 60 s.** The shell silences the server's
  stdout/stderr. Run it manually to see the error:
  `app\src-tauri\target\release\BrepsiBrain.exe serve --port 8765`.
