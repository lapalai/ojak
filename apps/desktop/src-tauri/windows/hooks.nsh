!define OJAK_HOOK_DIR "${__FILEDIR__}"
Var OjakUpdatePrepared

; $2 = 사용자에게 보여줄 문장. 같은 문장을 $TEMP\Ojak-installer.log 에도 남긴다.
; NSIS는 제거 섹션에서 "un."으로 시작하는 함수만 부를 수 있어 같은 본문을 두 이름으로 만든다.
!macro OJAK_FAIL_BODY
  ClearErrors
  FileOpen $1 "$TEMP\Ojak-installer.log" a
  IfErrors +4
  FileSeek $1 0 END
  FileWrite $1 "$2$\r$\n"
  FileClose $1
  MessageBox MB_OK|MB_ICONSTOP "$2$\r$\n$\r$\n기록 / Log: $TEMP\Ojak-installer.log$\r$\n설치 창의 [자세히]를 여세요. / Open Details in this window." /SD IDOK
!macroend

Function OjakFail
  !insertmacro OJAK_FAIL_BODY
FunctionEnd

Function un.OjakFail
  !insertmacro OJAK_FAIL_BODY
FunctionEnd

Function OjakRecoverUpdate
  ${If} $OjakUpdatePrepared == 1
    nsExec::ExecToLog '"$PLUGINSDIR\ojak-update\aam.exe" installer recover --directory "$INSTDIR"'
    Pop $0
    ${If} $0 != 0
      StrCpy $2 "이전 버전으로 되돌리지 못했어요. 실행 중인 작업을 강제로 끄지 마세요. 오류를 고친 뒤 설치 파일을 다시 실행해 주세요. 복구 기록은 남겨 뒀어요. / Could not restore the previous version. Do not force-stop running work. Fix the error, then run the installer again. The recovery record was kept."
      Call OjakFail
    ${EndIf}
    StrCpy $OjakUpdatePrepared 0
  ${EndIf}
FunctionEnd

Function .onInstFailed
  Call OjakRecoverUpdate
FunctionEnd

Function .onGUIEnd
  Call OjakRecoverUpdate
FunctionEnd

!macro NSIS_HOOK_PREINSTALL
  InitPluginsDir
  SetOutPath "$PLUGINSDIR\ojak-update"
  File /oname=aam.exe "${OJAK_HOOK_DIR}\..\binaries\installer-aam.exe"
  File /oname=aam-service.exe "${OJAK_HOOK_DIR}\..\binaries\installer-aam-service.exe"
  CreateDirectory "$INSTDIR"
  nsExec::ExecToLog '"$PLUGINSDIR\ojak-update\aam.exe" installer prepare --directory "$INSTDIR"'
  Pop $0
  ${If} $0 != 0
    SetErrorLevel 1
    StrCpy $2 "기존 Ojak을 안전하게 멈추지 못해 설치를 멈췄어요. 파일은 바꾸지 않았어요. 실행 중인 작업을 끝낸 뒤 설치 파일을 다시 실행해 주세요. / Could not safely stop the existing Ojak. No files were replaced. Finish running work, then run the installer again."
    Call OjakFail
    Abort
  ${EndIf}
  StrCpy $OjakUpdatePrepared 1
  SetOutPath "$INSTDIR"
!macroend

!macro NSIS_HOOK_POSTINSTALL
  nsExec::ExecToLog '"$PLUGINSDIR\ojak-update\aam.exe" installer finish --directory "$INSTDIR"'
  Pop $0
  ${If} $0 != 0
    Call OjakRecoverUpdate
    SetErrorLevel 1
    StrCpy $2 "설치 확인에 실패했어요. 새 버전을 서비스로 시작하지 않았어요. 설치 파일을 다시 실행하면 이전 상태로 돌려요. / Installation check failed. The new version was not started. Run the installer again to restore the previous version."
    Call OjakFail
    Abort
  ${EndIf}
  StrCpy $OjakUpdatePrepared 0
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ${If} ${FileExists} "$INSTDIR\aam.exe"
    nsExec::ExecToLog '"$INSTDIR\aam.exe" installer remove --directory "$INSTDIR"'
    Pop $0
    ${If} $0 != 0
      SetErrorLevel 1
      StrCpy $2 "제거를 멈췄어요. 실행 중인 작업을 끝내거나 PC를 다시 시작한 뒤 다시 시도해 주세요. 앱 파일은 지우지 않았어요. omp 연결은 이미 원래대로 돌아갔을 수 있어요. / Uninstall stopped. Finish running work or restart the PC, then try again. Application files were not removed. omp may already be back to its original login."
      Call un.OjakFail
      Abort
    ${EndIf}
  ${ElseIf} ${FileExists} "$INSTDIR\aam-service.exe"
    SetErrorLevel 1
    StrCpy $2 "Ojak 실행 파일이 없어 서비스를 안전하게 끌 수 없어요. 설치 파일을 다시 실행해 고친 뒤 제거해 주세요. / The Ojak program is missing, so the service cannot be stopped safely. Run the installer again to repair, then uninstall."
    Call un.OjakFail
    Abort
  ${EndIf}
!macroend
