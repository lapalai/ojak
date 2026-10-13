!define OJAK_HOOK_DIR "${__FILEDIR__}"
Var OjakUpdatePrepared
Var OjakInstallerStage
Var OjakFailureCode
Var OjakInstallerLock

; 같은 사용자의 설치·제거만 직렬화한다. share=0 + delete-on-close로 강제 종료 뒤에도 OS가 잠금을 회수한다.
; .onInit에서는 잡지 않는다: Tauri의 재설치 페이지가 먼저 이전 uninstaller를 자식으로 실행할 수 있다.
!macro OJAK_LOCK PREFIX
  StrCpy $OjakInstallerStage "lock"
  System::Call 'kernel32::CreateFileW(w"$APPDATA\ai.aam.installer.lock", i0x40000000, i0, p0, i4, i0x04200000, p0)p.r0?e'
  Pop $1
  StrCpy $OjakInstallerLock $0
  ${If} $OjakInstallerLock = -1
    StrCpy $0 $1
    StrCpy $OjakFailureCode "INSTALLER_LOCK_FAILED"
    ${If} $0 == 32
    ${OrIf} $0 == 33
      StrCpy $OjakFailureCode "INSTALLER_ALREADY_RUNNING"
    ${EndIf}
    StrCpy $2 "다른 Ojak 설치·제거가 실행 중이거나 설치 잠금을 확인하지 못했어요. 해당 작업이 끝난 뒤 다시 실행해 주세요. 앱 파일은 바꾸지 않았어요. / Another Ojak installation or removal is running, or its lock could not be verified. Try again after it finishes. Application files were not changed."
    SetErrorLevel 1
    Call ${PREFIX}OjakFail
    Abort
  ${EndIf}
!macroend

; nsExec::ExecToLog는 자세히 창에만 출력한다. helper의 오류 코드는 별도 ASCII 파일로 받는다.
!macro OJAK_EXEC HELPER ACTION
  StrCpy $OjakInstallerStage "${ACTION}"
  StrCpy $OjakFailureCode "HELPER_RESULT_UNAVAILABLE"
  Delete "$PLUGINSDIR\ojak-error.txt"
  nsExec::ExecToLog '"${HELPER}" installer ${ACTION} --directory "$INSTDIR" --error-file "$PLUGINSDIR\ojak-error.txt"'
  Pop $0
  ClearErrors
  FileOpen $1 "$PLUGINSDIR\ojak-error.txt" r
  ${Unless} ${Errors}
    FileRead $1 $OjakFailureCode
    FileClose $1
  ${EndUnless}
  DetailPrint "Ojak: stage=$OjakInstallerStage exit=$0 code=$OjakFailureCode"
  !insertmacro OJAK_LOG_LINE
!macroend

; 코드만 ASCII로 기록한다. CP949/UTF-8 혼선과 원문 오류의 개인정보 유출을 피한다.
; NSIS는 제거 섹션에서 "un."으로 시작하는 함수만 부를 수 있어 같은 본문을 두 이름으로 만든다.
!macro OJAK_LOG_LINE
  ClearErrors
  FileOpen $1 "$TEMP\Ojak-installer.log" a
  IfErrors +4
  FileSeek $1 0 END
  FileWrite $1 "Ojak: stage=$OjakInstallerStage exit=$0 code=$OjakFailureCode$\r$\n"
  FileClose $1
!macroend

!macro OJAK_FAIL_BODY
  !insertmacro OJAK_LOG_LINE
  MessageBox MB_OK|MB_ICONSTOP "$2$\r$\n$\r$\n오류 / Error: $OjakFailureCode (stage=$OjakInstallerStage, exit=$0)$\r$\n기록 / Log: $TEMP\Ojak-installer.log$\r$\n설치 창의 [자세히]를 여세요. / Open Details in this window." /SD IDOK
!macroend

Function OjakFail
  !insertmacro OJAK_FAIL_BODY
