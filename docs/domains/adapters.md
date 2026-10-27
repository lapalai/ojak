# adapters (`crates/adapters`)

## 개요
공식 CLI(Claude Code, Codex, omp)를 다루는 라이브러리. 바이너리 탐지, 계정 identity·quota 조회, 등록·로그인·실행 계획, 설정 가져오기 미리보기, 인증 우회 안전 검사, takeover용 세션 소유자 찾기, omp 세션 읽기 전용 관측.

## 진입점 (`src/lib.rs`)
`scan`(`:742`), `verify`(`:992`), `build_launch_plan`(`:1035`), `register`(`:1110`), `enrollment_plan`(`:1241`), `inspect_cli`(`:632`). 그 밖에 `observed::discover_sessions`(`observed.rs:141`), `takeover::session_owner`(`takeover.rs:162`), `settings::preview_settings`(`settings.rs:239`). 이 밖의 공개 함수(`native_access_staged`, `omp_inventory`, `same_login`, `relogin_plan`, `copy_conversation`, `share_extensions` 등)와 위치는 아래 트리에 있다.

## 바이너리 경로 (`src/process.rs`)
- `discover()`: `integration.json`의 `nativeBinaries`는 `version == 2`일 때만 믿고, 아니면 PATH를 찾는다. 반환값은 **진입 경로**(예: `~/.local/bin/claude` symlink).
- `executable()`: 쓸 때마다 canonicalize한다. 그래서 `claude install` 같은 공식 자체 업데이트를 재연결 없이 따라간다.
- 재귀 방지: 이름이 `aam`/`aam-*`, 경로에 `shims`, `AAM_`·`aam run`을 담은 `#!` 스크립트는 거부 (`process.rs:61-89`).
- Windows의 `bin/<tool>.cmd`는 `"<aam.exe>" --shim <tool> %*` 한 줄 배치이고, 배치 본문의 `aam.exe" --shim ` 흔적으로 거부한다. 이전 버전의 `bin/<tool>.exe`(launcher 복사본)도 다른 AAM_HOME이라도 상위 `integration.json`의 앱 owner와 `shims` 목록으로 확인되면 `SHIM_RECURSION`으로 거부한다. 명령 이름·bin 폴더만으로 원본을 거부하지 않으며, manifest는 링크를 따라가지 않고 64 KiB로 제한해 읽는다.
- 계정의 `binary_path`도 진입 경로를 저장한다. `scan`의 `refresh_bound`가 현재 탐지 경로로 갱신한다.
- `version()`: 성공한 `--version` 응답만 진입 경로·canonical 경로·파일 메타데이터 기준으로 캐시한다. 원본 교체·symlink 대상 변경 시 다시 확인하며, 실패는 캐시하지 않는다. 확인 중 파일이 바뀌면 그 결과도 캐시에 넣지 않는다.
- `native_access(account, refresh, deadline)`는 전후 identity 확인·Claude 키체인/`/usage`·Codex app-server에 같은 `Instant` deadline을 전달한다. `Probe::spawn_until`은 이미 만료된 작업을 시작하지 않고, 후속 RPC도 만료 후에는 보내거나 캐시된 성공으로 처리하지 않는다. 등록 프로필 경로는 `bound_paths`로 검증하고 원본 CLI 진입 경로는 사용할 때 해석한다. 이 deadline은 native broker 인증 작업용이며, 일반 계정·한도 scan의 개별 조회 제한과는 별개다.
- Windows 외부 OMP 관측은 `observed_windows.rs`에서 정확한 실행 파일·동일 사용자 SID·프로세스 생성 시각·열린 쓰기 디스크 핸들을 확인한다. argv/env/PEB의 대화 내용을 읽거나 경로 선언만으로 소유 세션을 추정하지 않는다. 메타데이터 읽기 전후 identity·파일 객체를 다시 확인한다.

