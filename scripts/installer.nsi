; Packs dist\FlowBar into a Setup program that installs it for the current user, in
; %LOCALAPPDATA%\Programs\FlowBar where the in-app update can replace it, with a Start
; menu shortcut and an uninstaller. Needs NSIS 3.04 or later, which reads the paths
; below relative to this script:
;   makensis -DVERSION=0.2.1 -DARCH=x86_64 scripts\installer.nsi
; The Setup program's /S installs silently and does not open FlowBar afterwards.
; Saved as UTF-8 with a BOM: makensis on Windows reads a file without one in the
; system code page, which would garble the Chinese text.
Unicode true
ManifestDPIAware true
SetCompressor /SOLID lzma

!include MUI2.nsh
!include LogicLib.nsh
!include x64.nsh

!ifndef VERSION | ARCH
    !error "Pass -DVERSION=<version> -DARCH=<x86_64|arm64>"
!endif
!if ${ARCH} != x86_64
!if ${ARCH} != arm64
    !error "ARCH must be x86_64 or arm64"
!endif
!endif
!ifndef OUTFILE
    !define OUTFILE "..\dist\FlowBar-${VERSION}-windows-${ARCH}-setup.exe"
!endif
!define SOURCE "..\dist\FlowBar"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\FlowBar"
!define RUN_KEY "Software\Microsoft\Windows\CurrentVersion\Run"

Name "FlowBar"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\FlowBar"
RequestExecutionLevel user
BrandingText "FlowBar ${VERSION}"

!define MUI_ICON "..\Resources\windows\AppIcon.ico"
!define MUI_UNICON "..\Resources\windows\AppIcon.ico"
!define MUI_FINISHPAGE_RUN "$INSTDIR\FlowBar.exe"
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "SimpChinese"

; FlowBar has no quit command, so it is ended; its supervisor then stops the core. A TUN
; supervisor and core run elevated and exit on their own, so this waits until their
; files can be deleted.
!macro CLOSE_FLOWBAR un
Function ${un}CloseFlowBar
    nsExec::Exec '"$SYSDIR\taskkill.exe" /F /IM FlowBar.exe'
    Pop $0
    ${For} $0 1 80
        Delete "$INSTDIR\FlowBar.exe"
        Delete "$INSTDIR\rill.exe"
        ; The core FlowBar 0.4.0 and earlier ran, or the placeholder that replaced it.
        Delete "$INSTDIR\mihomo.exe"
        ${IfNot} ${FileExists} "$INSTDIR\FlowBar.exe"
        ${AndIfNot} ${FileExists} "$INSTDIR\rill.exe"
        ${AndIfNot} ${FileExists} "$INSTDIR\mihomo.exe"
            Return
        ${EndIf}
        Sleep 250
    ${Next}
    MessageBox MB_ICONSTOP "FlowBar 仍在运行，请从菜单退出 FlowBar 后再试。" /SD IDOK
    Abort
FunctionEnd
!macroend
!insertmacro CLOSE_FLOWBAR ""
!insertmacro CLOSE_FLOWBAR "un."

Function .onInit
!if ${ARCH} == arm64
    ${IfNot} ${IsNativeARM64}
        MessageBox MB_ICONSTOP "这是 ARM64 版安装程序，这台电脑请下载 windows-x86_64 版。" /SD IDOK
        Abort
    ${EndIf}
!else
    ${IfNot} ${IsNativeAMD64}
        MessageBox MB_ICONSTOP "这是 x86_64 版安装程序，这台电脑请下载 windows-arm64 版。" /SD IDOK
        Abort
    ${EndIf}
!endif
FunctionEnd

Section
    Call CloseFlowBar
    SetOutPath "$INSTDIR"
    File /r "${SOURCE}\*"
    WriteUninstaller "$INSTDIR\Uninstall.exe"
    CreateShortcut "$SMPROGRAMS\FlowBar.lnk" "$INSTDIR\FlowBar.exe"
    ; No DisplayVersion: in-app updates would leave it stale.
    WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "FlowBar"
    WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\FlowBar.exe"
    WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
    WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\Uninstall.exe"'
    WriteRegStr HKCU "${UNINSTALL_KEY}" "QuietUninstallString" '"$INSTDIR\Uninstall.exe" /S'
    WriteRegStr HKCU "${UNINSTALL_KEY}" "URLInfoAbout" "https://github.com/best-shuke/FlowBar"
    WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
    WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1
SectionEnd

; The whole directory is removed, so an uninstaller copied elsewhere must not run.
Function un.onInit
    StrCpy $0 "$INSTDIR" "" -8
    ${If} $0 != "\FlowBar"
    ${OrIfNot} ${FileExists} "$INSTDIR\rill.exe"
        MessageBox MB_ICONSTOP "找不到 FlowBar 的安装目录。" /SD IDOK
        Abort
    ${EndIf}
FunctionEnd

; Settings, the imported profile and logs in %LOCALAPPDATA%\FlowBar stay.
Section "Uninstall"
    Call un.CloseFlowBar
    ReadRegStr $0 HKCU "${RUN_KEY}" "FlowBar"
    ${If} $0 == '"$INSTDIR\FlowBar.exe"'
        DeleteRegValue HKCU "${RUN_KEY}" "FlowBar"
    ${EndIf}
    Delete "$SMPROGRAMS\FlowBar.lnk"
    DeleteRegKey HKCU "${UNINSTALL_KEY}"
    RMDir /r "$INSTDIR"
SectionEnd
