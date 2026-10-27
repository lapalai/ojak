# protocol (`crates/protocol`)

## 개요
공유 serde 타입, 상수, 프로세스 identity, 로컬 RPC 클라이언트/코덱(macOS Unix socket, Windows named pipe — `ipc.rs`, 권한 도우미 `secure.rs`, Windows API `winutil.rs`). `apps/desktop/src/types.ts`가 같은 타입을 TypeScript로 복제한다.

## 상수 (`src/lib.rs`)
| 이름 | 값 | 위치 |
|---|---|---|
| `PROTOCOL_VERSION` | 1 | `lib.rs:27` |
| `MAX_FRAME` | 1 MiB | `lib.rs:28` |
| `NATIVE_DEFAULT_MODEL` | `native-default` (모델 미지정, `--model` 주입 안 함) | `lib.rs:30` |
| `INTEGRATION_VERSION` | 2 (integration.json이 진입 경로를 저장) | `lib.rs:33` |
| `BRIDGE_PORT` | 4020 | `lib.rs:35` |

## 경로 (`Paths::discover`, `lib.rs:74`)
- `AAM_HOME` 기본값: macOS `~/Library/Application Support/AI Account Manager`, Windows `%APPDATA%\AI Account Manager`(`APPDATA`가 없으면 `~/AppData/Roaming/AI Account Manager`) (`lib.rs:82-98`)
- 하위: `state.sqlite3`, `profiles/`, `bin/`(shim), `bridge.json`, `bridge.token`, `integration.json`, `logs/`, Unix socket용 `run/`. 그 밖의 기록은 각 crate가 쓴다: `service-install.json`·`shell-integration.json`(launcher), `ui-settings.json`(desktop이 쓰고 launcher는 읽기만 한다), `bridge-native.token`(service `bridge.rs`), `omp-observations/`(adapters·launcher), Windows 전용 `omp-broker-service.json`·`installer-update.json`·`service-supervisor.{lock,gate,retire,child,process}`
- socket: macOS/Unix `AAM_HOME/run/<fnv(AAM_HOME) hex>.sock`(`Paths::prepare`가 `run/`을 `secure::restrict_dir`로 0700 제한, `lib.rs:101-121`), Windows named pipe `\\.\pipe\aam-<fnv hex>`

## RPC
- `call` (`lib.rs:702`): 4바이트 길이 prefix + JSON `RpcRequest` → `RpcResponse`
- `read_frame`/`write_frame` (`lib.rs:679`, `lib.rs:693`)
- Windows `call_for_process`(`lib.rs:708`)는 요청을 보낼 **동일 pipe 연결**에서 현재 사용자와 기대 서버 PID를 검사한다. 호출자는 PID 재사용을 막도록 검증한 프로세스 핸들을 호출 완료까지 유지해야 한다. installer의 안전 승인·취소는 이 경로를 쓰며, RPC wire version·메서드·프레임 형식은 바꾸지 않는다.
- Windows 서비스는 시작 시 자기 프로세스 DACL에 현재 사용자 SID의 `PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE`, 토큰 DACL에 `TOKEN_QUERY`만 추가해 관리자 SSH와 일반 데스크톱 사이에서도 기존 SID 검증을 수행할 수 있게 한다. 기존 ACE를 유지하고 강제 종료·메모리·토큰 수정 권한은 추가하지 않는다. 설정 실패는 `PEER_ACCESS_SETUP_FAILED`로 시작을 중단하며, 확인 실패를 같은 사용자로 간주하는 fallback은 없다. 0.16.6 격리 실행에서 관리자 서비스 → 일반 Session 1 조회와 일반 권한 서비스의 시작·응답을 확인했다.
- `resetCredits.read({accountId,refresh?})`는 저장된 결과 또는 명시적 공급자 조회, `resetCredits.redeem({accountId,quoteId,creditId,requestId,confirmed})`는 확인된 한 번의 사용이다. 두 메서드만 RPC 읽기 제한 90초를 사용하며 나머지는 60초다(`lib.rs:738`, 쓰기 제한은 5초 `lib.rs:741`). 전송 실패를 mutation 재시도 근거로 쓰지 않는다.

## 주요 엔티티
`Account`, `Session`, `Snapshot`, `Policy`(`Default` 구현 `lib.rs:337-357`), `ProcessIdentity`(`lib.rs:372`), `LaunchIntent`, `LeaseGrant`, `Decision`, `ProjectRoute`, `ApiError`, `CreditsState`, `ExtraUsageState`, `AccountQuotaSummary`(+`CreditsSummary`·`ExtraUsageSummary`·`ExpiringQuota`). `process_identity` 함수는 macOS `src/process.rs:70`, Windows `src/process.rs:113`에 구현이 있고 그 밖의 OS는 `UNSUPPORTED_PLATFORM` 오류를 돌려주는 stub(`src/process.rs:125`)이다.