## 계정·사용량 조회 (`scan`)
- 공식 프로필 검사는 최대 4개씩 병렬로 실행하며, 각 프로필에서 identity를 먼저 확인·발행하고 한도를 조회한다. 전체 identity 검사 완료를 기다린 뒤 한도를 조회하는 단계 장벽은 없다. omp 사용량·broker 로그인 목록·CLI 버전 조회도 별도 작업으로 실행하고 도착한 결과부터 `publish` 콜백으로 반영한다.
- 등록된 경로와 기본 `~/.claude`·`~/.codex` 외에, 강제 새로고침 또는 등록 계정이 없는 최초 조회에서 `profiles.rs`가 `AAM_HOME/profiles` 바로 아래 Ojak이 만든 `claude-<UUIDv4>`·`codex-<UUIDv4>` 디렉터리를 찾는다. 링크·다른 이름·파일은 제외하고 공식 CLI가 확인한 로그인만 복구한다. 같은 identity로 이미 등록된 프로필과 로그인 전 빈 폴더는 새 계정으로 만들지 않는다.
- identity가 없었던 기본 프로필은 공식 CLI가 로그인을 확인하면 기존 계정 ID·배정 설정을 유지한 채 정보를 채운다. 이미 고정된 identity가 바뀌면 계속 배정을 차단한다.
- 이전에 `authenticated`·`preflight-verified`·`canLaunch`와 identity가 확인된 계정의 `PROBE_TIMEOUT/PROBE_IO/PROBE_START_FAILED/PROBE_CLOSED`는 로그인 폐기로 간주하지 않는다. binding을 보존하고 기존 한도를 stale로 표시하며, 실패 코드와 안내를 남긴다. 초기 미검증 계정을 승격하거나 명시 로그아웃·identity 변경·인증 덮어쓰기·스키마 오류를 무시하지 않는다.
- 신원 조회의 성공 여부는 계정의 `reason`과 별도로 전달한다. 조회 실패 뒤에는 별도 한도 조회를 중복 실행하지 않지만, workspace ID 없는 정상 Codex 계정의 안내 문구는 한도 조회를 막지 않는다. 실행·토큰 제공 경로의 실제 identity 재검증은 생략하지 않는다.
- 연결된 omp broker의 `/v1/snapshot`에서 `omp_inventory`가 공급자·이메일·계정/조직/프로젝트 정보만 추출한다. 사용량 응답이 없어도 로그인 목록은 남고, 토큰·API key·Ojak 브릿지 로그인은 계정 결과로 옮기지 않는다. inventory 콜백은 `Result<Option<Vec<Account>>, ApiError>`로 미연결(`None`)과 실패를 구분한다. 실패는 `OMP_INVENTORY_UNAVAILABLE` 안내로 남기고 기존 사용량 경로·관측은 보존한다.
- 로그인 목록과 사용량 보고가 같은 identity이면 같은 계정 행을 갱신한다. 현재 사용량이 누락됐다는 이유로 확인한 identity를 이메일-only 행으로 바꾸지 않으며, 다른 subject·workspace의 `accountsWithoutUsage`는 별도로 유지한다. 더 오래된 omp 보고가 새 공식 한도를 덮지 않는다.
- 과거 identity 없는 omp 관측은 이메일만으로 병합·삭제하지 않는다. 로그인 목록만 다시 확인한 것은 인증 복구 근거가 아니므로 기존 `auth-required`·`error`와 이유를 보존한다. 정상 사용량·공식 identity 확인이 있어야 상태를 복구한다.


