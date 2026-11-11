<#
.SYNOPSIS
  Removes the FlowCursor agent of the current user. -Purge also deletes its identity and state.
#>
param([switch]$Purge)
$ErrorActionPreference = "Continue"
Get-Process flow-agent -ErrorAction SilentlyContinue | Stop-Process -Force
Remove-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -Name "FlowCursor" -ErrorAction SilentlyContinue
Unregister-ScheduledTask -TaskName "FlowCursor Agent" -Confirm:$false -ErrorAction SilentlyContinue
Get-NetFirewallRule -DisplayName "FlowCursor agent" -ErrorAction SilentlyContinue | Remove-NetFirewallRule
Remove-Item -Recurse -Force (Join-Path $env:LOCALAPPDATA "Programs\FlowCursor") -ErrorAction SilentlyContinue
if ($Purge) {
  Remove-Item -Recurse -Force (Join-Path $env:APPDATA "flowcursor") -ErrorAction SilentlyContinue
  Remove-Item -Recurse -Force (Join-Path $env:LOCALAPPDATA "flowcursor") -ErrorAction SilentlyContinue
}
Write-Host "FlowCursor agent removed."