`Account.omp_credential_pins`(`Vec<OmpCredentialPin{provider,hash}>`, `#[serde(default)]`, `lib.rs:230-232`)는 원본 OMP usage identity의 scope/case를 보존한 관측용 digest다.

`Policy`의 `#[serde(default)]` 필드(`lib.rs:294-336`): `allocation_mode`(`Smart`/`Priority`, 기본 `Smart`), `auto_takeover`(기본 true), `provider_pins`(공급자→계정 ID), `expiring_boost`(기본 false), `expiring_window_hours`(기본 48), `expiring_min_percent`(기본 30). 구버전 저장 정책에 없어도 읽힌다.

`ObservedAttribution.route`는 선택 필드(`bridge` / `direct` / `unknown`, 누락·null은 확인 불가)다. 요청별 hook에서 관측한 경로만 나타내며 응답 완료나 계정 identity를 승격하지 않는다. 역할을 알 수 없는 경로 기록의 `role`은 `unknown`이다. 과거 공급자 이름으로 route를 채우지 않는다.

`Snapshot.quotaSummaries`는 서비스가 계산한 현재 계정 그룹별 한도 요약이다(`#[serde(default)]`, 구서비스에는 누락될 수 있음). 각 `AccountQuotaSummary`는 `accountIds`, `kind`(available/reserve/partial/resting/excluded/login/unknown), `until`, `models`, `label`, `rate`, 선택 필드 `expiring`을 담는다. 실제 실행 입장 허용은 `route.explain`/`lease.acquire`가 판단한다. 화면은 누락된 요약을 자체 계산해 성공으로 대체하지 않는다.
새 필드(모두 `#[serde(default)]`, `types.ts` 미러): `Account.credits`(`CreditsState`), `Account.extra_usage`(`ExtraUsageState`), `Policy.use_credits_after_limit`·`use_extra_usage_after_limit`(기본 false), `LeaseGrant.credits_fallback`·`extra_usage_fallback`, `AccountQuotaSummary.credits`·`extra_usage`와 `kind`의 `credits`/`extra`. 크레딧은 ChatGPT 크레딧 단위(금액 아님), 추가 사용량은 USD다. 둘을 섞지 않는다.
- 초기화권은 `src/reset.rs`의 `ResetCredit`·`ResetInventory`·`ResetOperation`, 화면 미러는 `apps/desktop/src/resets.ts`다. 유료 `CreditsState`와 별개다. inventory의 `known/unknown/unsupported`와 nullable 수량을 구분하고, operation의 `preparing/submitted/unknown/succeeded/declined/resolved`를 그대로 유지한다. `resolved`는 이전 제안이 끝났다는 뜻이지 이 요청의 성공 증거가 아니다.

## 호환성
- 새 필드는 `#[serde(default)]`로 추가한다. `ProjectRoute`는 `deny_unknown_fields`라 필드를 더 보내면 실패한다.
- 타입을 바꾸면 `apps/desktop/src/types.ts`(초기화권은 `resets.ts`)도 함께 고친다. `types.ts`의 `LaunchIntent`(`types.ts:213-220`)는 `tool`·`model`·`cwd`·`accountId`·`parentSessionId`·`resumeSessionId` 여섯 필드를 선언하고, Rust `LaunchIntent`의 `nativeSessionId`·`adopted`·`continueElsewhere`(`lib.rs:548-555`)는 아직 미러하지 않았다.

## Screen Flow / Lifecycle
<!-- screen-flows-v3: 2026-09-27, type=library -->

| Stage | 상태 | 근거 |
|---|---|---|
| Module | ✅ | crates/protocol/Cargo.toml:2 package aam-protocol (workspace member Cargo.toml:3); entry crates/protocol/src/lib.rs:1-11 (`mod ipc`/`process`/`reset`, `pub mod secure`, `#[cfg(windows)] pub mod winutil` 및 재내보내기). |
| Function | ⚡ | lib.rs의 pub fn/struct/enum/const 59개(메서드·cfg 변형 포함). 5 consts (lib.rs:27-35), 자유 함수 now_ms :36, new_id :42, pin_provider :56, user_home :66, project_allowed :360, read_frame :679, write_frame :693, call :702, Windows 전용 service_pid :15·call_for_process :708; process.rs: process_identity(macOS :70 / Windows :113 / stub :125), process_alive :133. |
| Example | 🚧 | crates/protocol에는 README·examples/ 없음. 사용 패턴은 테스트뿐: crates/protocol/tests/process.rs(macOS 전용, 2 tests), tests/windows_job.rs(Windows 전용, 2 tests), lib.rs:762 `attribution_compatibility_tests`. |