## shim 우회 (`inspect_cli`)
서비스 없이 원본 CLI를 바로 실행하는 경우. 인증 명령은 넘기지 않는다.
- 인수가 `--help`/`--version`/`-h` 하나 (Claude는 `-v`, Codex는 `-V`도)
- Claude `auth status [--json]`만. `auth login`·`logout`·`setup-token`은 넘기지 않는다.
- 첫 인수가 Claude `install`/`update`/`upgrade`, Codex `update`
- 모델 세션을 시작하지 않고 인증을 바꾸지 않는 관리 명령: Claude `mcp`·`plugin`·`plugins`·`doctor`·`help`, Codex `mcp`·`plugin`·`doctor`·`completion`·`features`·`help`
  - `mcp`·`plugin`·`plugins`·`features`는 Ojak 계정을 고르지 않는다. 상속된 `CLAUDE_CONFIG_DIR`/`CODEX_HOME`은 이 실행에서만 지우고, 공식 CLI 기본 설정 폴더에서 실행한다. stderr에 한 줄로 알리며 환경 변수 값은 출력하지 않는다. Codex의 MCP·플러그인·기능은 다음 관리 실행이 기본 폴더에서 계정 프로필로 가져온다. Claude 사용자 MCP는 복사하지 않고, Unix에서는 플러그인 폴더만 계정 세션과 연결한다.
  - `config`·`settings`·`profile`은 키마다 인증 여부를 가를 수 없어 넘기지 않는다. `login`·`resume`처럼 세션을 시작하는 명령도 넘기지 않는다.

## identity (`src/native.rs`)
- Claude: `authMethod=claude.ai`, `apiProvider=firstParty` 필수. `identity_key = anthropic|subject:<id>|workspace:<orgId>` (`native.rs:54-61`).
- Codex: `account.type=chatgpt` 필수. `app-server`가 보고한 codexHome이 프로필과 같아야 한다.
- `inspect_identity`는 Codex identity 확인에서 한도를 읽지 않는다. `codex_usage`는 한도 조회 전·후에 고정된 identity를 확인하고, 빈 한도 응답이나 identity 변경을 성공으로 처리하지 않는다. 이전 버킷의 관측 시각은 실패로 갱신하지 않으며, 확인하지 못한 크레딧은 제거한다.
- native broker의 `inspect_identity_until`도 같은 신원 검사만 수행한다. 인증 갱신 전후 확인 때문에 Codex 한도 API를 추가로 호출하지 않는다.
- identity를 얻어야만 `preflight-verified` + `can_launch=true` (`native.rs:63-72`).
- omp 계정은 관측 전용(`OMP_GATE`, `native.rs:10-12`). Ojak이 omp를 직접 실행하지 않는다.
- `omp usage --json.disabledCredentials`의 `invalid_grant`·`invalid_token`은 기존 관측 계정과 공급자·subject/workspace identity가 정확히 일치할 때만 `auth-required`로 반영한다. 원문 오류는 저장하지 않는다. 같은 identity의 현재 보고서와 정상 native 로그인은 무효화하지 않으며, 단순 조회 누락·타임아웃·429·503은 인증 만료 근거가 아니다.
- 크레딧(`native.rs` → `quota.rs::codex_credits`): Codex는 `account/rateLimits/read` 응답의 `credits{hasCredits,unlimited,balance}`·`ordinaryUsageAllowed`를 `Account.credits`에 읽는다. `codex` 한도 묶음 → 단일 `rateLimits` → 크레딧을 알려 준 다른 묶음 순. `hasCredits`가 없거나 응답에 `credits`가 없으면 `None`(있다고 가정하지 않음), `available = (hasCredits && 잔액이 0 이하가 아님) || unlimited`. `balance`는 숫자로 읽힐 때만 보관(ChatGPT 크레딧 단위, 금액 아님). 조회에 실패하면 이전 값을 이어 쓰지 않는다. 실제 크레딧 사용 상태는 관측한 적 없어 공식 스키마의 필드 형태로만 시험했다(미검증).
- Claude 추가 사용량(`quota.rs::omp_extra_usage`): omp 사용량 보고의 `anthropic:extra` 항목(`amount.unit="usd"`, `used`, 선택 `limit`, `status`)을 `Account.extra_usage{enabled,used_usd,limit_usd,observed_at}`로 읽어 Claude 계정에 붙인다. omp는 켜져 있고 USD일 때만 이 항목을 내보내므로 항목이 없으면 "켜졌다고 보지 않음"이다. USD 항목은 `omp_buckets`가 건너뛰어 percent 버킷이 되지 않는다(소진 판정에 섞이지 않음). 음수·잘못된 금액·`limit<=0`은 무시, 항목이 `exhausted`면 쓸 수 없는 것으로 본다. 켜진 상태는 이 Mac에서 관측한 적 없고 omp `claude.ts` 소스의 필드 형태 기준이다(미관측).
- Claude 구독 한도: 신선한 omp 관측이 없으면 등록된 공식 프로필에서 `/usage`를 safe mode·도구/MCP 비활성화·세션 저장 없이 조회한다. 구독 인증(`apiKeySource=none`), 성공한 로컬 명령(`num_turns=0`), 구조화된 `limits` 배열을 모두 확인하고 조회 뒤 identity를 다시 검증한다. 캐시된 본문만 있거나 조회·identity 검증이 실패하면 이전 관측 시각을 갱신하지 않고 `NATIVE_USAGE_UNAVAILABLE`로 알린다. omp에 같은 계정을 다시 로그인할 필요가 없으며, 토큰 갱신은 공식 CLI가 담당한다. 기존 omp 버킷 ID를 이어 사용한다.
- Claude **스캔 전용** 신원 확인(`native::inspect_identity_for_scan`)과 `/usage`는 개별 호출에 50초 절대 제한을 적용한다. 일반 20초 조회·실행 전 검증·broker의 공유 deadline·Codex 경로는 바꾸지 않는다. 실제 Mac 조회에서 정상 완료가 22.4초 걸린 사례를 반영한 제한이며, 그 지연의 OS·네트워크 하위 원인이 확정된 것은 아니다.
- 공식 한도가 정상일 때는 10분 주기로 조회하지만, `stale`·`unknown`이면 다음 정기 스캔에서 다시 조회한다. 별도 자동 재시도 루프는 없고 기존 서비스 60초 스케줄과 중복 스캔 방지를 따른다. 실패한 관측 시각은 갱신하지 않는다.
- 사용량 실패는 `reason`에 고정 안내와 `preflight`/`usage`/`parse`/`identity-recheck` 단계·오류 코드·경과 시간을 남긴다. CLI 원문 오류는 저장하지 않는다. 신원 확인 성공만으로 사용량 오류를 지우지 않으며, 공식 사용량 조회 성공이나 신선한 omp 한도로 복구됐을 때 해제한다. 신원 확인 실패도 기존 안내·오류 코드에 경과 시간을 붙인다.
- 실기기 대조: 동일 Claude 계정·workspace의 웹 사용량과 공식 CLI에서 5시간/주간/Fable 사용률이 각각 90/24/1%로 일치함을 확인했다. 웹은 사용률, Ojak은 잔여율(`100 - 사용률`)을 표시한다. 웹 화면도 캐시된 값과 마지막 갱신 시각을 표시하므로 같은 관측 시점끼리 비교하며, 리셋 시각은 브라우저와 OS 시간대 차이를 UTC로 정규화한다. 이 대조는 웹 로그인된 한 계정에 대한 검증이다.

