<#
.SYNOPSIS
  Removes FlowCursor installed by install.ps1 for the current user. -Purge also deletes its
  identity and state. (Installed with the setup exe? Use Apps & features instead.)
#>
param([switch]$Purge)
$ErrorActionPreference = "Continue"
$dir = Join-Path $env:LOCALAPPDATA "Programs\FlowCursor"
$tray = Join-Path $dir "flow-tray.exe"
$exe = Join-Path $dir "flow-agent.exe"
if (Test-Path $tray) { Start-Process -FilePath $tray -ArgumentList "--quit" -Wait -ErrorAction SilentlyContinue }
if (Test-Path $exe) { Start-Process -FilePath $exe -ArgumentList "stop" -Wait -WindowStyle Hidden -ErrorAction SilentlyContinue }
Get-Process flow-tray, flow-agent -ErrorAction SilentlyContinue | Stop-Process -Force
Remove-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -Name "FlowCursor" -ErrorAction SilentlyContinue
Unregister-ScheduledTask -TaskName "FlowCursor Agent" -Confirm:$false -ErrorAction SilentlyContinue
Get-NetFirewallRule -DisplayName "FlowCursor agent" -ErrorAction SilentlyContinue | Remove-NetFirewallRule
Remove-Item -Recurse -Force (Join-Path ([Environment]::GetFolderPath("Programs")) "FlowCursor") -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500
Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
if ($Purge) {
  Remove-Item -Recurse -Force (Join-Path $env:APPDATA "flowcursor") -ErrorAction SilentlyContinue
  Remove-Item -Recurse -Force (Join-Path $env:LOCALAPPDATA "flowcursor") -ErrorAction SilentlyContinue
}
Write-Host "FlowCursor removed."