```
aam_protocol (crates/protocol, 워크스페이스 버전 0.17.0)
├── [modules]  lib.rs:1-11
│   ├── mod ipc (비공개) → pub use connect, peer_is_self, LocalListener, LocalStream   lib.rs:1,8
│   ├── mod process → pub use process_alive, process_identity                          lib.rs:2,9
│   ├── mod reset → pub use ResetCredit, ResetInventory, ResetOperation                lib.rs:3-4
│   ├── pub mod secure (restrict_dir, open_owned_file, restrict_file …)                lib.rs:5
│   └── #[cfg(windows)] pub mod winutil → job_name, process_parent, spawn_in_job, ProcessJob  lib.rs:6-7,10-11
├── const PROTOCOL_VERSION: u32 = 1            lib.rs:27
├── const MAX_FRAME: usize = 1 MiB             lib.rs:28
├── const NATIVE_DEFAULT_MODEL = "native-default" lib.rs:30
├── const INTEGRATION_VERSION: u32 = 2 ///     lib.rs:33
├── const BRIDGE_PORT: u16 = 4020 ///          lib.rs:35
├── now_ms() -> i64                            lib.rs:36
├── new_id() -> String                         lib.rs:42
├── #[cfg(windows)] service_pid(&Paths) -> io::Result<u32> ///  lib.rs:15
├── struct Paths { home, socket, database, profiles }  lib.rs:48
│   ├── discover() -> io::Result<Paths>        lib.rs:74
│   ├── prepare(&self) -> io::Result<()>       lib.rs:113
│   ├── bridge_settings(&self) -> PathBuf ///  lib.rs:123
│   └── bridge_token(&self) -> PathBuf ///     lib.rs:127
├── pin_provider(name) -> &str ///             lib.rs:56
├── user_home() -> Option<PathBuf>             lib.rs:66
├── [accounts]
│   ├── struct QuotaBucket                     lib.rs:134
│   ├── struct CreditsState ///                lib.rs:149
│   ├── struct ExtraUsageState ///             lib.rs:175
│   ├── struct OmpCredentialPin                lib.rs:198
│   ├── struct Account                         lib.rs:204
│   ├── struct ToolStatus                      lib.rs:236
│   └── struct IdentityEvidence                lib.rs:559
├── [routing / policy]
│   ├── enum RouteScope { Directory, Repository }          lib.rs:248
│   ├── enum RouteMode { Pinned, Automatic, Unmanaged }    lib.rs:254
│   ├── struct ProjectRoute (deny_unknown_fields)          lib.rs:261
│   ├── struct RouteResolution                             lib.rs:271
│   ├── enum AllocationMode { Smart*, Priority }           lib.rs:283
│   ├── struct Policy + impl Default                       lib.rs:294,337
│   ├── project_allowed(&Policy, account_id, cwd) ///      lib.rs:360
│   ├── struct Candidate                                   lib.rs:590
│   └── struct Decision                                    lib.rs:610
├── [sessions / leases]
│   ├── struct ProcessIdentity { pid, started_at, boot_id } lib.rs:372
│   ├── struct Session                                     lib.rs:379
│   ├── struct ObservedSession ///                         lib.rs:409
│   ├── struct ObservedAttribution                         lib.rs:428
│   ├── struct Takeover ///                                lib.rs:454
│   ├── struct LaunchIntent                                lib.rs:540
│   ├── struct LaunchPlan                                  lib.rs:566
│   └── struct LeaseGrant                                  lib.rs:574
├── [snapshot]
│   ├── struct Notice                                      lib.rs:445
│   ├── struct Snapshot                                    lib.rs:466
│   ├── struct AccountQuotaSummary                         lib.rs:491
│   ├── struct ExtraUsageSummary ///                       lib.rs:511
│   ├── struct CreditsSummary ///                          lib.rs:522
│   └── struct ExpiringQuota                               lib.rs:530
├── [reset]  (mod reset, re-exported lib.rs:4)
│   ├── struct ResetCredit                                 reset.rs:5
│   ├── struct ResetInventory                              reset.rs:19
│   └── struct ResetOperation                              reset.rs:33
├── [rpc]
│   ├── struct RpcRequest { version, id, method, params }  lib.rs:620
│   │   └── new(method: &str, params: Value) -> Self       lib.rs:628
│   ├── struct ApiError { code, message, retryable }       lib.rs:639
│   │   └── new(code: &str, message: impl Into<String>)    lib.rs:645
│   ├── struct RpcResponse { version, id, result, error }  lib.rs:655
│   │   ├── ok(id, result: Value) -> Self                  lib.rs:662
│   │   └── err(id, error: ApiError) -> Self               lib.rs:670
│   ├── read_frame<R: Read, T>(&mut R) -> io::Result<T>    lib.rs:679
│   ├── write_frame<W: Write, T>(&mut W, &T) -> io::Result<()> lib.rs:693
│   ├── call(&Paths, method, params) -> Result<Value, ApiError> lib.rs:702
│   └── #[cfg(windows)] call_for_process(&Paths, method, params, pid) ///  lib.rs:708
└── [process]  (mod process, re-exported lib.rs:9)
    ├── process_identity(pid: u32) -> Result<ProcessIdentity, ApiError> /// process.rs:70 (macOS) / :113 (Windows) / :125 (그 외 OS: UNSUPPORTED_PLATFORM stub)
    └── process_alive(&ProcessIdentity) -> bool ///        process.rs:133

/// = rustdoc 주석 있음 (lib.rs pub fn/struct/enum/const 59개 중 17개, 메서드 `usable`·`exclusion_codes` 포함)
```