## 안전 검사 (`src/safety.rs`)
- 환경변수·설정 파일의 API key·base URL·token·auth helper를 찾으면 `AUTH_OVERRIDE_CONFLICT`. 메시지에는 환경변수 이름, 또는 설정 파일 경로와 키만 적고 값은 적지 않는다. 이 터미널에서 변수를 해제하거나 해당 파일에서 키를 제거하라고 안내한다. 자동으로 고치지 않는다.
- `NODE_EXTRA_CA_CERTS`·`SSL_CERT_FILE`·`SSL_CERT_DIR`와 프록시(`HTTPS_PROXY`·`HTTP_PROXY`·`ALL_PROXY`)는 구독 계정을 고르지 않으므로 막지 않는다. `NODE_OPTIONS`·`LD_PRELOAD`·`DYLD_INSERT_LIBRARIES`는 코드를 넣어 인증을 바꿀 수 있어 막는다. `ANTHROPIC_BASE_URL`·`OPENAI_BASE_URL`도 endpoint를 바꾸므로 막는다.
- 상속된 `CLAUDE_CONFIG_DIR`/`CODEX_HOME`이 선택한 프로필과 다르면 `HOST_PROFILE_CONFLICT`.
- `NODE_OPTIONS`는 cmux의 정확한 값 하나만 허용한다. 그 예외가 아니면 변수 이름을 알리고 멈춘다.

