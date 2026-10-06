# Windows 이식 실측 조사 (2026-09-29)

대상: Windows 11 x64 개발용 PC(`<windows-pc>`). 원격 접속은 키 인증 SSH였고, 그 세션은 관리자 권한이었다.

## 빌드 환경

- 설치함: VS 2022 Build Tools(VCTools 워크로드), rustup → `stable-x86_64-pc-windows-msvc`, rustc/cargo 1.98.1.
- 있던 것: Node v24.21, Git, winget 1.29, WebView2 153.
- SSH 세션이 관리자 권한으로 실행된다(원격 winget 설치가 UAC 없이 됨).

## 공식 CLI 배치

| 도구 | 진입 경로 | 비고 |
|---|---|---|
| Claude Code 2.1.268 | `%USERPROFILE%\.local\bin\claude.exe` | symlink가 아니라 **복사본**(221,637,792 B, `versions\2.1.268`과 같은 크기). 업데이트하면 진입 경로 파일이 교체된다. 진입 경로를 저장하는 현재 규칙이 그대로 맞다. 설정은 `%USERPROFILE%\.claude`, `.claude.json`. |
| Codex 0.153.4 | `%APPDATA%\npm\codex.cmd`/`.ps1`/`codex`(sh) | npm shim. 실제 실행은 `node …\@openai\codex\bin\codex.js`. shim 판별과 원본 탐지를 `.cmd` 기준으로 새로 짜야 한다. 홈은 `%USERPROFILE%\.codex`(`auth.json`, `config.toml`, sqlite들). |
| omp | `%LOCALAPPDATA%\omp\omp.exe` | 자기 업데이트가 `omp.exe.<ts>.bak`을 남긴다. 설정은 `%USERPROFILE%\.omp`. |

- 사용자 PATH에 `%APPDATA%\npm`, `%LOCALAPPDATA%\omp`, `%USERPROFILE%\.local\bin`이 있다. shim 디렉터리는 사용자 PATH 맨 앞에 넣어야 한다(레지스트리 `HKCU\Environment\Path` + `WM_SETTINGCHANGE`).
- `CLAUDE_CONFIG_DIR`, `CODEX_HOME`은 기본으로 설정되어 있지 않다.

## 프로필 격리 실측 (2026-09-29)

빈 임시 폴더를 가리켜 상태만 조회했다. 기존 로그인은 건드리지 않았고 임시 폴더는 지웠다.

| 도구 | 기본 | 빈 폴더 지정 | 결론 |
|---|---|---|---|
| Claude (`CLAUDE_CONFIG_DIR`) | `loggedIn: true`, `claude.ai` | `loggedIn: false`, `projectsDirectory`도 지정 폴더 아래 | 격리됨. 자격 증명이 `%USERPROFILE%\.claude\.credentials.json` 파일에 있어(macOS Keychain 아님) 프로필 폴더 단위로 분리된다. 빈 폴더에 `.claude.json`, `backups`를 만든다. |
| Codex (`CODEX_HOME`) | `Logged in using ChatGPT`, exit 0 | `Not logged in`, exit 1 | 격리됨. 빈 폴더에 `tmp`를 만든다. |

- 남은 확인: 프로필 폴더에서 실제 `claude /login`·`codex login`으로 두 번째 계정을 넣었을 때 기본 프로필이 바뀌지 않는지(계정 로그인이 필요해 사용자 참여 후 진행).
- Windows는 자격 증명이 평문 파일이므로 프로필 폴더 ACL을 현재 사용자 전용으로 제한해야 한다(macOS의 0700에 해당).

## 부팅 식별자

macOS는 `kern.bootsessionuuid`로 "이전 부팅"을 판정한다. Windows 후보:

- `HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management\PrefetchParameters\BootId` = 47. 부팅마다 증가하는 카운터로 알려져 있지만 **재부팅 전후로 값이 바뀌는지 아직 검증하지 않았다**.
- `LastBootUpTime`, Kernel-General 이벤트 12의 시작 시각. 시계 보정 영향을 받는 wall-clock 값이다.
- `GetTickCount64`는 계속 증가하는 가동 시간이지 식별자가 아니다. 쓰지 않는다.

**규칙**: 안정적인 부팅 식별자를 재부팅으로 검증하기 전까지 Windows에서는 "이전 부팅이므로 슬롯 반환"을 판정하지 않는다. 슬롯 반환 근거는 Job Object로 확인한 종료만 쓴다.

## 구현 상태 (2026-09-29)

`cargo test --workspace --exclude ai-account-manager -- --test-threads=1` 결과: Windows 96개, macOS 131개 모두 통과.

### 데스크톱 앱 (Tauri)

- `node scripts/build.mjs`가 Windows에서도 동작한다(`.exe` sidecar 이름, `tauri.cmd`, 업데이터 끔 설정은 임시 파일 `tauri.local-build.conf.json`로 전달). `tauri.windows.conf.json`이 창을 기본 제목 표시줄로 바꾸고 번들을 NSIS로 정한다. 아이콘 `icons/icon.ico` 추가.
- 플랫폼 분기: 서비스 재시작은 `install::service_restart`(앱이 등록한 서비스만 종료 확인 후 재시작), 세션 열기는 새 콘솔 창에서 `aam`을 argv로 직접 실행(셸·스크립트 파일 없음), 링크 열기는 `explorer.exe`, 시스템 언어는 `GetUserDefaultLocaleName`. vibrancy가 없으므로 `.is-windows`에서 불투명 배경을 칠한다. 팝오버는 트레이 아이콘이 화면 아래쪽이면 위로 연다.
- 실측(설치된 앱, 로그온 세션, UI Automation으로 실제 버튼 클릭): 무인 설치 → 연결 → "서비스 설치·시작" 승인 → 서비스 실행·Run 값 등록 → "연결 파일 설치" 승인 → `integration.json`과 shim 생성 → 세션 → 새 관리 세션(Codex/Claude, 작업 폴더 지정) → 배정 확인 → "새 터미널에서 시작" → Windows Terminal에 관리 실행된 Claude Code가 열림, 세션 ACTIVE → Claude를 정상 종료하면 EXITED로 반환 → 제거 후 앱·데이터·Run 값·PATH 모두 원래대로. 트레이 아이콘과 팝오버는 아직 눈으로 확인하지 않았다.
- 이 실측에서 고친 결함: 앱이 `aam`(확장자 없음)을 찾아 모든 관리 버튼이 `INSTALLATION_ERROR`, 일반 권한 서비스가 관리자 권한 클라이언트의 프로세스 토큰을 열지 못해 파이프 연결이 끊김(서버는 파이프 가장으로 클라이언트 토큰 확인, 첫 요청 프레임을 읽은 뒤·처리 전에 검사), 로그온 환경에 `HOME`이 없어 서비스의 계정 조회가 시작되지 않음(`aam_protocol::user_home`이 `USERPROFILE` 우선), omp 상태 조회가 오류 배너를 띄움(Windows는 "연결 안 됨"으로 응답), GUI에서 부르는 `aam` 콘솔 창 깜박임(`CREATE_NO_WINDOW`), 새 콘솔에서 실행 실패 시 창이 바로 닫혀 오류를 못 읽음(`AAM_HOLD_ON_ERROR`). macOS 용어(LaunchAgent·zsh·메뉴바·⌘)는 Windows에서 별도 문구와 Ctrl 단축키로 바뀐다.

### 구현함

