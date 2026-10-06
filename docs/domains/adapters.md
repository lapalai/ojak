# adapters (`crates/adapters`)

## 개요
공식 CLI(Claude Code, Codex, omp)를 다루는 라이브러리. 바이너리 탐지, 계정 identity·quota 조회, 등록·로그인·실행 계획, 설정 가져오기 미리보기, 인증 우회 안전 검사, takeover용 세션 소유자 찾기, omp 세션 읽기 전용 관측.

## 진입점 (`src/lib.rs`)
`scan`(`:393`), `verify`(`:501`), `build_launch_plan`(`:544`), `register`(`:614`), `enrollment_plan`(`:658`), `inspect_cli`(`:359`). 그 밖에 `observed::discover_sessions`(`observed.rs:138`), `takeover::session_owner`(`takeover.rs:166`), `settings::preview_settings`(`settings.rs:240`).

## 바이너리 경로 (`src/process.rs`)
- `discover()`: `integration.json`의 `nativeBinaries`는 `version == 2`일 때만 믿고, 아니면 PATH를 찾는다. 반환값은 **진입 경로**(예: `~/.local/bin/claude` symlink).
- `executable()`: 쓸 때마다 canonicalize한다. 그래서 `claude install` 같은 공식 자체 업데이트를 재연결 없이 따라간다.
- 재귀 방지: 이름이 `aam`/`aam-*`, 경로에 `shims`, `AAM_`·`aam run`을 담은 `#!` 스크립트는 거부 (`process.rs:61-89`).
- Windows의 `bin/<tool>.cmd`는 `"<aam.exe>" --shim <tool> %*` 한 줄 배치이고, 배치 본문의 `aam.exe" --shim ` 흔적으로 거부한다. 이전 버전의 `bin/<tool>.exe`(launcher 복사본)도 다른 AAM_HOME이라도 상위 `integration.json`의 앱 owner와 `shims` 목록으로 확인되면 `SHIM_RECURSION`으로 거부한다. 명령 이름·bin 폴더만으로 원본을 거부하지 않으며, manifest는 링크를 따라가지 않고 64 KiB로 제한해 읽는다.
- 계정의 `binary_path`도 진입 경로를 저장한다. `scan`의 `refresh_bound`가 현재 탐지 경로로 갱신한다.
- Windows 외부 OMP 관측은 `observed_windows.rs`에서 정확한 실행 파일·동일 사용자 SID·프로세스 생성 시각·열린 쓰기 디스크 핸들을 확인한다. argv/env/PEB의 대화 내용을 읽거나 경로 선언만으로 소유 세션을 추정하지 않는다. 메타데이터 읽기 전후 identity·파일 객체를 다시 확인한다.

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
- identity를 얻어야만 `preflight-verified` + `can_launch=true` (`native.rs:63-72`).
- omp 계정은 관측 전용(`OMP_GATE`, `native.rs:10-12`). Ojak이 omp를 직접 실행하지 않는다.

## 안전 검사 (`src/safety.rs`)
- 환경변수·설정 파일의 API key·base URL·token·auth helper를 찾으면 `AUTH_OVERRIDE_CONFLICT`. 메시지에는 환경변수 이름, 또는 설정 파일 경로와 키만 적고 값은 적지 않는다. 이 터미널에서 변수를 해제하거나 해당 파일에서 키를 제거하라고 안내한다. 자동으로 고치지 않는다.
- `NODE_EXTRA_CA_CERTS`·`SSL_CERT_FILE`·`SSL_CERT_DIR`와 프록시(`HTTPS_PROXY`·`HTTP_PROXY`·`ALL_PROXY`)는 구독 계정을 고르지 않으므로 막지 않는다. `NODE_OPTIONS`·`LD_PRELOAD`·`DYLD_INSERT_LIBRARIES`는 코드를 넣어 인증을 바꿀 수 있어 막는다. `ANTHROPIC_BASE_URL`·`OPENAI_BASE_URL`도 endpoint를 바꾸므로 막는다.
- 상속된 `CLAUDE_CONFIG_DIR`/`CODEX_HOME`이 선택한 프로필과 다르면 `HOST_PROFILE_CONFLICT`.
- `NODE_OPTIONS`는 cmux의 정확한 값 하나만 허용한다. 그 예외가 아니면 변수 이름을 알리고 멈춘다.