## Screen Flow / Lifecycle
<!-- screen-flows-v3: 2026-09-27, type=library -->

| Stage | 상태 | 근거 |
|---|---|---|
| Module | ✅ | `crates/adapters/Cargo.toml:2` 패키지 `aam-adapters`, 버전은 `version.workspace`(워크스페이스 버전 0.17.0, `Cargo.toml:7`). 암묵 lib 타깃 `crates/adapters/src/lib.rs`이며 `lib.rs:1-19`가 모듈을 선언하고 공개 항목을 재노출한다. |
| Function | ⚡ | 재노출된 공개 함수 26개는 모두 `Result<_, ApiError>` 등 타입 있는 시그니처이고, `///` rustdoc은 18개에만 있다. 없는 8개: `verify`(`lib.rs:992`), `build_launch_plan`(`:1035`), `register`(`:1110`), `enrollment_plan`(`:1241`), `native_access`(`access.rs:32`), `preview_settings`(`settings.rs:239`), `prepare_settings_import`(`:332`), `apply_settings_import`(`:336`). |
| Example | 🚧 | README·`crates/adapters/tests/`·doctest 없음, `crates/adapters/examples/`에 추적되는 파일 없음. 사용 예는 크레이트 안 `#[test]` 87개(`src/*.rs`의 `mod tests` 등)뿐이다. |