| 영역 | macOS | Windows |
|---|---|---|
| RPC 전송 | Unix socket(0600) + `getpeereid` | named pipe `\\.\pipe\aam-<fnv(AAM_HOME)>`. 보호된 DACL(현재 사용자 GA만), `FILE_FLAG_FIRST_PIPE_INSTANCE`로 이름 선점 거부, `PIPE_REJECT_REMOTE_CLIENTS`. 서버는 클라이언트, 클라이언트는 서버 프로세스의 토큰 사용자 SID를 현재 사용자와 비교(`protocol/src/ipc.rs`, `winutil.rs`). 항상 대기 인스턴스 하나를 두어 연결 사이에 "파이프 없음"이 생기지 않는다. |
| 앱 폴더·파일 권한 | 0700/0600, uid 검사, `O_NOFOLLOW` | 보호된 DACL `(A;OICI;GA;;;<user>)`(하위에 상속), 재분석 지점 거부, 단일 링크 일반 파일. 읽을 때 소유자는 현재 사용자 SID여야 한다. 기록(`install.rs` `atomic_write`, broker receipt)은 쓸 때 소유자를 현재 사용자 SID로 명시한다. 관리자 그룹 사용자의 승격 토큰(SSH·설치 실행)은 새 파일의 기본 소유자가 `Administrators`라서, 명시하지 않으면 다음 일반 권한 실행이 `UNSAFE_PATH`로 기록을 거부한다 (`protocol/src/secure.rs`). |
| 프로세스 identity | pid + 시작 시각 + `kern.bootsessionuuid` | pid + `GetProcessTimes` 생성 시각 + 레지스트리 BootId(저장만) |
| 부모·자손 판정(`lease.started`, 자식 세션) | `proc_pidinfo` ppid + 시작 시각 | Toolhelp 부모 PID → 그 PID의 현재 identity를 읽고, 자식보다 늦게 생긴 프로세스(재사용 PID)는 부모로 인정하지 않음(`service/src/process.rs` `windows_parent_of`) |
| 실행 감독 | 프로세스 그룹, 신호 전달, 터미널 foreground | 일시 정지로 만들어 **이름 있는 Job**(`Local\aam-job-<pid>-<생성시각>`, 소유자 전용)에 넣은 뒤 재개. 자손은 Job을 벗어날 수 없다. Ctrl+C는 실행기만 삼키고(핸들러, 상속 안 됨) 같은 콘솔의 native가 받는다. 실행기가 끝나도 Job은 프로세스를 죽이지 않는다(`launcher/src/supervisor.rs`, `descendants.rs`). |
| 종료 근거 | 기록된 자손 Dead + 그룹 ESRCH, 또는 이전 부팅 | 서비스가 `lease.started` 때 Job을 이름으로 열어 핸들을 쥔다. **활성 프로세스 수 0**만 종료 근거다(`lease.release`와 reconcile 모두). 이름은 마지막 핸들이 닫히면 살아 있는 프로세스가 있어도 사라지므로 "열 수 없음"은 근거로 쓰지 않는다(테스트 `protocol/tests/windows_job.rs`). 서비스 재시작으로 핸들을 잃으면 근거가 없어 슬롯을 유지한다. 부팅 기반 반환은 하지 않는다. |
| 공식 CLI 탐지 | 실행 비트, shebang shim 검사 | `claude.exe`·`codex.cmd` 등 확장자로 탐지, 배치 shim 내용 검사. 탐색 경로: `%USERPROFILE%\.local\bin`, `%APPDATA%\npm`, `%LOCALAPPDATA%\omp`. `.cmd`는 Rust 표준 라이브러리의 배치 인수 이스케이프(안전하게 못 하면 실행 거부)로 실행한다. |
| 상태 조회 probe | non-blocking fd, 그룹 SIGKILL | 읽기 스레드 + 전체 시간 제한, `kill_on_close` Job(새 콘솔 그룹)으로 조회 자손까지 정리 |
| 원본 CLI로 넘김 | `exec` | 같은 콘솔에서 실행하고 종료 코드로 끝냄 |
| 콘솔 한글 | — | 시작 시 출력 코드 페이지를 UTF-8로 설정 |

