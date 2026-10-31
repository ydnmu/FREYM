; SPDX-License-Identifier: GPL-3.0-or-later
; Copyright (C) 2026 FREYM contributors
Unicode true
RequestExecutionLevel user
Name "FREYM"
OutFile "${OUTPUT}"
InstallDir "$LOCALAPPDATA\Programs\FREYM"
InstallDirRegKey HKCU "Software\FREYM\Native" "InstallDir"
Icon "${ICON}"
UninstallIcon "${ICON}"
SetCompressor /SOLID lzma
!include "MUI2.nsh"
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "${PAYLOAD}\LICENSE"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Section "FREYM native" Main
  SetShellVarContext current
  SetOutPath "$INSTDIR"
  File /r "${PAYLOAD}\*"
  WriteUninstaller "$INSTDIR\uninstall.exe"
  CreateDirectory "$SMPROGRAMS\FREYM"
  CreateShortcut "$SMPROGRAMS\FREYM\FREYM.lnk" "$INSTDIR\freym-launcher.exe" "" "$INSTDIR\freym.exe"
  CreateShortcut "$SMPROGRAMS\FREYM\Uninstall FREYM.lnk" "$INSTDIR\uninstall.exe"
  WriteRegStr HKCU "Software\FREYM\Native" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FREYMNative" "DisplayName" "FREYM"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FREYMNative" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FREYMNative" "Publisher" "FREYM contributors"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FREYMNative" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FREYMNative" "UninstallString" '$"$INSTDIR\uninstall.exe$"'
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FREYMNative" "NoModify" 1
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FREYMNative" "NoRepair" 1
SectionEnd

Section "Uninstall"
  SetShellVarContext current
  ; Delete only the generated inventory of installed files. User-created files,
  ; recordings and externally stored preferences/plugins survive uninstall.
  !include "${UNINSTALL_LIST}"
  Delete "$SMPROGRAMS\FREYM\FREYM.lnk"
  Delete "$SMPROGRAMS\FREYM\Uninstall FREYM.lnk"
  RMDir "$SMPROGRAMS\FREYM"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FREYMNative"
  DeleteRegKey HKCU "Software\FREYM\Native"
SectionEnd
