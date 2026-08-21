; NSIS installer for Schist. Build with:
;   makensis -DVERSION=0.1.0 packaging/windows/installer.nsi
!ifndef VERSION
  !define VERSION "0.1.0"
!endif

Name "Schist ${VERSION}"
OutFile "..\..\dist\Schist-${VERSION}-setup.exe"
InstallDir "$PROGRAMFILES64\Schist"
InstallDirRegKey HKLM "Software\Schist" "InstallDir"
RequestExecutionLevel admin
Unicode true

Icon "schist.ico"
UninstallIcon "schist.ico"

Page directory
Page instfiles
UninstPage uninstConfirm
UninstPage instfiles

Section "Schist"
  SetOutPath "$INSTDIR"
  File "..\..\target\release\schist.exe"
  File "schist.ico"

  WriteRegStr HKLM "Software\Schist" "InstallDir" "$INSTDIR"
  ; Add/Remove Programs entry.
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Schist" \
    "DisplayName" "Schist"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Schist" \
    "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Schist" \
    "UninstallString" "$INSTDIR\uninstall.exe"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Schist" \
    "DisplayIcon" "$INSTDIR\schist.ico"

  ; Open .psd files with Schist.
  WriteRegStr HKCR ".psd\OpenWithProgids" "Schist.psd" ""
  WriteRegStr HKCR "Schist.psd\shell\open\command" "" '"$INSTDIR\schist.exe" "%1"'
  WriteRegStr HKCR "Schist.psd\DefaultIcon" "" "$INSTDIR\schist.ico"

  CreateShortcut "$SMPROGRAMS\Schist.lnk" "$INSTDIR\schist.exe" "" "$INSTDIR\schist.ico"
  WriteUninstaller "$INSTDIR\uninstall.exe"
SectionEnd

Section "Uninstall"
  Delete "$INSTDIR\schist.exe"
  Delete "$INSTDIR\schist.ico"
  Delete "$INSTDIR\uninstall.exe"
  Delete "$SMPROGRAMS\Schist.lnk"
  RMDir "$INSTDIR"
  DeleteRegKey HKLM "Software\Schist"
  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Schist"
  DeleteRegKey HKCR "Schist.psd"
SectionEnd