실측 스모크(임시 `AAM_HOME`): 서비스 기동 → `aam status` 성공, 연속 호출·동시 5개 호출 모두 성공, 두 번째 서비스는 `SERVICE_ALREADY_RUNNING`으로 거부, `AAM_HOME` ACL은 보호됨·현재 사용자 항목만.

### 설치 (`crates/launcher/src/install_windows.rs`)

| 명령 | Windows 동작 |
|---|---|
| `aam integration install` | 서비스의 첫 계정 조회가 끝나길 기다린 뒤(최대 60초) 계정이 준비된 도구의 원본 CLI 진입 경로를 기록하고, `AAM_HOME\bin\claude.exe`·`codex.exe`를 launcher 복사본으로 만든다(symlink는 관리자·개발자 모드 필요). launcher는 argv0 파일 이름(확장자 제외, 대소문자 무시)으로 shim 모드를 고른다. 앱이 만든 적 없는 파일이 있으면 `SHIM_CONFLICT`로 멈춘다. 실행 중인 shim은 이름을 바꿔 두고 다음 설치에서 지운다. |
| `aam shell install` | 사용자 `HKCU\Environment\Path` 맨 앞에 `AAM_HOME\bin`을 넣고(값 형식 보존) `WM_SETTINGCHANGE`를 알린다. 제거는 그 항목만 뺀다. |
| `aam service install` | `HKCU\...\Run`의 `Ojak Service` 값으로 로그인 시 `aam-service.exe`를 실행하고, 지금 콘솔·작업과 분리해 시작한다. 이전 `Ojak` 값은 소유 기록이 일치할 때만 이관한다. 데스크톱 앱 자동 실행 값과 충돌하지 않는다. |
| `aam service stop`/`uninstall` | macOS와 같은 안전 절차(`service.prepareUninstall` permit, 활성·불확실 lease 검사) 뒤 파이프 서버 프로세스(같은 사용자 확인)를 종료한다. 제거는 앱이 쓴 Run 값만 지운다. |

원본 CLI 탐색(`adapters::discover`)은 현재 `AAM_HOME\bin`을 제외한다. 다른 AAM_HOME의 복사본 shim도 상위 `integration.json`의 앱 owner·shims 목록으로 확인해 거부한다.

`shell install`은 시스템(HKLM) PATH에 원본 CLI가 있으면 경고한다. Windows는 시스템 PATH를 사용자 PATH보다 먼저 찾으므로 그 도구는 shim이 가려진다.

실측 스모크(임시 `AAM_HOME`, 실제 사용자 레지스트리): 설치 → 별도의 새 SSH 로그온 세션(레지스트리로 환경을 새로 구성)에서 PowerShell `Get-Command`와 `cmd /c where` 모두 `claude`·`codex`를 shim으로 해석(이 PC는 시스템 PATH에 원본 없음). 테스트 프로세스에서는 `codex`/`claude`가 shim으로 해석 → `claude --version`은 원본으로 통과 → `codex exec`가 관리 실행(응답 OK, 세션 EXITED) → 제거 후 사용자 Path가 원래 값과 바이트 단위로 같고 Run 값 삭제, 서비스 프로세스 없음. 로그인 시 자동 실행은 Run 값 등록까지만 확인했다(로그오프·재로그인 미실행).

업데이트 구현(2026-10-01): NSIS hook이 `installer prepare`로 앱 소유 서비스의 활성·불확실 lease를 확인하고 파일을 백업한 뒤 중지한다. 교체 후 `finish`가 launcher/service SHA-256을 확인하고 서비스를 재시작하며, 종료 permit에 대응하는 RPC로 DB의 신규 배정 차단도 해제한다. 실패 시 `recover`로 검증된 원본을 복구한다. 임시 설치 디렉터리의 실제 서비스로 불완전 교체 거부·원본 복구·정상 교체·배정 재개·복구 기록 정리를 확인했다. 실제 사용자 설치본의 최종 검증은 아래 V3 실행 승인 대기로 남아 있다.

### 플랫폼 차이와 남은 검증