FunctionEnd

Function un.OjakFail
  !insertmacro OJAK_FAIL_BODY
FunctionEnd

; 사용자가 직접 취소하면 기록만 남기고 멈춘다. 취소 뒤에 또 확인 창을 띄우지 않는다.
Function OjakCancelled
  !insertmacro OJAK_LOG_LINE
  DetailPrint "Ojak: installation canceled; no further programs were closed and no Ojak files were replaced."
FunctionEnd

Function OjakRecoverUpdate
  Push $0
  Push $OjakInstallerStage
  Push $OjakFailureCode
  ${If} $OjakUpdatePrepared == 1
    !insertmacro OJAK_EXEC "$PLUGINSDIR\ojak-update\aam.exe" "recover"
    ${If} $0 != 0
      StrCpy $2 "이전 버전으로 되돌리지 못했어요. 실행 중인 작업을 강제로 끄지 마세요. 오류를 고친 뒤 설치 파일을 다시 실행해 주세요. 복구 기록은 남겨 뒀어요. / Could not restore the previous version. Do not force-stop running work. Fix the error, then run the installer again. The recovery record was kept."
      Call OjakFail
    ${EndIf}
    StrCpy $OjakUpdatePrepared 0
  ${EndIf}
  Pop $OjakFailureCode
  Pop $OjakInstallerStage
  Pop $0
FunctionEnd

Function .onInstFailed
  Call OjakRecoverUpdate
FunctionEnd

Function .onGUIEnd
  Call OjakRecoverUpdate
FunctionEnd

!macro NSIS_HOOK_PREINSTALL
  !insertmacro OJAK_LOCK ""
  InitPluginsDir
  SetOutPath "$PLUGINSDIR\ojak-update"
  File /oname=aam.exe "${OJAK_HOOK_DIR}\..\binaries\installer-aam.exe"
  File /oname=aam-service.exe "${OJAK_HOOK_DIR}\..\binaries\installer-aam-service.exe"
  CreateDirectory "$INSTDIR"
  !insertmacro OJAK_EXEC "$PLUGINSDIR\ojak-update\aam.exe" "prepare"
  ; 이전 prepare가 중단된 경우 먼저 검증된 복구를 끝낸다. 실패 기록을 지우거나 prepare를 무한 반복하지 않는다.
  ${If} $0 != 0
  ${AndIf} $OjakFailureCode == "UPDATE_RECOVERY_REQUIRED"
    DetailPrint "Ojak: recovering the previous update before starting a new transaction."
    !insertmacro OJAK_EXEC "$PLUGINSDIR\ojak-update\aam.exe" "recover"
    ${If} $0 == 0
      !insertmacro OJAK_EXEC "$PLUGINSDIR\ojak-update\aam.exe" "prepare"
    ${EndIf}
  ${EndIf}
  ; 실행 파일 점유(INSTALLER_FILES_BUSY)일 때만, 그리고 화면이 있는 설치에서만 도우미가 직접 동의를 받는다.
  ; 무인(/S) 설치는 프로그램을 닫지 않고 아래에서 실패로 끝난다. 동의·정상 종료·강제 종료 확인은 모두 도우미가 처리하고
  ; prepare 트랜잭션까지 끝낸 뒤 0으로 돌아오므로 prepare를 다시 부르지 않는다.
  ${If} $0 != 0
  ${AndIf} $OjakFailureCode == "INSTALLER_FILES_BUSY"
  ${AndIfNot} ${Silent}
    DetailPrint "Ojak: executable is in use; asking permission to close the listed programs."
    !insertmacro OJAK_EXEC "$PLUGINSDIR\ojak-update\aam.exe" "prepare-interactive"
  ${EndIf}
  ${If} $0 != 0
    SetErrorLevel 1
    ${If} $OjakFailureCode == "INSTALLER_CLOSE_CANCELLED"
      Call OjakCancelled
      Abort
    ${EndIf}
    ${If} $OjakFailureCode == "INSTALLER_FILES_BUSY"
      StrCpy $2 "Ojak 실행 파일을 쓰는 프로그램이 아직 열려 있어 설치를 멈췄어요. Ojak 파일은 바꾸지 않았어요. 해당 프로그램을 종료한 뒤 설치 파일을 다시 실행해 주세요. 조용한(/S) 설치는 프로그램을 자동으로 닫지 않아요. / A program is still using Ojak's executable, so installation stopped. No Ojak files were replaced. Close that program, then run the installer again. Silent (/S) installs never close programs automatically."
    ${ElseIf} $OjakInstallerStage == "recover"
      StrCpy $2 "이전 업데이트 복구를 확인하지 못해 설치를 멈췄어요. 계정 데이터와 복구 기록을 보존했어요. 백업을 임의로 지우지 말고 아래 오류 코드와 로그를 확인해 주세요. / The previous update could not be safely recovered. Account data and recovery records were preserved. Do not delete the backups; check the error code and log below."
    ${ElseIf} $OjakInstallerStage == "prepare-interactive"
      StrCpy $2 "기존 Ojak을 안전하게 멈추지 못해 설치를 멈췄어요. Ojak 파일은 바꾸지 않았어요. 허용해서 이미 닫은 프로그램은 자동으로 다시 열리지 않아요. 아래 오류 코드를 확인한 뒤 설치 파일을 다시 실행해 주세요. / Could not safely stop the existing Ojak, so installation stopped. No Ojak files were replaced. Programs you already allowed to close are not reopened automatically. Check the error code below, then run the installer again."
    ${Else}
      StrCpy $2 "기존 Ojak을 안전하게 멈추지 못해 설치를 멈췄어요. 파일은 바꾸지 않았어요. 아래 오류 코드를 확인하고, 실행 중인 작업이 있으면 끝낸 뒤 설치 파일을 다시 실행해 주세요. / Could not safely stop the existing Ojak. No files were replaced. Check the error code below, finish any running work, then run the installer again."
    ${EndIf}
    Call OjakFail
    Abort
  ${EndIf}
  StrCpy $OjakUpdatePrepared 1
  SetOutPath "$INSTDIR"
