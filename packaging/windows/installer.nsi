; NSIS installer for Photoslop. Build with:
;   makensis -DVERSION=0.1.0 packaging/windows/installer.nsi
!ifndef VERSION
  !define VERSION "0.1.0"
!endif

Name "Photoslop ${VERSION}"
OutFile "..\..\dist\Photoslop-${VERSION}-setup.exe"
InstallDir "$PROGRAMFILES64\Photoslop"
InstallDirRegKey HKLM "Software\Photoslop" "InstallDir"
RequestExecutionLevel admin
Unicode true

Page directory
Page instfiles
UninstPage uninstConfirm
UninstPage instfiles

Section "Photoslop"
  SetOutPath "$INSTDIR"
  File "..\..\target\release\photoslop.exe"

  WriteRegStr HKLM "Software\Photoslop" "InstallDir" "$INSTDIR"
  ; Add/Remove Programs entry.
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Photoslop" \
    "DisplayName" "Photoslop"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Photoslop" \
    "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Photoslop" \
    "UninstallString" "$INSTDIR\uninstall.exe"

  ; Open .psd files with Photoslop.
  WriteRegStr HKCR ".psd\OpenWithProgids" "Photoslop.psd" ""
  WriteRegStr HKCR "Photoslop.psd\shell\open\command" "" '"$INSTDIR\photoslop.exe" "%1"'

  CreateShortcut "$SMPROGRAMS\Photoslop.lnk" "$INSTDIR\photoslop.exe"
  WriteUninstaller "$INSTDIR\uninstall.exe"
SectionEnd

Section "Uninstall"
  Delete "$INSTDIR\photoslop.exe"
  Delete "$INSTDIR\uninstall.exe"
  Delete "$SMPROGRAMS\Photoslop.lnk"
  RMDir "$INSTDIR"
  DeleteRegKey HKLM "Software\Photoslop"
  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Photoslop"
  DeleteRegKey HKCR "Photoslop.psd"
SectionEnd