- (2026-09-29 구현 완료로 제거) shim·자동 실행·PATH는 아래 "설치" 표 참고.
- omp broker·bridge·observer와 외부 OMP 관측은 Windows 구현 및 빌드가 추가됐다. broker는 현재 사용자 Task Scheduler 작업, observer는 ACL을 검사하는 native writer, 세션 관측은 정확한 프로세스와 열린 쓰기 파일 핸들을 사용한다. 실제 저장 세션을 연 공식 OMP에서 세션 파일·attribution·스냅샷까지 확인했다. Windows 계정 broker 연결과 실제 모델 요청의 최종 설치본 검증은 V3 실행 승인 후 진행해야 한다.
- cmux NODE_OPTIONS 예외는 Windows에서 충돌로 멈춘다. Claude self-exec wrapper(`CLAUDE_CODE_PROCESS_WRAPPER`)는 설정하지 않으며, Claude 재실행은 같은 Job·같은 프로필 환경을 물려받는다.
- 부팅 기반 슬롯 반환(BootId 재부팅 검증 전).

### 2026-09-29 당시 검증 범위

- 실제 계정 관리 실행: 임시 `AAM_HOME`에서 기본 프로필 자동 인식 → `aam integration install` → `aam run codex -- exec ... "Reply with exactly: OK"` 응답 OK, exit 0, 세션 EXITED(2026-09-29). Claude는 테스트 계정의 주간 한도가 임박하고 5시간 리셋 시각이 미확인(`RESET_UNCONFIRMED`)이라 정책대로 제외되어 실행하지 않았다. 계정 없이 확인한 것: 실제 자식으로 starting→started(ACTIVE)→release(EXITED), 실행기 유실 시 손자가 끝난 뒤에만 반환(`service/src/tests.rs` `windows_*`), 실행기 감독이 남은 손자를 기다림(`launcher/src/supervisor.rs`), 서비스·파이프 스모크.
- 다른 Windows 사용자의 접속 거부는 계정을 만들지 않고 설계(보호 DACL + 토큰 SID 비교)로만 확인했다.

### 2026-10-01 최종 검증과 실행 승인 대기

- Windows Rust 132개, native writer를 사용한 observer 8개 통과. helper 경로 canonicalization 수정 뒤 launcher 34개와 NSIS 빌드도 통과했다.
- macOS Rust 148개, desktop 13개, observer 8개 통과. canonicalization 수정 뒤 launcher 40개 및 앱/DMG 재빌드, 앱·shim 양쪽의 observer `current: true`, 실제 앱의 준비 완료 화면을 확인했다.
- 양쪽 OS의 격리 HOME/AAM_HOME에서 CLI 없음·계정 없음과 실제 CLI 있음·로그인 없음 상태를 각각 실행했다. 모두 준비 완료를 잘못 표시하지 않았다.
- macOS 실제 OMP의 `ojak-codex/gpt-6-astra` 요청은 `OK`로 완료했다. 별도 native Codex 관리 실행도 `OK`, exit 0, lease `EXITED`로 완료했다. 프로필이 충돌하는 호출은 `HOST_PROFILE_CONFLICT`와 `ABORTED`로 안전하게 중단했다.
- Windows NSIS 실행 뒤 launcher/service는 해당 빌드와 SHA-256이 일치했고, GUI는 Tauri의 `__TAURI_BUNDLE_TYPE_VAR_` 뒤 `UNK`→`NSS` 3바이트만 달랐다. 원시 GUI hash 비교로 구버전 설치라고 판단하면 안 된다.
- **현재 설치본 완료 판정 불가:** AhnLab V3 Lite의 **프로그램 실행 알림**이 설치 경로의 `aam.exe` 실행을 보류한다. `--version`도 같은 상태이고, V3 창의 클라우드 평판·행위 기반 실행 알림을 직접 확인했다. 실행 허용·예외 추가·보안 기능 해제는 하지 않았다. 이후 canonicalization을 포함한 최종 NSIS는 빌드했지만 재설치는 승인 해결 전 보류했다.
- 사용자의 실행 허용 완료 통보 뒤 설치본 상태 조회를 한 번 재개했지만 반환되지 않았고, V3에 새 `1/1` 실행 알림이 남아 있음을 확인했다. 추가 실행을 만드는 검증 프로세스는 종료했다. 파일별 지속 허용 여부는 사용자가 결정하며, 에이전트가 실행 허용이나 보안 예외를 자동 적용하지 않는다.
- 두 번째 승인 완료 통보 후 설치 경로의 `aam.exe --version`은 `aam 0.1.0`으로 정상 종료했다. 상태 조회는 `DAEMON_UNAVAILABLE`, NSIS 재설치는 exit 1이었다. 이어 `service install`은 30초 내 반환되지 않았고, V3 화면에서 이번에는 **`aam-service.exe`** 실행 알림 `1/1`을 확인했다. 서비스 실행 승인이 남아 있으며 최종 설치·서비스 복구 성공으로 판단하지 않는다.
- 따라서 Windows 최종 서비스 재개·OMP 연결·실요청·전체 트레이/팝오버 상호작용은 미완료다. macOS 주요 화면·개인정보 가림·준비 완료는 확인했으나 팝오버 표시의 시각 확인은 완료하지 못했다. 새 계정 OAuth, 로그아웃/재부팅, 실제 공급자 장애·장시간 동작도 별도 실측이 필요하다.
- NSIS를 PowerShell에서 관찰할 때 `Start-Process -Wait`는 서비스 자손까지 기다릴 수 있다. 설치 프로세스 자체의 `WaitForExit()`와 별도의 서비스 상태 확인을 구분한다.