```text
aam-adapters v0.17.0 (crates/adapters/src/lib.rs)
├── [crate root] lib.rs
│   ├── scan(paths: &Paths, existing: &[Account], force: bool, inventory: impl FnOnce() -> Result<Option<Vec<Account>>, ApiError> + Send, publish: impl FnMut(&Account) -> Result<(), ApiError> + Send) -> Result<ScanResult, ApiError>  :742  ← service/lib.rs:1606
│   ├── verify(account: &Account) -> Result<IdentityEvidence, ApiError>  :992  ← launcher/lib.rs:871
│   ├── build_launch_plan(account: &Account, intent: &LaunchIntent) -> Result<LaunchPlan, ApiError>  :1035  ← launcher/lib.rs:818
│   ├── register(paths: &Paths, tool: &str, label: &str, profile_path: &str) -> Result<Account, ApiError>  :1110  ← service/lib.rs:840, launcher/provider_login.rs:407
│   ├── enrollment_plan(paths: &Paths, tool: &str, label: &str) -> Result<EnrollmentPlan, ApiError>  :1241  ← launcher/lib.rs:1275, launcher/provider_login.rs:365
│   ├── relogin_plan(paths: &Paths, tool: &str, label: &str, profile: &Path) -> Result<EnrollmentPlan, ApiError>  :1299  ← launcher/lib.rs:1332, launcher/provider_login.rs:361
│   ├── 📝 inspect_cli(paths: &Paths, tool: &str, args: &[OsString]) -> Option<Result<ExitStatus, ApiError>>  :632  ← launcher/main.rs:825
│   ├── 📝 supports_shared_profile_concurrency(account: &Account) -> bool  :1029  ← service/lib.rs:820
│   ├── 📝 check_native_auth_context(tool: &str, profile: &Path, cwd: &Path) -> Result<(), ApiError>  :108  ← launcher/lib.rs:640
│   ├── 📝 check_subscription_auth_env(tool: &str) -> Result<(), ApiError>  :118  ← service/reset_credits.rs:99
│   ├── 📝 omp_inventory(snapshot: &Value, existing: &[Account]) -> Result<Vec<Account>, ApiError>  :694  ← service/bridge.rs:1806
│   ├── 📝 same_login(native: &Account, observed: &Account) -> bool  :1146  ← launcher/provider_login.rs:416
│   ├── 📝 omp_credential_identity(provider, email, account_id, org_id, project_id) -> Option<String>  :1176  ← launcher/provider_login.rs:616, service/bridge.rs:1835
│   ├── 📝 copy_conversation(source: &Account, target: &Account, native_id: &str) -> Result<(), ApiError>  :2093  ← launcher/lib.rs:805
│   ├── 📝 copy_claude_transcript(source, target, native_id) -> Result<(), ApiError>  :1996  ← copy_conversation (lib.rs:2095)
│   ├── 📝 codex_session_from(account: &Account, cwd: &str, written: &[PathBuf]) -> Option<String>  :2059  ← launcher/lib.rs:997
│   ├── 📝 share_extensions(account: &Account) -> Result<(), ApiError>  :2082  ← launcher/lib.rs:808
│   ├── struct ScanResult { accounts, tools, notices, merged_observation_ids }  :46
│   └── struct EnrollmentPlan { program, args, env, profile_path, tool, label }  #[derive(Debug, Clone)]  :62
├── access.rs  (pub use lib.rs:2)
│   ├── native_access(account: &Account, refresh: bool, deadline: Instant) -> Result<NativeAccess, ApiError>  :32  (crate 밖 호출 없음)
│   ├── 📝 native_access_staged(account, refresh, deadline, stage: &mut dyn FnMut(&'static str)) -> Result<NativeAccess, ApiError>  :37  ← service/bridge_native.rs:287,385
│   ├── 📝 const STAGES: [&str; 8] (lib.rs 재노출 이름 NATIVE_ACCESS_STAGES)  :21
│   └── struct NativeAccess { access, expires, account_id }  (Debug·Serialize 없음)  :11
├── observed.rs  (pub use lib.rs:7)
│   ├── 📝 discover_sessions(paths: &Paths, accounts: &[Account]) -> ObservedScan  :141  ← service/lib.rs:359
│   ├── 📝 open_jsonl_writers(pid: u32) -> Vec<PathBuf>  :836  ← launcher/lib.rs:279, launcher/auto_continue.rs:656
│   └── struct ObservedScan { sessions: Vec<ObservedSession>, notices: Vec<Notice> }  #[derive(Default)]  :15
├── takeover.rs  (pub use lib.rs:15)
│   ├── 📝 session_owner(accounts: &[Account], tool: &str, native: &str) -> Result<SessionOwner, ApiError>  :162  ← service/lib.rs:745, service/managed_sessions.rs:80
│   ├── 📝 configured_model(tool: &str, cwd: &Path, profile: Option<&str>) -> Option<(String, String)>  :125  ← service/scheduler.rs:163
│   ├── 📝 struct SessionOwner { account_id, native_session_id, cwd, evidence }  :15
│   └── ⚠ claude_session_owner(accounts, native) — pub but not re-exported (crate-internal)  :58
├── settings.rs  (pub use lib.rs:16-19)
│   ├── preview_settings(tool: &str) -> Result<SettingsPreview, ApiError>  :239  ← launcher/main.rs:513
│   ├── prepare_settings_import(tool: &str, digest: &str) -> Result<SettingsImport, ApiError>  :332  ← launcher/lib.rs:1273, launcher/provider_login.rs:372
│   ├── apply_settings_import(profile: &Path, import: SettingsImport) -> Result<(), ApiError>  :336  ← launcher/lib.rs:1277, launcher/provider_login.rs:376
│   ├── struct SettingsPreview { tool, source, digest, can_import, changes, omitted, warnings }  Serialize(camelCase)  :13
│   ├── struct SettingsImport { /* private: filename, content */ }  :29
│   └── ⚠ struct SettingChange { key, value } — reachable via SettingsPreview.changes, not re-exported  :24
└── (crate-private — no public surface)
    ├── native.rs  inspect · inspect_at · parse_claude · parse_codex · claude_identity · profile_env · blank_account · OMP_GATE
    ├── process.rs  discover · executable · search_dirs · base_env · run_json · run_json_slow(omp usage, 50초) · version · Probe{spawn, send, request, output}
    ├── quota.rs  stable_id · text · credential_pin · provider_id · report_identity · omp_buckets · codex_buckets
    ├── safety.rs  profile_var · check_env · check_inherited_profile · check_settings · canonical_profile
    └── observed_metadata.rs (pub(super))  read_sessions · attribute · WriterFile · SessionMetadata · MAX_FILES
```

