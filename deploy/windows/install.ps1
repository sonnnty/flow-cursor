<#
.SYNOPSIS
  Installs FlowCursor (agent + tray icon) for the current Windows user and starts it at sign-in.
  The setup exe (FlowCursor-setup-<version>.exe, also silent: /S /TOKEN=... /HUB=...) is the
  normal way; this script is for scripted installs and for -Elevated.
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File install.ps1 -Token FC1-... -Hub 192.168.1.10
.PARAMETER Token
  Join token from the hub dashboard (only needed the first time).
.PARAMETER Hub
  Hub address (host or host:port). Omit to discover the hub with mDNS.
.PARAMETER Elevated
  Start the agent elevated at sign-in through a scheduled task (needs an admin
  PowerShell). Lets FlowCursor control elevated windows; still cannot
  control UAC prompts or the lock screen (that needs a SYSTEM service).
  The tray then only shows the agent and never starts a second copy.
#>
param(
  [string]$Token = "",
  [string]$Hub = "",
  [switch]$Elevated
)
$ErrorActionPreference = "Stop"
$dir = Join-Path $env:LOCALAPPDATA "Programs\FlowCursor"
$exe = Join-Path $dir "flow-agent.exe"
$tray = Join-Path $dir "flow-tray.exe"
New-Item -ItemType Directory -Force -Path $dir | Out-Null

# Stop what runs: the tray (window message), the agent (over its pipe), then
# anything left of this user's processes (older versions).
if (Test-Path $tray) { Start-Process -FilePath $tray -ArgumentList "--quit" -Wait -ErrorAction SilentlyContinue }
if (Test-Path $exe) { Start-Process -FilePath $exe -ArgumentList "stop" -Wait -WindowStyle Hidden -ErrorAction SilentlyContinue }
Get-Process flow-tray, flow-agent -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500
Copy-Item (Join-Path $PSScriptRoot "flow-agent.exe") $exe -Force
Copy-Item (Join-Path $PSScriptRoot "flow-tray.exe") $tray -Force

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
$trayArgs = "--show"
if ($Elevated) {
  if (-not $isAdmin) { throw "-Elevated needs an administrator PowerShell" }
  $action = New-ScheduledTaskAction -Execute $exe -Argument "run"
  $trigger = New-ScheduledTaskTrigger -AtLogOn -User "$env:USERDOMAIN\$env:USERNAME"
  $principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType Interactive -RunLevel Highest
  $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 `
    -RestartInterval (New-TimeSpan -Minutes 1) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
  Register-ScheduledTask -TaskName "FlowCursor Agent" -Action $action -Trigger $trigger -Principal $principal `
    -Settings $settings -Force | Out-Null
  Start-ScheduledTask -TaskName "FlowCursor Agent"
  $trayArgs = "--no-start-agent --show"
  Set-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -Name "FlowCursor" -Value "`"$tray`" --no-start-agent"
} else {
  # The tray starts at sign-in and starts the agent (detached from any console).
  Set-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -Name "FlowCursor" -Value "`"$tray`""
}

# Start menu entry (also used below to start the tray through Explorer).
$programs = Join-Path ([Environment]::GetFolderPath("Programs")) "FlowCursor"
New-Item -ItemType Directory -Force -Path $programs | Out-Null
$lnk = Join-Path $programs "FlowCursor.lnk"
$s = (New-Object -ComObject WScript.Shell).CreateShortcut($lnk)
$s.TargetPath = $tray
$s.Arguments = $trayArgs
$s.IconLocation = "$tray,0"
$s.Description = "Status of FlowCursor (mouse and keyboard sharing)"
$s.Save()

# Start through Explorer, not as a child of this PowerShell: a child of this
# terminal would belong to its console and die when the window is closed,
# and from an admin PowerShell it would run elevated.
Start-Process -FilePath "explorer.exe" -ArgumentList "`"$lnk`""

if ($isAdmin) {
  # Direct (P2P) connections from other agents arrive on UDP 7879.
  Get-NetFirewallRule -DisplayName "FlowCursor agent" -ErrorAction SilentlyContinue | Remove-NetFirewallRule
  New-NetFirewallRule -DisplayName "FlowCursor agent" -Direction Inbound -Program $exe -Protocol UDP `
    -LocalPort 7879 -Action Allow -Profile Private,Domain | Out-Null
} else {
  Write-Host "Not running as administrator: Windows may ask once to allow flow-agent through the firewall (choose Private networks)."
}
Write-Host "FlowCursor installed in $dir and started. Log: $env:LOCALAPPDATA\flowcursor\data\agent.log"
