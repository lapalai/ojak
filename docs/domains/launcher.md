# launcher (`crates/launcher`)

## 개요
`aam` 바이너리 하나가 clap CLI, argv0 기반 `claude`/`codex` shim, Claude process wrapper를 겸한다. 관리 실행의 lease 수명주기를 돌리고, LaunchAgent·shim·zsh PATH 설치를 맡는다.

## 실행 모드 (`src/main.rs:475-506`)
1. `--claude-process-wrapper` / `--claude-native-exec` → Claude 자기 재실행 검증 (`lib.rs:688-797`)
2. argv0이 `claude`/`codex`(Unix symlink) 또는 `--shim claude|codex`(Windows `.cmd` shim) → `inspect_cli` 우회 확인 후 `run_cli`
   - Windows shim은 launcher `.exe` 복사본이었으나, 원본과 이름이 같은 서명 없는 `claude.exe`를 Defender가 `Behavior:Win32/Execution.A!ml`로 격리해 배치 파일로 바꿨다. 설치·업데이트 때 남은 앱 소유 `.exe` 복사본을 지운다. `aam.cmd`는 launcher를 그대로 부르고, `claude.cmd`·`codex.cmd`만 `--shim`을 붙인다. 배치는 OEM 코드 페이지로 읽히므로 사용자 폴더는 `%LOCALAPPDATA%` 등으로 적고, 나머지 경로가 ASCII가 아니면 `SHIM_PATH_UNSUPPORTED`로 멈춘다.
3. 그 외 → `aam <command>`

## 명령 (`main.rs:24-78`)
`status`, `doctor`, `refresh`, `explain --tool`, `run <claude|codex> [-- native args]`, `account {add, login, settings-preview}`, `service {install, uninstall, stop, restart, status}`, `integration {install, uninstall}`, `omp-broker|omp-bridge {status, connect, disconnect}`, `omp-observer {status, install, uninstall}`, `takeover {list, adopt, release}`, `shell {install, uninstall}`.

## 관리 실행 흐름 (`lib.rs`)
shim → `route.resolve` → `lease.acquire`(PREPARED) → 실행 계획·identity 재확인(`IDENTITY_MISMATCH`) → `lease.starting` → supervisor로 spawn → `lease.started` → 10초 heartbeat → release/abort.
- `lease.starting` 응답이 유실돼도 spawn을 재시도하거나 예약을 돌려주지 않는다 (`lib.rs:588-602`).
- Claude 새 세션에는 `--session-id`를 항상 붙인다.
- 자기 재실행은 계정의 현재 해석 경로 또는 `AAM_CLAUDE_ROOT_PROGRAM`(세션을 시작한 실행 파일)과 같아야 한다. 세션 도중 `claude install`로 버전이 바뀌어도 기존 세션이 끊기지 않게 하기 위함.
- 격리 프로필은 실행 직전 `adapters::share_extensions`로 기본 폴더의 확장을 연결한다. Codex(`~/.codex`)는 `skills`·`agents`·`rules`·`plugins`·`prompts`·`AGENTS.md`·`hooks.json`을 연결하고 config.toml에 `plugins`·`marketplaces`·`mcp_servers`·`hooks`·`notify`·`features`만 옮긴다(`shell_environment_policy`·모델·프로젝트 신뢰·`auth.json`은 제외). Claude는 `~/.claude`의 `skills`·`agents`·`commands`·`plugins`·`hooks`·`output-styles`·`CLAUDE.md`를 symlink로 연결하고(이미 링크면 그대로, 내용 있는 폴더는 `backups/<이름>.before-shared-<ms>`로 옮김), settings.json에는 `enabledPlugins`·`extraKnownMarketplaces`·`hooks`·`statusLine`만 복사한다(기본에서 빠지면 프로필에서도 제거). `env`·인증 키·`.claude.json`(로그인·user MCP)은 공유하지 않는다. 합친 설정도 기존 `check_settings`를 거친다. 실패는 경고만 하고 실행은 계속한다. Windows는 symlink 권한 문제로 폴더 연결 없이 설정 키만 옮긴다.
- 관리 실행은 자식에게 `AAM_PARENT_SESSION_ID`·`AAM_PARENT_CAPABILITY`를 넘긴다. 이 값을 물려받은 셸이 부모 native의 자손이 아니면(관리 세션 안에서 띄운 터미널 앱·tmux 서버가 퍼뜨린 경우), 서비스가 `PARENT_SESSION_UNKNOWN`(acquire) 또는 `PARENT_PROCESS_UNVERIFIED`(starting)를 명시적으로 돌려줄 때만 예약을 풀고, 두 변수를 지운 뒤 부모 없이 한 번 다시 배정한다. native spawn 전 단계라 spawn 재시도 금지와 충돌하지 않는다. 진짜 자손은 그대로 부모 계정을 쓴다.
- 터미널(stdout·stderr가 모두 TTY)에서 관리 실행이 시작되면 stderr에 고른 계정 이름을 한 줄 적는다. 고정·한도 전환·수동 계정 불가일 때만 이유를 붙인다. 이메일은 `ui-settings.json`의 `privacy`가 `visible`일 때만 넣고, 없으면 가린다. 스크립트에는 이 줄을 찍지 않는다.
- 한도를 다 쓴 뒤 [Y/n] 이어가기 질문을 띄울 수 없으면(터미널이 아니거나 부모 세션) `aam continue` 한 줄을 남긴다.

