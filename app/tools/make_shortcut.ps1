# Creates the desktop shortcut for the Brepsi Brain shell.
# Usage: pwsh -NoProfile -File app\tools\make_shortcut.ps1
$ErrorActionPreference = 'Stop'

$release = Join-Path $PSScriptRoot '..\src-tauri\target\release'
$exe = Join-Path $release 'BrepsiBrain.exe'
if (-not (Test-Path $exe)) { throw "exe not found (build first): $exe" }
# Рабочая папка — корень данных (config.toml и data), а не папка сборки.
$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path

$desktop = [Environment]::GetFolderPath('Desktop')
$lnkPath = Join-Path $desktop 'Brepsi Brain.lnk'

$ws = New-Object -ComObject WScript.Shell
$lnk = $ws.CreateShortcut($lnkPath)
$lnk.TargetPath = $exe
$lnk.WorkingDirectory = $root
$lnk.IconLocation = (Resolve-Path (Join-Path $PSScriptRoot '..\src-tauri\icons\icon.ico')).Path
$lnk.Description = 'Brepsi Brain — локальная память для ИИ-агентов'
$lnk.Save()

if (-not (Test-Path $lnkPath)) { throw 'shortcut was not created' }
Write-Host "shortcut created: $lnkPath"