### 2026-10-01 후속 복구: broker XML 인코딩

- `service install` 성공 후 설치본 서비스 및 계정 4개를 확인했다. 이전 실행 승인 대기 기록과 달리 이 시점에는 서비스가 정상 응답한다.
- `BROKER_AGENT_FAILED`는 `schtasks /Create /XML`에서 `The task XML is malformed. (1,40) 인코딩을 전환할 수 없습니다`로 재현했다. 작업 정의를 UTF-16LE BOM과 일치하는 XML 선언으로 저장하도록 `omp_broker_windows.rs`를 수정했다.
- Windows release launcher 빌드 및 launcher 테스트 34개 통과. 실제 broker 작업 등록·실행, 인증된 snapshot HTTP 200을 확인했다. 첫 실행은 준비 시간 내 응답하지 않아 `BROKER_NOT_READY`였지만 이후 broker 정상 응답을 확인하고 연결을 완료했다.
- 수정 launcher를 설치 경로에 반영하고 `integration install`, 기존 관리형 observer 제거·재설치 후 **설치본** 조회에서 `ready: true`, broker `connected/supervised: true`, bridge `connected: true`, observer `installed/current: true`를 확인했다. 확장 변경은 새 OMP 실행부터 적용된다. `observed-session-discovery` notice는 남아 있다.
- 별도의 V3 악성코드 차단 화면은 `Execution/MDP.Powershell.M2514`, 대상 `powershell.exe`, 처리 `프로세스 종료`였다. Microsoft 서명은 Valid이며 진단용 `-EncodedCommand` 이벤트가 확인됐다. 진단 명령과 관련됐을 가능성이 높지만 V3 상세 로그와의 일대일 연결은 미확인이다. 해당 실행 방식을 중단했고 보안 예외·해제는 하지 않았다.
- 후속으로 수정 포함 NSIS를 재빌드하고 실행 중 GUI를 종료한 뒤 정식 재설치에 성공했다. 설치 launcher/service SHA-256이 release와 일치했고 설치 후 service·broker·bridge·observer 준비 상태가 유지됐다. Windows OMP 18.4.2에서 최초 요청은 Ojak provider 미로그인으로 모델을 찾지 못했다. 공식 `omp login ojak-codex` 성공 후 `omp -p --no-tools --model ojak-codex/gpt-6-astra --no-session "Reply with exactly OK"`가 `OK`, exit 0으로 완료됐다. 새 계정 OAuth가 아니라 기존 Ojak 브릿지 공급자 활성화이며, 저장 세션 관측·전체 UI 검증을 대신하지 않는다.