## 연결 확장 (`extend_integration`)
- 설치 스크립트는 계정이 없을 때 `aam integration install`을 돌리므로 그때는 `aam` shim만 생긴다. 계정 등록이 끝나면(`aam account login`·`aam account add`·앱의 `account.register`) 연결 기록(integration.json)이 있을 때만 `aam integration extend --tool <도구>`로 새로 실행 가능해진 도구의 shim을 더한다. 서비스 시작 직후 도구 탐지가 끝나지 않았으면 최대 20초 기다린다. 연결을 설치한 적이 없으면 아무것도 바꾸지 않는다.

## 인수 검사 (`src/arguments.rs`)
- 관리 실행은 허용 목록에 있는 옵션만 받는다. 인증·프로필·공급자·엔드포인트 옵션은 `AUTH_OVERRIDE_CONFLICT`. 확인하지 않은 옵션과 세션 선택기는 `NATIVE_OPTION_UNSUPPORTED`. 오류는 옵션·하위 명령 이름만 말하고 값은 출력하지 않는다. 데스크톱은 이 코드를 따로 분기하지 않고 메시지와 코드 문자열만 보여 준다.
- 세션에 영향 없는 플래그(`--add-dir`, `--debug`, `--verbose`, `--search` 등)는 허용한다. `--bare`, `--mcp-config`, `--settings`(호스트 hook 제외), Codex `-c`/`--config`/`--oss`/`--profile`은 막는다. `--bg`처럼 계정 예약을 벗어날 수 있는 플래그도 막는다.
- 첫 위치 인수가 `auth`·`login`·`logout`·`setup-token`·`config`·`settings`·`profile`이면 `AUTH_OVERRIDE_CONFLICT`이고, 계정 추가는 앱의 연결 > 계정 추가 또는 `aam account login`을 안내한다. `resume`은 `aam continue`를 안내한다. `mcp`·`plugin`·`doctor` 등은 shim의 `inspect_cli`가 먼저 원본으로 넘긴다. `aam run`으로 오면 도구 명령으로 실행하라고 안내하고 세션을 시작하지 않는다.
- 모델은 `claude --model`, `codex --model`(또는 `-m`), `aam run --model`로 지정한다. shim이 빼서 배정에 쓴다.

## 원본 통과 (`lib.rs`)

