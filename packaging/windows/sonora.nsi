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
;   makensis /DVERSION=0.1.9 /DINSTALLER_ICON=<abs path to icon.ico> sonora.nsi
;
; Silent installs (`Sonora-Setup-<version>.exe /S`) are how the in-app
; updater applies this package. The updater waits until sonora.exe has
; exited before launching it, so these files are not locked. Keep the
; script non-interactive: no message boxes, and no MUI_FINISHPAGE_RUN
; (the updater starts sonora.exe itself after /S returns).

!ifndef VERSION
  !error "VERSION must be defined (pass /DVERSION=x.y.z)"
!endif

Unicode true
SetCompressor /SOLID lzma

Name "Sonora"
OutFile "Sonora-Setup-${VERSION}.exe"
InstallDir "$PROGRAMFILES64\Sonora"
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

!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

!macro SetUninstallValue KEY VALUE
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Sonora" "${KEY}" "${VALUE}"
!macroend

Section "Sonora" SecMain
  SetShellVarContext all
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
  CreateShortCut "$DESKTOP\Sonora.lnk" "$INSTDIR\sonora.exe"
SectionEnd

Section "Uninstall"
  SetShellVarContext all
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
SectionEnd