범례: 📝 = `///` rustdoc 있음 (fn 18/26, type 1/8) · ⚠ = 가시성 이상 · `← caller file:line`

계정 lifecycle 호출 순서: `enrollment_plan` → (사용자 터미널 로그인) → `register` → `scan` (refresh_bound) → `verify` → `build_launch_plan`

### 이슈
- [medium] Function: 재노출된 공개 함수 26개 중 8개는 `///` 문서 주석을 갖지 않는다. 계정 lifecycle 진입점 `verify`(lib.rs:992), `build_launch_plan`(lib.rs:1035), `register`(lib.rs:1110), `enrollment_plan`(lib.rs:1241)와 `native_access`(access.rs:32), settings 3종(settings.rs:239,332,336)이다.
- [low] Function: `SettingChange`(settings.rs:24)는 공개 필드 `SettingsPreview.changes`(settings.rs:18)로 노출되지만 lib.rs:16-19에서 재노출하지 않아 다른 크레이트는 타입 이름을 직접 쓰지 못하는 상태다.
- [low] Function: `takeover::claude_session_owner`(takeover.rs:58)는 비공개 모듈 안의 `pub`이고 재노출되지 않는다. 호출처는 `session_owner`(takeover.rs:168) 한 곳이라 공개 API처럼 보이는 가시성이 실제와 다르다.
- [medium] Example: `build_launch_plan`의 검사(lib.rs:1036-1080: ACCOUNT_MISMATCH, ACCOUNT_DISABLED, claude·codex 외 RESUME_UNVERIFIED, SESSION_MAPPING_INVALID, MODEL_REQUIRED 문자·160바이트 제한)와 `validate_label`(lib.rs:1231-1239)을 직접 검증하는 테스트를 찾지 못했다.
- [low] Example: `configured_model` 우선순위(settings.local.json > settings.json > 프로필 settings.json, takeover.rs:130-137)는 시험하지 않는다. takeover.rs의 `#[test]` 개수는 0이다.
- [low] Example: README·tests 디렉터리·doctest는 확인되지 않는다. examples 폴더는 비어 있다. 소비자용 안내는 이 문서의 서술로 대신하고 코드 예시는 두지 않았다.

### 다음 할 일
- [ ] Function: `verify`/`build_launch_plan`/`register`/`enrollment_plan`/`native_access`/`preview_settings`/`prepare_settings_import`/`apply_settings_import`에 `///`로 ApiError 코드와 부수 효과를 적는다 (`enrollment_plan`은 프로필 폴더를 `restrict_dir`로 제한 lib.rs:1253,1268, `apply_settings_import`는 `create_new` + Unix 0600 settings.rs:339-342).
- [ ] Function: lib.rs에 크레이트 수준 `//!`로 lifecycle 순서 `enrollment_plan` → 로그인 → `register` → `scan` → `verify` → `build_launch_plan`을 적는다.
- [ ] Function: `SettingChange`를 lib.rs:16-19에서 재노출하거나 `pub(crate)`로 낮추고, `claude_session_owner`(takeover.rs:58)를 비공개 fn으로 내린다.
- [ ] Example: 단위 테스트 추가 대상은 `build_launch_plan` 검사 순서·코드, `validate_label` 경계(빈 값, 120 vs 121바이트, 제어 문자), 임시 `.claude` 폴더를 쓴 `configured_model` 우선순위다.
- [ ] Example (선택): `build_launch_plan`/`verify`에 Account + LaunchIntent → LaunchPlan 흐름을 보이는 rustdoc 예제나 `examples/launch_plan.rs`를 둔다.