`NO_ELIGIBLE_ACCOUNT`, `QUOTA_*`, `DAEMON_UNAVAILABLE` 등 "관리할 수 없음" 코드일 때만, shim 모드에서만 원본 CLI로 넘긴다. 사용자가 건 제한(`ROUTE_CONFLICT` 등)과 `AUTH_OVERRIDE_CONFLICT`·`NATIVE_OPTION_UNSUPPORTED`는 이 목록에 넣지 않는다. `aam run`은 통과하지 않는다.
계정이 모두 한도를 다 써 `NO_ELIGIBLE_ACCOUNT`·`QUOTA_EXHAUSTED`로 원본에 넘길 때는 '배정 설명' 화면을 가리키지 않는다. Ojak 계정이 모두 소진된 시각(가장 빠른 리셋), 원본 CLI가 쓸 기본 프로필 계정, `aam explain --tool`을 한 번에 적는다.
원본으로 그냥 넘기면 공급자가 구독 한도 뒤에 크레딧(Codex)·추가 사용량(Claude, API 요금)을 쓸 수 있다. Ojak의 해당 옵트인(`use_credits_after_limit`/`use_extra_usage_after_limit`)이 꺼져 있는데 원본 CLI가 쓸 기본 프로필 계정에 공급자가 명시한 과금 경로가 있고(크레딧 `available`·`ordinaryUsageAllowed!=true` / 추가 사용량 `enabled`이고 상한 미도달) 구독 한도가 지금도 남았다고 확실하지 않으면(소진·낡은 관측·미확인 포함, 더 묻는 쪽이 안전) `passthrough`가 조용히 넘기지 않는다. Ojak 설정이 꺼져 있고 원본 CLI가 과금될 수 있다는 한국어 안내를 먼저 보이고, 터미널이면 `[y/N]`(기본 N)로 확인, 터미널이 아니면 `SPEND_CONFIRMATION_REQUIRED`로 실행을 거절한다. 거절하면 `SPEND_CONFIRMATION_DECLINED`. 한도가 남았거나 과금 경로가 없거나 관측이 없으면 기존 통과 동작 그대로다. 이 확인은 Ojak이 크레딧으로 배정하지 않겠다는 설정과 안내일 뿐이다. 이미 실행 중인 원본 CLI의 공급자 쪽 과금 전환은 Ojak이 막을 수 없고, 쓴 금액도 볼 수 없다. 안내와 확인 문구에 그 한계를 적는다. Ojak이 크레딧·추가 사용량으로 배정하면 TTY 안내 줄(`announce_chosen_account`)이 "구독 한도를 다 써서 크레딧을 쓰고 있어요"(Claude는 추가 사용량)로 바뀐다.

