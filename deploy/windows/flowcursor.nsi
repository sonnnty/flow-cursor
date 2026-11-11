; FlowCursor setup for Windows (NSIS 3, built on Linux by build/package.sh):
;   makensis -DVERSION=0.1.0 -DBINDIR=<dir with the exes> -DOUTFILE=<setup.exe> flowcursor.nsi
;
; Per-user install without administrator rights:
;   %LOCALAPPDATA%\Programs\FlowCursor   flow-agent.exe, flow-tray.exe, uninstall.exe
;   Start menu folder "FlowCursor", entry in Apps & features (HKCU),
;   autostart of the tray at sign-in (HKCU\...\Run), which starts the agent.
; Only the optional firewall rule needs administrator rights: it is added by
; a separate elevated netsh call (one UAC prompt); the installer itself and
; everything it starts stay unelevated.
;
; Silent install / scripted deployment:
;   FlowCursor-setup.exe /S [/TOKEN=FC1-...] [/HUB=192.168.1.10] [/FIREWALL]
;                           [/NOAUTOSTART] [/NORUN] [/D=C:\path (must be last)]
; Silent uninstall: "%LOCALAPPDATA%\Programs\FlowCursor\uninstall.exe" /S [/PURGE]
;
; Upgrades: the running tray and agent are stopped, files replaced, the
; tray started again. Configuration and identity live elsewhere
; (%APPDATA%\flowcursor, %LOCALAPPDATA%\flowcursor) and are kept.

Unicode true
ManifestDPIAware true

!ifndef VERSION
  !define VERSION "0.0.0"
!endif
!ifndef BINDIR
  !define BINDIR "."
!endif
!ifndef OUTFILE
  !define OUTFILE "FlowCursor-setup.exe"
!endif
!ifndef ICON
  !define ICON "..\icons\flowcursor.ico"
!endif

!define APP "FlowCursor"
!define REGKEY "Software\FlowCursor"
!define UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\FlowCursor"
!define RUNKEY "Software\Microsoft\Windows\CurrentVersion\Run"
!define FWRULE "FlowCursor agent"
; flow-agent keeps its state here (directories crate, Windows layout).
!define STATEFILE "$LOCALAPPDATA\flowcursor\data\agent-state.json"

Name "${APP}"
OutFile "${OUTFILE}"
RequestExecutionLevel user
InstallDir "$LOCALAPPDATA\Programs\FlowCursor"
InstallDirRegKey HKCU "${REGKEY}" "InstallDir"
SetCompressor /SOLID lzma
BrandingText "${APP} ${VERSION}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${APP}"
VIAddVersionKey "FileDescription" "${APP} setup"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "CompanyName" "${APP}"
VIAddVersionKey "LegalCopyright" "MIT License"

!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "Sections.nsh"
!include "FileFunc.nsh"
!include "nsDialogs.nsh"
!include "WinMessages.nsh"

!define MUI_ICON "${ICON}"
!define MUI_UNICON "${ICON}"
!define MUI_ABORTWARNING
!define MUI_COMPONENTSPAGE_NODESC

Var Token
Var Hub
Var NoRun
Var TokenField
Var HubField

!define MUI_WELCOMEPAGE_TEXT "This installs ${APP} ${VERSION} for your Windows account (no administrator rights needed).$\r$\n$\r$\n${APP} shares this PC's mouse, keyboard and clipboard with your other computers through the FlowCursor hub on your home server.$\r$\n$\r$\nIf ${APP} is already installed it is updated; your settings are kept."
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
Page custom JoinPage JoinPageLeave
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_TEXT "Start ${APP} now"
!define MUI_FINISHPAGE_RUN_FUNCTION LaunchInteractive
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_COMPONENTS
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; ---------------------------------------------------------------------------
; helpers (shared by installer and uninstaller)
; ---------------------------------------------------------------------------

!macro StopFlowCursor un
Function ${un}StopFlowCursor
  DetailPrint "Stopping ${APP} if it is running..."
  ; Close the tray (WM_CLOSE to its hidden window), stop the agent over its
  ; pipe, then make sure (older versions, hung processes): only this
  ; user's processes.
  FindWindow $0 "FlowCursorTray"
  ${If} $0 <> 0
    SendMessage $0 ${WM_CLOSE} 0 0 /TIMEOUT=3000
  ${EndIf}
  ${If} ${FileExists} "$INSTDIR\flow-agent.exe"
    nsExec::Exec '"$INSTDIR\flow-agent.exe" stop'
    Pop $0
  ${EndIf}
  ReadEnvStr $1 "USERNAME"
  nsExec::Exec 'taskkill /F /FI "USERNAME eq $1" /IM flow-tray.exe'
  Pop $0
  nsExec::Exec 'taskkill /F /FI "USERNAME eq $1" /IM flow-agent.exe'
  Pop $0
  ; Wait until the files can be replaced.
  StrCpy $2 0
  ${Do}
    ClearErrors
    ${If} ${FileExists} "$INSTDIR\flow-agent.exe"
      FileOpen $3 "$INSTDIR\flow-agent.exe" a
      ${IfNot} ${Errors}
        FileClose $3
        ${Break}
      ${EndIf}
    ${Else}
      ${Break}
    ${EndIf}
    Sleep 250
    IntOp $2 $2 + 1
  ${LoopUntil} $2 >= 40
