; Included by Tauri's standard NSIS template. Application data lives outside INSTDIR.
!define EXPANDRV_WINFSP_MSI "${__FILEDIR__}\deps\winfsp-runtime.msi"
Var ExpanDrvRuntimeReady

Function ExpanDrvCheckRuntime
  Push $0
  Push $1
  Push $2
  StrCpy $ExpanDrvRuntimeReady 0
  SetRegView 64
  ReadRegStr $0 HKLM "Software\WOW6432Node\WinFsp" "InstallDir"
  ${If} $0 == ""
    ReadRegStr $0 HKLM "Software\WinFsp" "InstallDir"
  ${EndIf}
  SetRegView lastused
  ${If} $0 != ""
    ClearErrors
    GetDLLVersion "$0\bin\winfsp-x64.dll" $1 $2
    ${IfNot} ${Errors}
    ${AndIf} $1 >= 0x00020001
      StrCpy $ExpanDrvRuntimeReady 1
    ${EndIf}
  ${EndIf}
  Pop $2
  Pop $1
  Pop $0
FunctionEnd

!macro ExpanDrvRunCli ARGUMENTS
  ${If} ${FileExists} "$INSTDIR\expandrv.exe"
    nsExec::ExecToStack /TIMEOUT=180000 '"$INSTDIR\expandrv.exe" ${ARGUMENTS}'
    Pop $0
    Pop $1
    ${If} $0 != "0"
      DetailPrint "$1"
      MessageBox MB_OK|MB_ICONSTOP "ExpanDrv 작업을 마치지 못했습니다. 업로드 상태를 확인한 뒤 다시 시도하세요." /SD IDOK
      SetErrorLevel 2
      Abort
    ${EndIf}
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro ExpanDrvRunCli "stop"
  Call ExpanDrvCheckRuntime
  ${If} $ExpanDrvRuntimeReady != 1
    InitPluginsDir
    File /oname=$PLUGINSDIR\winfsp-runtime.msi "${EXPANDRV_WINFSP_MSI}"
    DetailPrint "WinFsp 런타임을 설치합니다. 드라이버 설치에는 관리자 승인이 필요합니다."
    ClearErrors
    ExecShellWait "runas" "$SYSDIR\msiexec.exe" '/i "$PLUGINSDIR\winfsp-runtime.msi" /qn /norestart INSTALLLEVEL=1 /l*v "$TEMP\ExpanDrv-WinFsp-install.log"' SW_HIDE
    ${If} ${Errors}
      MessageBox MB_OK|MB_ICONSTOP "WinFsp 설치를 완료한 뒤 다시 실행하세요." /SD IDOK
      SetErrorLevel 2
      Abort
    ${EndIf}
    Call ExpanDrvCheckRuntime
    ${If} $ExpanDrvRuntimeReady != 1
      MessageBox MB_OK|MB_ICONSTOP "WinFsp 런타임을 확인할 수 없습니다. $TEMP\ExpanDrv-WinFsp-install.log를 확인하세요." /SD IDOK
      SetErrorLevel 2
      Abort
    ${EndIf}
  ${EndIf}
!macroend

; 실패해도 설치를 멈추지 않는 CLI 실행(자동 실행 등록 등 부가 작업)
!macro ExpanDrvTryCli ARGUMENTS
  ${If} ${FileExists} "$INSTDIR\expandrv.exe"
    nsExec::ExecToStack /TIMEOUT=30000 '"$INSTDIR\expandrv.exe" ${ARGUMENTS}'
    Pop $0
    Pop $1
    DetailPrint "$1"
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; 로그인 자동 실행을 시작 앱(HKCU Run → 콘솔 창 없는 GUI)으로 등록합니다.
  ; 사용자가 끈 상태는 유지하고, 이전 버전의 시작 프로그램 바로가기는 새 방식으로 옮깁니다.
  !insertmacro ExpanDrvTryCli "autostart install"
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro ExpanDrvRunCli "stop"
  ${If} $UpdateMode != 1
    !insertmacro ExpanDrvRunCli "shell disable-local"
    !insertmacro ExpanDrvTryCli "autostart uninstall"
  ${EndIf}
  DetailPrint "설정, 자격 증명, 캐시와 업로드 대기 파일은 보존합니다."
!macroend