## 설치 (`src/install.rs`)
- `integration.json`(`version: 2`)은 원본 CLI **진입 경로**를 저장한다. 판이 없는 이전 기록의 `prior`/서비스 관측 경로는 다시 고정하지 않는다 (`install.rs:292-320`).
- shim은 Unix에서 `AAM_HOME/bin/{aam,claude,codex}` → 앱의 `aam` symlink, Windows에서 같은 이름의 `.cmd`다. `aam`은 계정이 없어도 만든다. 사용자 파일이 있으면 `SHIM_CONFLICT`로 멈춘다.
- LaunchAgent `ai.aam.service`. `aam-service`는 `aam`과 같은 폴더에 있어야 한다.
- zsh PATH 블록: `shell.rs` (`shell-integration.json`).
- 통합 준비: `aam setup`은 누락된 서비스·shim·셸 설정을 설치하고 새 로그인 셸에서 명령을 확인한다. `--with-omp`는 동의한 사용자를 위해 기존 broker → bridge → observer 안전 설치 경로도 실행한다. 충돌은 중단하며 omp 단계 오류는 JSON `omp.error`에 남긴다. 설치 함수가 돌려준 성공 문장은 버리고, `주의:` 경고와 명령 확인 실패는 JSON `notices`에 단계·코드·다음 행동과 함께 남긴다.
- `aam setup --status`는 설정 조회만 하며 셸 시작 파일을 실행하지 않는다. `--check`는 설치 없이 명령을 재검증한다. `configured`는 설정 파일 기준, `tools[].verified`는 실제 검사(true/false/null), `ready`는 명시적 검사 완료다. 기존 터미널·IDE·omp 세션 전환이나 실모델 요청 성공을 뜻하지 않는다. `notices`는 실패한 단계의 이유와 다음 행동이다.
- Unix 명령 검사는 `$SHELL`의 `/bin/zsh`, `/bin/bash`, `/bin/sh` 로그인 셸에서 `command -v`가 관리 shim과 같은 파일인지(`-ef`, symlink 허용) 확인한다. 그 외 셸은 `unsupported-shell`로 지원 셸과 직접 넣을 PATH를 알린다. 상속 PATH의 Ojak 경로를 먼저 제거하며, 명령별 5초 제한 후 프로세스 그룹을 종료한다. 셸 출력은 버리고 구조화된 결과만 보고한다. 인수 없는 `aam`은 짧은 도움말을 출력한다. 사용법 오류는 빠진 인수·알 수 없는 명령처럼 종류와 사용법 틀만 알리고 입력 값은 출력하지 않는다. shim 경로는 바꾸지 않는다.
- Windows 명령 검사는 별도 프로세스를 띄우지 않는다. 레지스트리의 저장된 시스템 PATH → 사용자 PATH 순서와 PATHEXT로 첫 실행 파일을 찾아 관리 shim과 비교한다(`install_windows.rs` `effective_command`). 숨긴 PowerShell 실행은 서명 없는 바이너리에서 Defender 행위 탐지(`Behavior:Win32/Execution.A!ml`)를 불러 제거했다. 기존 터미널의 별칭·프로필·IDE 환경까지 확인한 것은 아니다.
- 서비스 버전 맞춤 (`src/service_version.rs`): 앱을 DMG로 덮어쓰면 `service_install`은 같은 실행 파일 경로의 서비스를 그대로 두므로(재시작 없음) 화면은 새 버전인데 서비스는 예전 프로세스일 수 있다. 서비스는 `Snapshot.serviceVersion`(`CARGO_PKG_VERSION`)을 알리고, `aam`은 자기 버전과 비교한다(`compare`: `current`·`older`·`newer`·`unknown`). 이 필드가 없던 이전 서비스는 `unknown`이며 `older`와 함께 재시작 대상(`mismatch: true`), 더 새 서비스는 되돌리지 않는다. `aam setup --status|--check` JSON은 `serviceVersionMismatch`와 notice `service-version-mismatch`(step `service-version`)를, `aam service status`는 `서비스 버전` 줄과 다음 행동을 보여 준다. 조용히 재시작하지 않는다.
- `aam service restart` (`service_version::restart`): 같은 버전이면 아무것도 안 한다. 아니면 `service.prepareUninstall`(stop과 같은 lease 검사: 점유·불확실 lease가 있으면 `SESSION_BUSY`로 멈추고 아무것도 바꾸지 않음)으로 새 배정을 닫고 → macOS `launchctl kickstart -k`(`install::service_kickstart`: 앱이 설치한 기록·plist가 그대로일 때만) / Windows 기존 `service_restart`(안전 종료 + 기록된 실행 파일 시작) → 시작 시각이 바뀐 새 프로세스가 응답하면 `service.resumeAdmission` → 버전이 앱과 같은지 확인한다. 시작 실패 시 배정을 다시 열고 기존 서비스를 둔다. 새 프로세스를 20초 안에 확인하지 못하면 `SERVICE_NOT_READY`, 재시작했는데 버전이 다르면 `SERVICE_VERSION_STALE`. 시간 초과만으로 lease를 풀지 않는다.
- macOS와 Windows 모두 `ompSupported: true`이며 같은 준비 흐름을 사용한다. 지원 여부와 실제 broker·bridge·observer 연결 완료는 별도 값이다.
- Windows NSIS는 `installer prepare|finish|recover|remove`로 앱 소유 서비스의 활성·불확실 lease를 검사하고, 기존 실행 파일 백업 → 안전 종료 → 새 파일 hash 확인 → 서비스 재시작을 수행한다. 새 설치를 자동으로 서비스 등록하지 않으며, 실패하면 복구 기록을 보존하고 검증된 원본으로 복구한다. `remove`는 서비스를 끄기 전에 `aam deactivate`와 같은 순서로 omp bridge·broker·observer를 푼다. 앱이 만든 HKCU Run `Ojak`(로그인 자동 실행)만 지운다.
- Windows 업데이트·복구는 종료 permit에 대응하는 `service.cancelUninstall` RPC로 DB의 신규 배정 차단까지 해제한 뒤 설치 기록을 정리한다. 명시적인 `service install`과 앱의 서비스 재시작도 DB 배정을 재개하며, 앱의 재시작은 `service_stop`의 안전 검사를 거친다.

## 호스트 연결 (`src/hosts.rs`)
`host_connections`가 Orca·VS Code·Cursor(모든 OS)와 Superset·cmux·Conductor(macOS만)의 shim 연결 상태와 설정 안내를 만든다. 데스크톱 연결 화면이 사용한다.
- 안내 문장은 보내지 않는다. `notes: [{ key, params }]`만 보내고, 화면이 `i18n.ts`의 `hosts.note.<key>`로 표시 언어 문장을 만든다. 첫 note가 상태 요약이고 나머지는 [자세히]로 접는다. key를 추가하면 세 언어 사전에 함께 넣는다(없는 key는 화면에서 빠진다).
- 앱 설치 위치·설정 폴더는 OS별이다. macOS `/Applications`·`~/Library/Application Support`, Windows `%LOCALAPPDATA%\Programs`·`%ProgramFiles%`·`%APPDATA%`. 복사용 명령은 macOS POSIX 따옴표, Windows 큰따옴표와 `.exe` 경로다.

