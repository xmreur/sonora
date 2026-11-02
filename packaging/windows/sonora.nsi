; Sonora Windows installer (NSIS).
;
; Built by .github/workflows/release.yml from the staged payload dir,
; which must be the current working directory when invoking makensis:
;   sonora.exe     icon + version info compiled in (crates/sonora/build.rs)
;   *.dll          GStreamer MSVC runtime (all of gst's bin\)
;   plugins\*.dll  GStreamer plugin objects
; The app finds the plugins at startup via GST_PLUGIN_SYSTEM_PATH_1_0,
; set when a `plugins` dir sits next to the exe
; (crates/sonora/src/gst_bundle.rs).
;
;   makensis /DVERSION=0.1.13 /DINSTALLER_ICON=<abs path to icon.ico> sonora.nsi
;
; The in-app updater downloads this installer and launches it with no
; extra flags. Sonora keeps running through the wizard. The install
; section closes sonora.exe (the file is locked while it runs), then
; copies the new files. The finish page can create a desktop shortcut
; and start Sonora again.
;
; `/S` stays non-interactive: no finish page, so the desktop shortcut is
; created here and Sonora is not started.

!ifndef VERSION
  !error "VERSION must be defined (pass /DVERSION=x.y.z)"
!endif

Unicode true
SetCompressor /SOLID lzma

Name "Sonora"
OutFile "Sonora-Setup-${VERSION}.exe"
InstallDir "$PROGRAMFILES64\Sonora"
; Legacy installers wrote this key in the 32-bit view. New installs use
; the 64-bit view (SetRegView in .onInit). .onInit also checks that view
; so an upgrade still finds the previous directory.
InstallDirRegKey HKLM "Software\Sonora" "InstallDir"
RequestExecutionLevel admin

; VIProductVersion wants exactly four parts.
VIProductVersion "${VERSION}.0"
VIAddVersionKey /LANG=0 "ProductName" "Sonora"
VIAddVersionKey /LANG=0 "FileDescription" "Sonora - native Apple Music client"
VIAddVersionKey /LANG=0 "CompanyName" "Sonora contributors"
VIAddVersionKey /LANG=0 "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=0 "ProductVersion" "${VERSION}"

!ifndef INSTALLER_ICON
  !define INSTALLER_ICON "icon.ico"
!endif
!define MUI_ICON "${INSTALLER_ICON}"
!define MUI_UNICON "${INSTALLER_ICON}"

!include "MUI2.nsh"
!include "LogicLib.nsh"

!define MUI_FINISHPAGE_SHOWREADME
!define MUI_FINISHPAGE_SHOWREADME_TEXT "Create desktop shortcut"
!define MUI_FINISHPAGE_SHOWREADME_FUNCTION CreateDesktopShortcut
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_TEXT "Run Sonora"
!define MUI_FINISHPAGE_RUN_FUNCTION LaunchSonora

!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

!macro SetUninstallValue KEY VALUE
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Sonora" "${KEY}" "${VALUE}"
!macroend

; Prefer the 64-bit registry, then the 32-bit key written by installers
; that predate SetRegView 64.
Function .onInit
  SetRegView 64
  ReadRegStr $0 HKLM "Software\Sonora" "InstallDir"
  ${If} $0 == ""
    SetRegView 32
    ReadRegStr $0 HKLM "Software\Sonora" "InstallDir"
    SetRegView 64
  ${EndIf}
  ${If} $0 != ""
    StrCpy $INSTDIR $0
  ${EndIf}
FunctionEnd

Function CreateDesktopShortcut
  SetShellVarContext all
  CreateShortCut "$DESKTOP\Sonora.lnk" "$INSTDIR\sonora.exe"
FunctionEnd

Function LaunchSonora
  Exec '"$INSTDIR\sonora.exe"'
FunctionEnd

Section "Sonora" SecMain
  SetShellVarContext all
  SetRegView 64
  ; sonora.exe is locked while the app is open. Close it before copying.
  ; No /T: the installer must not kill its own process tree.
  ExecWait '"$SYSDIR\taskkill.exe" /IM sonora.exe'
  Sleep 1500
  ExecWait '"$SYSDIR\taskkill.exe" /F /IM sonora.exe'
  Sleep 500

  SetOutPath "$INSTDIR"
  File "sonora.exe"
  File "*.dll"
  SetOutPath "$INSTDIR\plugins"
  File "plugins\*.dll"
  SetOutPath "$INSTDIR"

  WriteRegStr HKLM "Software\Sonora" "InstallDir" "$INSTDIR"
  !insertmacro SetUninstallValue "DisplayName" "Sonora"
  !insertmacro SetUninstallValue "DisplayVersion" "${VERSION}"
  !insertmacro SetUninstallValue "Publisher" "Sonora contributors"
  !insertmacro SetUninstallValue "DisplayIcon" "$INSTDIR\sonora.exe"
  !insertmacro SetUninstallValue "UninstallString" '$\"$INSTDIR\uninstall.exe$\"'
  !insertmacro SetUninstallValue "QuietUninstallString" '$\"$INSTDIR\uninstall.exe$\" /S'
  WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Sonora" "NoModify" 1
  WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Sonora" "NoRepair" 1

  WriteUninstaller "$INSTDIR\uninstall.exe"
  CreateShortCut "$SMPROGRAMS\Sonora.lnk" "$INSTDIR\sonora.exe"
  ; The finish page is skipped for /S, so create the shortcut here.
  IfSilent create_desktop skip_desktop
  create_desktop:
    CreateShortCut "$DESKTOP\Sonora.lnk" "$INSTDIR\sonora.exe"
  skip_desktop:
SectionEnd

Section "Uninstall"
  SetShellVarContext all
  SetRegView 64
  Delete "$INSTDIR\sonora.exe"
  Delete "$INSTDIR\*.dll"
  Delete "$INSTDIR\plugins\*.dll"
  RMDir "$INSTDIR\plugins"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\Sonora.lnk"
  Delete "$DESKTOP\Sonora.lnk"
  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Sonora"
  DeleteRegKey HKLM "Software\Sonora"
  SetRegView 32
  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Sonora"
  DeleteRegKey HKLM "Software\Sonora"
SectionEnd