### 2026-10-03 후속 복구: 승격 실행이 남긴 기록 소유자

- 일반 권한으로 처음 실행할 때 `UNSAFE_PATH`(앱 설치 기록의 소유권 또는 파일 형식이 올바르지 않습니다)가 났다. `integration.json`·`service-install.json`의 소유자가 `BUILTIN\Administrators`였다. 승격된 SSH 세션의 설치·배포가 남긴 파일이다.
- 읽기 쪽 검사는 완화하지 않았다. 대신 `atomic_write`는 임시 파일에, broker receipt 저장은 쓴 파일에 `restrict_file`을 적용해 승격 실행에서도 소유자를 현재 사용자로 남긴다. 회귀 테스트 `install::windows_tests::saved_records_are_owned_by_the_user_even_from_an_elevated_writer`는 승격 SSH에서 통과했다.
- 기존 파일 4개는 `icacls /setowner`로 사용자 소유로 되돌렸다. 새 NSIS 설치본을 승격 실행으로 배포한 뒤에도 모든 기록의 소유자가 `<user>`로 유지됐다. Task Scheduler `RunLevel Limited`로 실행한 일반 권한 `service install`·`setup --check`는 exit 0이었고, `UNSAFE_PATH` 없이 `ready/service/configured: true`를 반환했다.
- 같은 원인이 omp 확장에도 있었다. 승격 실행으로 설치된 `~/.omp/agent/extensions/aam-accounts`·`aam-observer`의 `index.js`·`.aam-owner.json` 소유자가 `Administrators`라서, 일반 권한 `setup --with-omp`가 `EXTENSION_FAILED`로 끝났다(앱의 [시작하기]가 반응 없어 보임). `omp_extension.rs` `write_new`와 대화 기록 복사(`adapters` `copy_private`)도 쓸 때 `restrict_file`로 소유자를 현재 사용자로 명시한다. 회귀 테스트 `omp_extension::windows_tests::extension_files_are_owned_by_the_user_even_from_an_elevated_writer`는 승격 SSH에서 통과했다. 기존 확장 파일 4개는 `icacls /setowner`로 되돌렸고, 일반 권한 `setup --with-omp`가 `ready: true`, broker·bridge·observer `true`, `error: null`을 반환했다.
- 승격 SSH로 띄운 서비스(세션 0)는 일반 권한 클라이언트가 붙지 못해 `service: false`·`SERVICE_START_TIMEOUT`이 됐다. 검증할 때는 서비스를 일반 권한(Task Scheduler `RunLevel Limited`)으로 다시 띄운다. cmd 래퍼의 종료 코드는 `/v:on`과 `!ERRORLEVEL!`로 읽어야 한다(`%ERRORLEVEL%`은 항상 0).
- omp 터미널 창이 계속 늘어났다. `aam-service`는 창 없는 프로그램이라, 콘솔 프로그램인 `omp auth-gateway`를 띄울 때마다 Windows가 새 콘솔(기본 터미널이 Windows Terminal이면 새 탭)을 열었다. gateway는 서비스가 끝나도 살아남아 재시작마다 4개씩 쌓였다(관측 28개, 부모 서비스는 모두 종료). `bridge.rs` `spawn_gateway`가 이제 `spawn_in_job(kill_on_close, new_group)`으로 띄운다. `new_group` 자식은 `CREATE_NO_WINDOW`로 만든다. 서비스를 강제 종료한 뒤 gateway 0개, 재시작 뒤 서비스 소유 4개만 남았고, 새 gateway는 Windows Terminal 탭을 만들지 않았다.
- broker 작업(`omp auth-broker serve`)도 터미널 창을 열었다. 작업 정의를 `conhost.exe --headless "<omp.exe>" auth-broker serve`로 바꿨다(이 개발 PC의 Windows 11에서 창·탭 없음 확인). 앱 소유 receipt가 일치하는 이전(창 있는) 정의는 `supervised: false`로 보고 `setup`이 `connect`로 지우고 다시 만든다. 관리자 권한으로 만든 작업은 일반 사용자에게 읽기 권한만 있어 지울 수 없으므로 `BROKER_AGENT_CONFLICT`로 멈추고 아무것도 바꾸지 않는다(실행 중인 broker도 끝내지 않음). 이 개발 PC의 이전 작업은 관리자 권한 SSH로 한 번 지웠다.
- 결과(일반 권한): broker 부모 `conhost.exe(--headless)`, Windows Terminal 탭 0개, gateway 4개, broker `connected/supervised: true`. 첫 `setup --with-omp`는 broker 재시작 직후라 `BRIDGE_NOT_READY`였고, 다시 실행하면 `ready: true`·`error: null`이었다.
- 배포 스크립트는 NSIS가 승격 토큰으로 다시 띄운 서비스를 `aam service stop`으로 내리고 Task Scheduler `RunLevel Limited`로 `service install`·앱을 다시 띄운다. 세션 0 서비스가 남으면 실패한다.
- 소유권 회귀 테스트 2개(`install`, `omp_extension`)는 수정 줄을 뺀 소스에서 실제로 FAILED, 원본에서 통과함을 확인했다. 서비스의 `git rev-parse`도 `CREATE_NO_WINDOW`로 띄운다.