## Screen Flow / Lifecycle
<!-- screen-flows-v3: 2026-09-27, type=cli-tool -->

| Stage | 상태 | 근거 |
|---|---|---|
| Command | ✅ | entry: crates/launcher/Cargo.toml:10-12 [[bin]] aam → src/main.rs. clap derive Parser `Cli`(main.rs:13-22) + Subcommand `Action` 최상위 12개(main.rs:24-78: status,  |
| Args | ✅ | clap 인자: run <tool> positional + --model(default NATIVE_DEFAULT_MODEL, main.rs:85) --cwd --account --resume-session, `--` 뒤 native_args(last=true, main.rs:93).  |
| Output | ⚡ | stdout/stderr 분리는 올바름. 데이터는 stdout으로 print_json pretty(main.rs:204-213) 또는 print_snapshot 사람용 텍스트(main.rs:214-261)로 나가고, 진단은 stderr에 `aam:` 접두사(main.rs:509, lib |
| Exit-Code | ⚡ | 성공과 --help/--version은 0(main.rs:499-502, 정상 반환). 모든 ApiError는 stderr `aam: msg (CODE)` 후 exit 1(main.rs:508-511). clap 사용법 오류도 INVALID_ARGUMENT로 바꿔 exit 1이 됨(ma |

```
aam                                    # clap derive (main.rs:13-78) · --help/--version → exit 0
├── status [--json]                    # 계정·할당·세션 스냅샷 (text | JSON)
├── doctor                             # 서비스 상태 + 도구 설치/격리 (text)
├── refresh                            # RPC quota.refresh (JSON)
├── explain --tool <T> [--model M] [--cwd P] [--account ID]
│                                      # RPC route.explain, 예약 없음 (JSON)
├── run <claude|codex> [--model M] [--cwd P] [--account ID] [--resume-session ID] [-- <native args>]
│                                      # 관리 실행(lease), passthrough 없음, exit = native 코드 / 128+sig
├── account
│   ├── add --tool <T> --label <L> --profile <DIR>          # account.register (JSON)
│   ├── login --tool <T> (--label <L> (--settings-digest D | --fresh-settings) | --account <ID>)
│   │                                  # 새 프로필 로그인, 또는 기존 프로필 다시 로그인 → 성공 시 등록
│   └── settings-preview --tool <T>    # (JSON)
├── service {install | uninstall | stop | restart | status} # LaunchAgent ai.aam.service (text). restart = 버전이 다른 서비스를 lease 검사 후 안전 재시작
├── integration {install | uninstall}  # Unix symlink / Windows .cmd shim: aam·claude·codex (text)
├── omp-broker {status [--json] | connect | disconnect}   # connect/disconnect → JSON
├── omp-bridge {status [--json] | connect | disconnect}   # connect는 omp-broker 연결 선행 필요
├── omp-observer {status | install | uninstall}           # 읽기 전용 관측 확장 (text)
├── takeover
│   ├── list                           # 인계 목록 + 후보 (JSON)
│   ├── adopt <tool> <session>         # takeover.adopt (JSON)
│   └── release <tool> <session>       # takeover.release (JSON)
└── shell {install | uninstall}        # zsh 앱 전용 PATH 블록 (text)

shim / 숨은 argv 모드 (main.rs:475-496, clap 이전 분기)
├── aam --claude-process-wrapper <claude-bin> [args…]
│                                      # CLAUDE_CODE_PROCESS_WRAPPER self-exec → lease.validate-child(1.2s) → exec
├── aam --claude-native-exec <program> [args…]
│                                      # cmux(CMUX_CLAUDE_PID) root spawn → 같은 검증 → exec
└── argv0 = claude | codex             # AAM_HOME/bin/<tool> → aam symlink
    ├── inspect_cli → 원본 CLI 바로 실행 (서비스 불필요)
    │   ├── --help | --version | -h (claude: -v, codex: -V)
    │   ├── claude auth status [--json]
    │   ├── self-update: claude install|update|upgrade · codex update
    │   └── 관리 명령: claude mcp|plugin|plugins|doctor|help · codex mcp|plugin|doctor|completion|features|help
    │       (mcp·plugin·features는 기본 설정 폴더 + stderr 한 줄. 인증 명령은 제외)
    └── run_cli → route.resolve
        ├── Managed   → lease.acquire → preflight verify → lease.starting → supervise → heartbeat 10s → lease.release
        ├── Unmanaged → exec 원본 (directory/repository 명시 승인만)
        └── management_unavailable(CODE) → stderr 경고 + exec 원본 (passthrough)
```

### 이슈
- [medium] Exit-Code: aam 자체 오류는 종류에 상관없이 exit 1(main.rs:510)임. clap 사용법 오류도 1(main.rs:504)이라 UNIX 관례의 2(usage)와 sysexits(예: DAEMON_UNAVAILABLE→69 EX_UNAVAILABLE)를 따르지 않음. 스크립트에서 사용법 오류, 데몬 부재, 정책 차단을 구분하려면 stderr 문자열을 파싱해야 함
- [medium] Exit-Code: run·shim 모드에서 aam이 spawn 전에 거부한 경우(IDENTITY_MISMATCH, AUTH_OVERRIDE_CONFLICT 등 → exit 1)와 native가 1로 끝난 경우의 exit code가 같음. 호스트(Orca/cmux 등)가 wrapper 거부와 native 실패를 구분할 수 없음(env/docker 관례는 125~127 예약)
- [low] Output: --json 지원이 status/omp-broker status/omp-bridge status에만 있음(main.rs:28,154,161). doctor/service status/integration/shell/omp-observer는 텍스트만, refresh/explain/takeover 등은 JSON만 출력해 형식이 명령마다 다름. --quiet/--verbose 없음
- [low] Output: hosts.rs:220 host_connections(호스트별 shim 연결 상태·설정 안내)는 데스크톱 Tauri 명령(apps/desktop/src-tauri/src/main.rs:419-424)에서만 호출되고 CLI 서브커맨드가 없음. 그런데 integration install 완료 메시지(install.rs:428)는 이 안내를 앱에서 확인하라고 함. CLI 사용자는 볼 수 없음
- [low] Args: ServiceAction/IntegrationAction/OmpBrokerAction/OmpBridgeAction/ObserverAction/ShellAction variant에 about/help 문구가 없어(main.rs:140-194) `aam service --help` 등에 액션 설명이 나오지 않음
- [low] Args: --tool 값과 run <tool>이 clap value_parser 없이 자유 문자열임. run은 intent_checked(lib.rs:87-93)에서 거부하지만 explain·account·takeover는 데몬/어댑터 호출 뒤에야 오류가 남 [INFERENCE: 서버 쪽 검증 여부는 확인 안 함]. help에 허용 값이 나오지 않음
- [low] Command: clap 명령 트리와 main의 argv 분기를 검증하는 테스트가 없음(테스트는 arguments.rs:448-575, lib.rs:858-1290, install.rs:760, descendants.rs:188-240, omp_*에만 있음). Cli::command().debug_assert()류 검증도 없음. docs/domains/launcher.md 명령 목록에는 run의 --mode

### 다음 할 일
- [ ] Exit-Code: INVALID_ARGUMENT는 exit 2로 바꾸고(메시지 비노출 정책은 유지), DAEMON_UNAVAILABLE/SERVICE_UNHEALTHY는 69, UNSUPPORTED_PLATFORM은 70 등으로 ApiError.code를 sysexits 값에 매핑
- [ ] Exit-Code: run·shim 모드에서 spawn 전 aam 거부는 별도 코드(예: 125)로 예약해 native 종료 코드와 구분하고, 표를 docs/domains/launcher.md에 기록
- [ ] Output: doctor/service status/integration/shell/omp-observer에 --json 추가(또는 전역 --json)해 모든 조회 명령이 text와 JSON을 모두 지원하게 함
- [ ] Output: `aam integration status`(또는 `aam hosts`) 서브커맨드로 hosts::host_connections를 CLI에 노출하고 install.rs:428 안내 문구를 CLI 기준으로 수정
- [ ] Args: 하위 액션 variant에 #[command(about=...)] 추가, --tool/<tool>에 value_parser = [claude, codex] 적용
- [ ] Command: Cli::command().debug_assert() 테스트를 추가하고 docs/domains/launcher.md 명령 목록에 run 옵션 반영