FunctionEnd
!macroend
!insertmacro StopFlowCursor ""
!insertmacro StopFlowCursor "un."

; Runs `cmd /c <command>` elevated (one UAC prompt) unless this process
; already is.
!macro Elevated un
Function ${un}Elevated
  Pop $0 ; command for cmd /c
  UserInfo::GetAccountType
  Pop $1
  ${If} $1 == "Admin"
    nsExec::Exec '"$SYSDIR\cmd.exe" /c $0'
    Pop $1
  ${Else}
    ExecShellWait "runas" "$SYSDIR\cmd.exe" "/c $0" SW_HIDE
  ${EndIf}
FunctionEnd
!macroend
!insertmacro Elevated ""
!insertmacro Elevated "un."

; ---------------------------------------------------------------------------
; install
; ---------------------------------------------------------------------------

Section "${APP} (required)" SecMain
  SectionIn RO
  Call StopFlowCursor
  SetOutPath "$INSTDIR"
  SetOverwrite on
  File "${BINDIR}\flow-agent.exe"
  File "${BINDIR}\flow-tray.exe"
  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKCU "${REGKEY}" "InstallDir" "$INSTDIR"

  CreateDirectory "$SMPROGRAMS\${APP}"
  CreateShortCut "$SMPROGRAMS\${APP}\${APP}.lnk" "$INSTDIR\flow-tray.exe" "--show" "$INSTDIR\flow-tray.exe" 0 \
    SW_SHOWNORMAL "" "Status of FlowCursor (mouse and keyboard sharing)"
  CreateShortCut "$SMPROGRAMS\${APP}\Uninstall ${APP}.lnk" "$INSTDIR\uninstall.exe"

  WriteRegStr HKCU "${UNINSTKEY}" "DisplayName" "${APP}"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTKEY}" "Publisher" "${APP}"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayIcon" "$INSTDIR\flow-tray.exe,0"
  WriteRegStr HKCU "${UNINSTKEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTKEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNINSTKEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${UNINSTKEY}" "EstimatedSize" "$0"

  ; Join the hub (the agent is stopped, so `join` writes the token itself;
  ; the agent uses it when it starts).
  ${If} $Token != ""
    DetailPrint "Storing the join token..."
    ${If} $Hub != ""
      nsExec::ExecToStack '"$INSTDIR\flow-agent.exe" join "$Token" --hub "$Hub"'
    ${Else}
      nsExec::ExecToStack '"$INSTDIR\flow-agent.exe" join "$Token"'
    ${EndIf}
    Pop $0
    Pop $1
    ${If} $0 != 0
      DetailPrint "flow-agent join failed ($0): $1"
      MessageBox MB_ICONEXCLAMATION|MB_OK "Joining the hub failed:$\r$\n$1$\r$\nYou can enter the token later in the FlowCursor window." /SD IDOK
      SetErrorLevel 2
    ${EndIf}
  ${EndIf}
SectionEnd

Section "Start ${APP} when I sign in" SecAutostart
  ; The tray starts at sign-in and starts the agent. An agent installed to
  ; run elevated by install.ps1 -Elevated (scheduled task) must not get a
  ; second, unelevated copy from the tray.
  StrCpy $0 ""
  nsExec::Exec 'schtasks /Query /TN "FlowCursor Agent"'
  Pop $1
  ${If} $1 == 0
    StrCpy $0 " --no-start-agent"
  ${EndIf}
  WriteRegStr HKCU "${RUNKEY}" "${APP}" '"$INSTDIR\flow-tray.exe"$0'
SectionEnd

Section "Allow direct connections in Windows Firewall (asks for administrator rights)" SecFirewall
  ; Direct (P2P) connections from the other machines arrive on UDP 7879.
  ; Same rule name as install.ps1, so both installers replace each other's.
  DetailPrint "Adding the Windows Firewall rule (UAC prompt)..."
  Push 'netsh advfirewall firewall delete rule name="${FWRULE}" & netsh advfirewall firewall add rule name="${FWRULE}" dir=in action=allow program="$INSTDIR\flow-agent.exe" protocol=UDP localport=7879 profile=private,domain'
  Call Elevated
  nsExec::Exec 'netsh advfirewall firewall show rule name="${FWRULE}"'
  Pop $0
  ${If} $0 == 0
    WriteRegDWORD HKCU "${REGKEY}" "FirewallRule" 1
  ${Else}
    DetailPrint "The firewall rule was not added; Windows may ask once when FlowCursor first starts."
  ${EndIf}
SectionEnd

Section "-Launch"
  ; Interactive installs start FlowCursor from the finish page.
  ${If} ${Silent}
  ${AndIf} $NoRun != 1
    Call LaunchSilent
  ${EndIf}