## Screen Flow / Lifecycle
<!-- screen-flows-v3: 2026-09-27, type=library -->

| Stage | 상태 | 근거 |
|---|---|---|
| Module | ✅ | crates/adapters/Cargo.toml:2 package aam-adapters (workspace version 0.1.0, Cargo.toml:7), implicit lib target crates/adapters/src/lib.rs. lib.rs:1-14 declares  |
| Function | ⚡ | 13 pub fns with typed Result<_, ApiError> signatures, but only 4 carry `///` rustdoc: supports_shared_profile_concurrency lib.rs:536-538, discover_sessions obse |
| Example | 🚧 | No README, no crates/adapters/examples/ or crates/adapters/tests/, no doctests. Usage patterns only via 28 in-crate unit tests (lib.rs:737,756,787; native.rs:23 |

```text
aam-adapters v0.1.0 (crates/adapters/src/lib.rs)
├── [crate root] lib.rs
│   ├── scan(paths: &Paths, existing: &[Account]) -> Result<ScanResult, ApiError>  :393  ← service/lib.rs:1363
│   ├── verify(account: &Account) -> Result<IdentityEvidence, ApiError>  :501  ← launcher/lib.rs:569
│   ├── build_launch_plan(account: &Account, intent: &LaunchIntent) -> Result<LaunchPlan, ApiError>  :544  ← launcher/lib.rs:511
│   ├── register(paths: &Paths, tool: &str, label: &str, profile_path: &str) -> Result<Account, ApiError>  :614  ← service/lib.rs:733
│   ├── enrollment_plan(paths: &Paths, tool: &str, label: &str) -> Result<EnrollmentPlan, ApiError>  :658  ← launcher/lib.rs:810
│   ├── inspect_cli(paths: &Paths, tool: &str, args: &[OsString]) -> Option<Result<ExitStatus, ApiError>>  :359  ← launcher/main.rs:491
│   ├── 📝 supports_shared_profile_concurrency(account: &Account) -> bool  :538  ← service/lib.rs:713
│   ├── struct ScanResult { accounts, tools, notices, merged_observation_ids }  :39
│   └── struct EnrollmentPlan { program, args, env, profile_path, tool, label }  #[derive(Debug, Clone)]  :47
├── observed.rs  (pub use lib.rs:4)
│   ├── 📝 discover_sessions(paths: &Paths, accounts: &[Account]) -> ObservedScan  :138  ← service/lib.rs:261
│   └── struct ObservedScan { sessions: Vec<ObservedSession>, notices: Vec<Notice> }  #[derive(Default)]  :12
├── takeover.rs  (pub use lib.rs:10)
│   ├── 📝 session_owner(accounts: &[Account], tool: &str, native: &str) -> Result<SessionOwner, ApiError>  :166  ← service/lib.rs:632, service/managed_sessions.rs:80
│   ├── 📝 configured_model(tool: &str, cwd: &Path, profile: Option<&str>) -> Option<(String, String)>  :129  ← service/scheduler.rs:128
│   ├── 📝 struct SessionOwner { account_id, native_session_id, cwd, evidence }  :16
│   └── ⚠ claude_session_owner(accounts, native) — pub but not re-exported (crate-internal)  :62
├── settings.rs  (pub use lib.rs:11-14)
│   ├── preview_settings(tool: &str) -> Result<SettingsPreview, ApiError>  :240  ← launcher/main.rs:360
│   ├── prepare_settings_import(tool: &str, digest: &str) -> Result<SettingsImport, ApiError>  :336  ← launcher/lib.rs:808
│   ├── apply_settings_import(profile: &Path, import: SettingsImport) -> Result<(), ApiError>  :340  ← launcher/lib.rs:812
│   ├── struct SettingsPreview { tool, source, digest, can_import, changes, omitted, warnings }  Serialize(camelCase)  :14
│   ├── struct SettingsImport { /* private: filename, content */ }  :30
│   └── ⚠ struct SettingChange { key, value } — reachable via SettingsPreview.changes, not re-exported  :25
└── (crate-private — no public surface)
    ├── native.rs  inspect · inspect_at · parse_claude · parse_codex · claude_identity · profile_env · blank_account · OMP_GATE
    ├── process.rs  discover · executable · search_dirs · base_env · run_json · run_json_slow(omp usage, 50초) · version · Probe{spawn, send, request, output}
    ├── quota.rs  stable_id · text · credential_pin · provider_id · report_identity · omp_buckets · codex_buckets
    ├── safety.rs  profile_var · check_env · check_inherited_profile · check_settings · canonical_profile
    └── observed_metadata.rs (pub(super))  read_sessions · attribute · WriterFile · SessionMetadata · MAX_FILES
```

범례: 📝 = `///` rustdoc 있음 (fn 4/13, type 1/6) · ⚠ = 가시성 이상 · `← caller file:line`

계정 lifecycle 호출 순서: `enrollment_plan` → (사용자 터미널 로그인) → `register` → `scan` (refresh_bound) → `verify` → `build_launch_plan`

### 이슈
- [medium] Function: 9/13 pub fns lack `///` rustdoc, including the core account lifecycle entry points scan (lib.rs:393), verify (lib.rs:501), build_launch_plan (lib.rs:544), register (lib.rs:614), enrollment_plan (lib.rs:658) and settings 
- [low] Function: inspect_cli (lib.rs:358-363) uses a `//` comment instead of `///`, so rustdoc is empty; the Option contract (None = not a bypass command, caller must route through the service) is undocumented.
- [low] Function: SettingChange (settings.rs:25) is exposed through the pub field SettingsPreview.changes (settings.rs:19) but not re-exported in lib.rs:11-14, so downstream crates cannot name the type.
- [low] Function: takeover::claude_session_owner (takeover.rs:62) is declared `pub` inside a private module and never re-exported; its only caller is session_owner (takeover.rs:172). The visibility suggests a public API that does not exis
- [medium] Example: build_launch_plan guard rules (lib.rs:545-588: ACCOUNT_MISMATCH, ACCOUNT_DISABLED, non-claude RESUME_UNVERIFIED, SESSION_MAPPING_INVALID, MODEL_REQUIRED charset/160-byte limit) and validate_label (lib.rs:649-653) have no
- [low] Example: configured_model precedence (settings.local.json > settings.json > profile settings.json, takeover.rs:134-140) is untested; takeover.rs has no in-crate tests, and session_owner is covered only indirectly by crates/servic
- [low] Example: No README, examples/ directory, or doctests. The only consumer-facing guide is the prose in docs/domains/adapters.md, which has no code snippets.

### 다음 할 일
- [ ] Function: add `///` docs to scan/verify/build_launch_plan/register/enrollment_plan/preview_settings/prepare_settings_import/apply_settings_import listing ApiError codes and side effects (enrollment_plan creates a 0700 profile dir lib.rs:681-699; apply_settings_import is create_new 0600 settings.rs:342-346); change the inspect_cli `//` comment (lib.rs:358) to `///`.
- [ ] Function: add a crate-level `//!` in lib.rs describing the lifecycle order enrollment_plan → login → register → scan → verify → build_launch_plan.
- [ ] Function: re-export SettingChange from lib.rs:11-14 (or make it pub(crate)) and downgrade claude_session_owner (takeover.rs:62) to a private fn.
- [ ] Example: add unit tests for build_launch_plan guard ordering/codes and validate_label boundaries (empty, 120 vs 121 bytes, control characters), plus configured_model precedence using temp .claude dirs.
- [ ] Example (optional): add a rustdoc example on build_launch_plan/verify, or an examples/launch_plan.rs, showing the Account + LaunchIntent → LaunchPlan flow.