!macroend

!macro NSIS_HOOK_POSTINSTALL
  !insertmacro OJAK_EXEC "$PLUGINSDIR\ojak-update\aam.exe" "finish"
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
  !insertmacro OJAK_LOCK "un."
  StrCpy $OjakInstallerStage "remove"
  StrCpy $OjakFailureCode "HELPER_MISSING"
  StrCpy $0 "not-started"
  ${If} ${FileExists} "$INSTDIR\aam.exe"
    InitPluginsDir
    ClearErrors
    CreateDirectory "$PLUGINSDIR\ojak-uninstall"
    CopyFiles /SILENT "$INSTDIR\aam.exe" "$PLUGINSDIR\ojak-uninstall\aam.exe"
    ${If} ${Errors}
      StrCpy $OjakFailureCode "HELPER_COPY_FAILED"
      SetErrorLevel 1
      StrCpy $2 "제거 도우미를 준비하지 못했어요. 앱 파일은 지우지 않았어요. / Could not prepare the uninstall helper. Application files were not removed."
      Call un.OjakFail
      Abort
    ${EndIf}
    !insertmacro OJAK_EXEC "$PLUGINSDIR\ojak-uninstall\aam.exe" "remove"
    ${If} $0 != 0
      SetErrorLevel 1
      StrCpy $2 "제거를 멈췄어요. 아래 오류 코드와 기록을 확인해 주세요. 앱 파일은 지우지 않았어요. 일부 연결은 이미 해제됐을 수 있어요. / Uninstall stopped. Check the error code and log below. Application files were not removed. Some integrations may already be disconnected."
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