SectionEnd

; Start through Explorer: the tray (and the agent it starts) then runs as
; the signed-in user, unelevated, and belongs to no console or installer.
Function LaunchInteractive
  Exec '"$WINDIR\explorer.exe" "$SMPROGRAMS\${APP}\${APP}.lnk"'
FunctionEnd

Function LaunchSilent
  Exec '"$WINDIR\explorer.exe" "$INSTDIR\flow-tray.exe"'
FunctionEnd

Function JoinPage
  ${If} $Token != ""
    Abort ; given on the command line
  ${EndIf}
  !insertmacro MUI_HEADER_TEXT "Join your FlowCursor hub" "Connect this PC to the hub on your home server."
  nsDialogs::Create 1018
  Pop $0
  ${If} ${FileExists} "${STATEFILE}"
    ${NSD_CreateLabel} 0 0 100% 30u "This PC has used a FlowCursor hub before. Leave the fields empty to keep it, or enter a new join token to join again."
  ${Else}
    ${NSD_CreateLabel} 0 0 100% 30u "Copy the join token from the hub dashboard (http://<server>:7880, Overview, Join token). You can also leave this empty and enter it later in the FlowCursor window."
  ${EndIf}
  Pop $0
  ${NSD_CreateLabel} 0 36u 100% 10u "Join token (FC1-...)"
  Pop $0
  ${NSD_CreateText} 0 48u 100% 13u "$Token"
  Pop $TokenField
  ${NSD_CreateLabel} 0 70u 100% 10u "Hub address, e.g. 192.168.1.10 (empty: find the hub on the network automatically)"
  Pop $0
  ${NSD_CreateText} 0 82u 100% 13u "$Hub"
  Pop $HubField
  nsDialogs::Show
FunctionEnd

Function JoinPageLeave
  ${NSD_GetText} $TokenField $Token
  ${NSD_GetText} $HubField $Hub
  ${If} $Token != ""
    StrCpy $0 $Token 4
    ${If} $0 != "FC1-"
      MessageBox MB_ICONEXCLAMATION|MB_OK "That does not look like a FlowCursor join token (it starts with FC1-)."
      Abort
    ${EndIf}
  ${EndIf}
FunctionEnd

Function .onInit
  ${GetParameters} $R0
  ClearErrors
  ${GetOptions} $R0 "/TOKEN=" $Token
  ClearErrors
  ${GetOptions} $R0 "/HUB=" $Hub
  ClearErrors
  ${GetOptions} $R0 "/NORUN" $0
  ${IfNot} ${Errors}
    StrCpy $NoRun 1
  ${EndIf}
  ClearErrors
  ${GetOptions} $R0 "/NOAUTOSTART" $0
  ${IfNot} ${Errors}
    !insertmacro UnselectSection ${SecAutostart}
  ${EndIf}
  ; The firewall step means a UAC prompt: in silent installs only on
  ; request; interactively preselected unless a rule was added before.
  ClearErrors
  ${GetOptions} $R0 "/FIREWALL" $0
  ${If} ${Errors}
    ReadRegDWORD $1 HKCU "${REGKEY}" "FirewallRule"
    ${If} ${Silent}
    ${OrIf} $1 == 1
      !insertmacro UnselectSection ${SecFirewall}
    ${EndIf}
  ${EndIf}
FunctionEnd

; ---------------------------------------------------------------------------
; uninstall
; ---------------------------------------------------------------------------

Section "un.${APP}" UnSecMain
  SectionIn RO
  Call un.StopFlowCursor
  DeleteRegValue HKCU "${RUNKEY}" "${APP}"
  ReadRegDWORD $0 HKCU "${REGKEY}" "FirewallRule"
  ${If} $0 == 1
    UserInfo::GetAccountType
    Pop $1
    ${IfNot} ${Silent}
    ${OrIf} $1 == "Admin"
      Push 'netsh advfirewall firewall delete rule name="${FWRULE}"'
      Call un.Elevated
    ${EndIf}
  ${EndIf}
  Delete "$INSTDIR\flow-agent.exe"
  Delete "$INSTDIR\flow-tray.exe"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\${APP}\${APP}.lnk"
  Delete "$SMPROGRAMS\${APP}\Uninstall ${APP}.lnk"
  RMDir "$SMPROGRAMS\${APP}"
  DeleteRegKey HKCU "${UNINSTKEY}"
  DeleteRegKey HKCU "${REGKEY}"
SectionEnd

Section /o "un.Also delete settings, identity and logs" UnSecPurge
  RMDir /r "$APPDATA\flowcursor"
  RMDir /r "$LOCALAPPDATA\flowcursor"
SectionEnd

Function un.onInit
  ${GetParameters} $R0
  ClearErrors
  ${GetOptions} $R0 "/PURGE" $0
  ${IfNot} ${Errors}
    !insertmacro SelectSection ${UnSecPurge}
  ${EndIf}
FunctionEnd