### 이슈
- [medium] Function: lib.rs pub fn/struct/enum/const 59개 중 42개는 rustdoc을 갖지 않는다. IPC 계약(read_frame/write_frame lib.rs:679-701, call lib.rs:702, MAX_FRAME/PROTOCOL_VERSION lib.rs:27-28)과 이 계약이 내는 오류 코드(DAEMON_UNAVAILABLE/DAEMON_UNTRUSTED/IPC_ERROR/PROTOCOL_MISMATCH, lib.rs:712-760)가 문서화돼 있지 않다.
- [low] Function: NATIVE_DEFAULT_MODEL 설명은 일반 `//` 주석(lib.rs:29)이라 rustdoc에 보이지 않는다.
- [medium] Example: 프레임 코덱(길이 0/초과 거부 lib.rs:683,695), call()의 id/version 불일치(lib.rs:747), Paths::discover socket 도출(lib.rs:74-111)을 직접 검증하는 테스트는 아직 crate에 추가되지 않았다. 간접 검증은 launcher 테스트의 `LocalListener` 가짜 서버(crates/launcher/src/lib.rs:1827) 한 곳이다.
- [low] Example: crates/protocol/tests/process.rs는 `#![cfg(target_os = "macos")]`(1행)로 제한되고, Windows 구현(process.rs:113)은 tests/windows_job.rs가 쓰며, 그 밖의 OS stub(process.rs:125)은 대응하는 테스트 파일을 갖지 않는다. crates/protocol에는 클라이언트 사용 예제와 README가 포함되지 않았다.
- [low] Function: launcher bounded_call(crates/launcher/src/lib.rs:46-79)은 call_inner(lib.rs:712-760)와 비슷한 프레이밍·검증을 반복한다(타임아웃·오류 문구만 다름). protocol::call은 읽기 60s(resetCredits.read/redeem은 90s)·쓰기 5s로 고정한다(lib.rs:738-741).
- [low] Module: 타입을 apps/desktop/src/types.ts에 손으로 미러하며 생성·검사 단계는 저장소에 두지 않았다. 예: TS LaunchIntent(types.ts:213-220)는 Rust의 nativeSessionId/adopted/continueElsewhere(lib.rs:548-555)를 선언하지 않는다. `#[serde(default)]` 덕에 오늘은 TS가 해당 필드를 보내지 않아도 Rust가 읽지만 드리프트 위험이다.

### 다음 할 일
- [ ] Function: wire format(4바이트 BE 길이 + JSON, MAX_FRAME, PROTOCOL_VERSION)을 설명하는 //! crate doc과 read_frame, write_frame, call(타임아웃·오류 코드), Paths::discover/prepare, RpcRequest/RpcResponse/ApiError의 /// 문서를 추가하고 lib.rs:29를 ///로 바꾼다.
- [ ] Example: 프레임 왕복, len==0 / >MAX_FRAME 거부, `LocalListener` 가짜 서버(패턴: crates/launcher/src/lib.rs:1827)를 이용한 call() PROTOCOL_MISMATCH를 crate 안 테스트로 추가할 것.
- [ ] Function: consider `call_with_timeout(paths, method, params, Duration)` in protocol so launcher bounded_call reuses it instead of duplicating framing/validation.
- [ ] Module: add a Rust↔TS drift check (e.g. ts-rs/specta generation or a serde snapshot test) for apps/desktop/src/types.ts.
