; NSIS installer for Schist. Build with:
;   makensis -DVERSION=0.11.0 packaging/windows/installer.nsi
!ifndef VERSION
  !define VERSION "0.11.0"
!endif

Name "Schist ${VERSION}"
OutFile "..\..\dist\Schist-${VERSION}-setup.exe"
InstallDir "$PROGRAMFILES64\Schist"
InstallDirRegKey HKLM "Software\Schist" "InstallDir"
RequestExecutionLevel admin
Unicode true

Icon "schist.ico"
UninstallIcon "schist.ico"

; One ProgID per extension. Registered under the extension's
; OpenWithProgids rather than as its default handler, so installing Schist
; never silently steals files from Photoshop or the Affinity apps -- it
; just joins their "Open with" menu, and Windows offers it as a choice.
!macro AssociateExt ext desc
  WriteRegStr HKCR ".${ext}\OpenWithProgids" "Schist.${ext}" ""
  WriteRegStr HKCR "Schist.${ext}" "" "${desc}"
  WriteRegStr HKCR "Schist.${ext}\DefaultIcon" "" "$INSTDIR\schist.ico"
  WriteRegStr HKCR "Schist.${ext}\shell\open\command" "" '"$INSTDIR\schist.exe" "%1"'
!macroend

!macro UnassociateExt ext
  DeleteRegValue HKCR ".${ext}\OpenWithProgids" "Schist.${ext}"
  DeleteRegKey HKCR "Schist.${ext}"
!macroend

Page directory
Page instfiles
UninstPage uninstConfirm
UninstPage instfiles

Section "Schist"
  SetOutPath "$INSTDIR"
  File "..\..\target\release\schist.exe"
  ; The MCP server. No shortcut and no association: it is a stdio server that
  ; an MCP client spawns by path, not something anyone double-clicks.
  File "..\..\target\release\schist-mcp.exe"
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

  ; Everything Schist can open that the shell has no better idea about.
  ; PSB is a PSD with 64-bit offsets, and the Affinity family is
  ; import-only, but all of them open by double-click just the same.
  !insertmacro AssociateExt "psd" "Photoshop Document"
  !insertmacro AssociateExt "psb" "Photoshop Large Document"
  !insertmacro AssociateExt "afphoto" "Affinity Photo Document"
  !insertmacro AssociateExt "afdesign" "Affinity Designer Document"
  !insertmacro AssociateExt "afpub" "Affinity Publisher Document"
  !insertmacro AssociateExt "af" "Affinity Document"
  ; SHCNE_ASSOCCHANGED: pick the new associations up now rather than at the
  ; next sign-in, so the icons and the Open With menu are right immediately.
  System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, i 0, i 0)'

  CreateShortcut "$SMPROGRAMS\Schist.lnk" "$INSTDIR\schist.exe" "" "$INSTDIR\schist.ico"
  WriteUninstaller "$INSTDIR\uninstall.exe"
SectionEnd

Section "Uninstall"
  Delete "$INSTDIR\schist.exe"
  Delete "$INSTDIR\schist-mcp.exe"
  Delete "$INSTDIR\schist.ico"
  Delete "$INSTDIR\uninstall.exe"
  Delete "$SMPROGRAMS\Schist.lnk"
  RMDir "$INSTDIR"
  DeleteRegKey HKLM "Software\Schist"
  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Schist"
  !insertmacro UnassociateExt "psd"
  !insertmacro UnassociateExt "psb"
  !insertmacro UnassociateExt "afphoto"
  !insertmacro UnassociateExt "afdesign"
  !insertmacro UnassociateExt "afpub"
  !insertmacro UnassociateExt "af"
  System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, i 0, i 0)'
SectionEnd
