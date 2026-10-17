; VERSION is never hard-coded here. ci/tasks/package-release passes the release
; tag (-DVERSION=vX.Y.Z), which ci/check-version.sh has already matched to the
; workspace version. A local build must pass it too.
!ifndef VERSION
  !error "pass -DVERSION=<version>, e.g. makensis -DVERSION=v1.6.0 windows.nsi"
!endif

!define PRODUCT_NAME "rustnzb"
!define PRODUCT_PUBLISHER "AusAgentSmith"
!define PRODUCT_URL "https://rustnzb.dev"
!define PRODUCT_UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\rustnzb"

!include "MUI2.nsh"

Name "${PRODUCT_NAME} ${VERSION}"
OutFile "../rustnzb-${VERSION}-windows-x86_64-setup.exe"
InstallDir "$PROGRAMFILES64\rustnzb"
RequestExecutionLevel admin
SetCompressor /SOLID lzma

!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$WINDIR\explorer.exe"
!define MUI_FINISHPAGE_RUN_PARAMETERS "http://localhost:9090"
!define MUI_FINISHPAGE_RUN_TEXT "Open rustnzb web UI"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

Section "rustnzb" SecMain
  SetOutPath "$INSTDIR"
  File "rustnzb.exe"

  ; .nzb file association — opens via rustnzb API
  WriteRegStr HKCR ".nzb" "" "rustnzb.NZBFile"
  WriteRegStr HKCR "rustnzb.NZBFile" "" "NZB File"
  WriteRegStr HKCR "rustnzb.NZBFile\DefaultIcon" "" "$INSTDIR\rustnzb.exe,0"
  WriteRegStr HKCR "rustnzb.NZBFile\shell\open\command" "" '"$INSTDIR\rustnzb.exe" "%1"'

  ; Start menu shortcuts
  CreateDirectory "$SMPROGRAMS\rustnzb"
  CreateShortCut "$SMPROGRAMS\rustnzb\rustnzb.lnk" "$INSTDIR\rustnzb.exe"
  CreateShortCut "$SMPROGRAMS\rustnzb\Uninstall.lnk" "$INSTDIR\Uninstall.exe"

  ; Uninstall registry entries
  WriteUninstaller "$INSTDIR\Uninstall.exe"
  WriteRegStr HKLM "${PRODUCT_UNINST_KEY}" "DisplayName" "${PRODUCT_NAME} ${VERSION}"
  WriteRegStr HKLM "${PRODUCT_UNINST_KEY}" "UninstallString" "$INSTDIR\Uninstall.exe"
  WriteRegStr HKLM "${PRODUCT_UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${PRODUCT_UNINST_KEY}" "Publisher" "${PRODUCT_PUBLISHER}"
  WriteRegStr HKLM "${PRODUCT_UNINST_KEY}" "URLInfoAbout" "${PRODUCT_URL}"
  WriteRegDWORD HKLM "${PRODUCT_UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKLM "${PRODUCT_UNINST_KEY}" "NoRepair" 1
SectionEnd

Section "Uninstall"
  Delete "$INSTDIR\rustnzb.exe"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"

  Delete "$SMPROGRAMS\rustnzb\rustnzb.lnk"
  Delete "$SMPROGRAMS\rustnzb\Uninstall.lnk"
  RMDir "$SMPROGRAMS\rustnzb"

  DeleteRegKey HKLM "${PRODUCT_UNINST_KEY}"
  DeleteRegKey HKCR ".nzb"
  DeleteRegKey HKCR "rustnzb.NZBFile"
SectionEnd
