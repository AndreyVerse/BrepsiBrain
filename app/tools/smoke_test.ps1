# Smoke test for the Brepsi Brain desktop shell.
# Starts the release exe, asserts the process stays alive and GET /api/stats
# answers 200, then force-stops the app and reaps the spawned server tree so
# port 8765 is free again for the user's first real launch.
# Usage: pwsh -NoProfile -File app\tools\smoke_test.ps1
$ErrorActionPreference = 'Stop'

$exe = Join-Path $PSScriptRoot '..\src-tauri\target\release\BrepsiBrain.exe'
$statsUrl = 'http://127.0.0.1:8765/api/stats'

Write-Host "starting $exe"
$proc = Start-Process -FilePath $exe -PassThru
Write-Host "pid $($proc.Id)"
Start-Sleep -Seconds 20

$alive = Get-Process -Id $proc.Id -ErrorAction SilentlyContinue
if (-not $alive) { Write-Host 'FAIL: process exited within 20 s'; exit 1 }
Write-Host 'OK: process alive after 20 s'

try {
  $resp = Invoke-WebRequest -Uri $statsUrl -UseBasicParsing -TimeoutSec 15
  $code = [int]$resp.StatusCode
} catch {
  Write-Host "FAIL: /api/stats request error: $($_.Exception.Message)"
  exit 1
}
Write-Host "GET /api/stats -> HTTP $code"
if ($code -ne 200) { exit 1 }

# Stop the app. /F is required: a graceful WM_CLOSE is swallowed by the
# hide-to-tray close handler by design.
taskkill /F /IM BrepsiBrain.exe 2>$null | Out-Null
Start-Sleep -Seconds 1

# Reap the server child that may have outlived the shell: it is the same
# BrepsiBrain.exe with `serve --port 8765` on its command line.
$ours = Get-CimInstance Win32_Process -Filter "Name='BrepsiBrain.exe'" |
  Where-Object { $_.CommandLine -like '*serve*8765*' }
foreach ($p in $ours) {
  Write-Host "taskkill /PID $($p.ProcessId) /T /F"
  taskkill /PID $p.ProcessId /T /F 2>$null | Out-Null
}
Start-Sleep -Seconds 2

$left = Get-NetTCPConnection -LocalPort 8765 -State Listen -ErrorAction SilentlyContinue
if ($left) { Write-Host 'FAIL: port 8765 is still busy'; exit 1 }
Write-Host 'SMOKE OK: app stopped, port 8765 free'