## 컴파일 실측 (이식 전)

소스는 Windows 작업 트리 복사본(커밋 `2e03913` + 미커밋 변경 + untracked, git 저장소 아님)이었다. `cargo check -p aam-protocol`이 첫 단계에서 멈춘다(`std::os::unix`, `Permissions::from_mode` 4곳). 나머지 크레이트는 protocol에 막혀 오류를 다 보지 못했다. Unix 전용 사용처 수(파일별 대략):

| 파일 | 수 | 대체 |
|---|---|---|
| `launcher/src/supervisor.rs` | 44 | Job Object, `CREATE_NEW_PROCESS_GROUP`, 콘솔 Ctrl 이벤트 |
| `adapters/src/observed.rs`, `observed_metadata.rs` | 28, 12 | Toolhelp32 / `NtQueryInformationProcess`로 프로세스 목록·명령줄 |
| `service/src/process.rs`, `protocol/src/process.rs`, `launcher/src/descendants.rs` | 12, 9, 12 | 프로세스 identity = pid + 생성 시각(`GetProcessTimes`) + 부팅 식별자, 자손 = Job Object |
| `service/src/server.rs`, `protocol/src/lib.rs` | 7, 6 | Named pipe + `GetNamedPipeClientProcessId`/토큰 SID 확인, ACL로 권한 제한 |
| `launcher/src/install.rs`, `shell.rs`, `lib.rs` | 5, 4, 4 | 로그인 실행(작업 스케줄러 또는 `HKCU\…\Run`), 사용자 PATH, `.exe`/`.cmd` shim |
| 그 외 | 1–6 | 파일 권한은 ACL, `SIGPIPE`류는 제외 |

## 다음 단계

1. 두 번째 계정 실제 로그인으로 격리 최종 확인 + `aam run` 관리 실행 끝까지(사용자 참여).
2. shim(`claude.exe`/`codex.cmd` 대체)·로그인 시 서비스 자동 실행·사용자 PATH 설치.
3. BootId 재부팅 검증(사용자 동의 후 재부팅 1회).
4. Tauri 앱: 메뉴바 → 트레이 아이콘, 창 스타일(`titleBarStyle`, `macOSPrivateApi`) 분기, NSIS/MSI 번들.
