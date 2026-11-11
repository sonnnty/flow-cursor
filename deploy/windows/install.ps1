<#
.SYNOPSIS
  Installs the FlowCursor agent for the current Windows user and starts it at login.
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File install.ps1 -Token FC1-... -Hub 192.168.1.10
.PARAMETER Token
  Join token from the hub dashboard (only needed the first time).
.PARAMETER Hub
  Hub address (host or host:port). Omit to discover the hub with mDNS.
.PARAMETER Elevated
  Start the agent elevated at login through a scheduled task (needs an admin
  PowerShell). Lets FlowCursor control elevated windows; still cannot
  control UAC prompts or the lock screen (that needs a SYSTEM service).
#>
param(
  [string]$Token = "",
  [string]$Hub = "",
  [switch]$Elevated
)
$ErrorActionPreference = "Stop"
$src = Join-Path $PSScriptRoot "flow-agent.exe"
$dir = Join-Path $env:LOCALAPPDATA "Programs\FlowCursor"
$exe = Join-Path $dir "flow-agent.exe"
New-Item -ItemType Directory -Force -Path $dir | Out-Null
Get-Process flow-agent -ErrorAction SilentlyContinue | Stop-Process -Force
Copy-Item $src $exe -Force

if ($Token) {
  $joinArgs = @("join", $Token)
  if ($Hub) { $joinArgs += @("--hub", $Hub) }
  # flow-agent.exe is a GUI-subsystem program, so `& $exe` neither waits for it
  # nor sets $LASTEXITCODE; wait explicitly and read its exit code.
  $p = Start-Process -FilePath $exe -ArgumentList $joinArgs -Wait -PassThru -NoNewWindow
  if ($p.ExitCode -ne 0) { throw "flow-agent join failed (exit code $($p.ExitCode))" }
}

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
  [Security.Principal.WindowsBuiltInRole]::Administrator)
$runKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"
if ($Elevated) {
  if (-not $isAdmin) { throw "-Elevated needs an administrator PowerShell" }
  Remove-ItemProperty -Path $runKey -Name "FlowCursor" -ErrorAction SilentlyContinue
  $action = New-ScheduledTaskAction -Execute $exe -Argument "run"
  $trigger = New-ScheduledTaskTrigger -AtLogOn -User "$env:USERDOMAIN\$env:USERNAME"
  $principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType Interactive -RunLevel Highest
  $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 `
    -RestartInterval (New-TimeSpan -Minutes 1) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
  Register-ScheduledTask -TaskName "FlowCursor Agent" -Action $action -Trigger $trigger -Principal $principal `
    -Settings $settings -Force | Out-Null
  Start-ScheduledTask -TaskName "FlowCursor Agent"
} else {
  Set-ItemProperty -Path $runKey -Name "FlowCursor" -Value "`"$exe`" run"
  # Start it through Explorer, not as a child of this PowerShell: flow-agent
  # attaches to its parent's console to print CLI output, so a child of this
  # terminal would be killed when the terminal window is closed. With no
  # arguments flow-agent runs the agent.
  Start-Process -FilePath "explorer.exe" -ArgumentList "`"$exe`""
}

if ($isAdmin) {
  # Direct (P2P) connections from other agents arrive on UDP 7879.
  Get-NetFirewallRule -DisplayName "FlowCursor agent" -ErrorAction SilentlyContinue | Remove-NetFirewallRule
  New-NetFirewallRule -DisplayName "FlowCursor agent" -Direction Inbound -Program $exe -Protocol UDP `
    -Action Allow -Profile Private,Domain | Out-Null
} else {
  Write-Host "Not running as administrator: Windows may ask once to allow flow-agent through the firewall (choose Private networks)."
}
Write-Host "FlowCursor agent installed in $dir and started. Log: $env:LOCALAPPDATA\flowcursor\data\agent.log"
