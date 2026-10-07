# protocol (`crates/protocol`)

## 개요
공유 serde 타입, 상수, 프로세스 identity, 로컬 RPC 클라이언트/코덱(macOS Unix socket, Windows named pipe — `ipc.rs`, 권한 도우미 `secure.rs`, Windows API `winutil.rs`). `apps/desktop/src/types.ts`가 같은 타입을 TypeScript로 복제한다.

## 상수 (`src/lib.rs`)
| 이름 | 값 | 위치 |
|---|---|---|
| `PROTOCOL_VERSION` | 1 | `lib.rs:12` |
| `MAX_FRAME` | 1 MiB | `lib.rs:13` |
| `NATIVE_DEFAULT_MODEL` | `native-default` (모델 미지정, `--model` 주입 안 함) | `lib.rs:15` |
| `INTEGRATION_VERSION` | 2 (integration.json이 진입 경로를 저장) | `lib.rs:18` |
| `BRIDGE_PORT` | 4020 | `lib.rs:20` |

## 경로 (`Paths::discover`, `lib.rs:40`)
- `AAM_HOME` 기본값: `~/Library/Application Support/AI Account Manager`
- 하위: `state.sqlite3`, `profiles/`, `bin/`(shim), `bridge.json`, `bridge.token`, `integration.json`, `logs/`
- socket: `$TMPDIR/aam-<euid>-<fnv(AAM_HOME)>/control.sock`, 디렉터리 0700

## RPC
- `call` (`lib.rs:458`): 4바이트 길이 prefix + JSON `RpcRequest` → `RpcResponse`
- `read_frame`/`write_frame` (`lib.rs:435`, `lib.rs:449`)

## 주요 엔티티
`Account`, `Session`, `Snapshot`, `Policy`(기본값 `lib.rs:200-214`), `ProcessIdentity`(`lib.rs:218`), `LaunchIntent`, `LeaseGrant`, `Decision`, `ProjectRoute`, `ApiError`. `process_identity` 함수는 `src/process.rs:70` (macOS 전용).

`ObservedAttribution.route`는 선택 필드(`bridge` / `direct` / `unknown`, 누락·null은 확인 불가)다. 요청별 hook에서 관측한 경로만 나타내며 응답 완료나 계정 identity를 승격하지 않는다. 역할을 알 수 없는 경로 기록의 `role`은 `unknown`이다. 과거 공급자 이름으로 route를 채우지 않는다.

`Snapshot.quotaSummaries`는 서비스가 계산한 현재 계정 그룹별 한도 요약이다(`#[serde(default)]`, 구서비스에는 누락될 수 있음). 각 `AccountQuotaSummary`는 `accountIds`, `kind`(available/reserve/partial/resting/excluded/login/unknown), `until`, `models`, `label`, `rate`를 담는다. 실제 실행 입장 허용은 `route.explain`/`lease.acquire`가 판단한다. 화면은 누락된 요약을 자체 계산해 성공으로 대체하지 않는다.
새 필드(모두 `#[serde(default)]`, `types.ts` 미러): `Account.credits`(`CreditsState`), `Account.extra_usage`(`ExtraUsageState`), `Policy.use_credits_after_limit`·`use_extra_usage_after_limit`(기본 false), `LeaseGrant.credits_fallback`·`extra_usage_fallback`, `AccountQuotaSummary.credits`·`extra_usage`와 `kind`의 `credits`/`extra`. 크레딧은 ChatGPT 크레딧 단위(금액 아님), 추가 사용량은 USD다. 둘을 섞지 않는다.

## 호환성
- 새 필드는 `#[serde(default)]`로 추가한다. `ProjectRoute`는 `deny_unknown_fields`라 필드를 더 보내면 실패한다.
- 타입을 바꾸면 `apps/desktop/src/types.ts`도 함께 고친다.

## Screen Flow / Lifecycle
<!-- screen-flows-v3: 2026-09-27, type=library -->

| Stage | 상태 | 근거 |
|---|---|---|
| Module | ✅ | crates/protocol/Cargo.toml:2 package aam-protocol (workspace member Cargo.toml:3); entry crates/protocol/src/lib.rs:1-2 (`mod process; pub use process::{process |
| Function | ⚡ | 47 public items: 5 consts (lib.rs:12-20), 7 free fns (now_ms lib.rs:21, new_id :27, read_frame :435, write_frame :449, call :458, process_identity process.rs:70 |
| Example | 🚧 | No README in repo and no examples/ dir (glob **/README*.md and **/examples/** empty). Usage patterns only in tests: crates/protocol/tests/process.rs:1-25 (2 tes |

```
aam_protocol (crates/protocol, v0.1.0 workspace)
├── const PROTOCOL_VERSION: u32 = 1            lib.rs:12
├── const MAX_FRAME: usize = 1 MiB             lib.rs:13
├── const NATIVE_DEFAULT_MODEL = "native-default" lib.rs:15
├── const INTEGRATION_VERSION: u32 = 2 ///     lib.rs:18
├── const BRIDGE_PORT: u16 = 4020 ///          lib.rs:20
├── now_ms() -> i64                            lib.rs:21
├── new_id() -> String                         lib.rs:27
├── struct Paths { home, socket, database, profiles }  lib.rs:33
│   ├── discover() -> io::Result<Paths>        lib.rs:40
│   ├── prepare(&self) -> io::Result<()>       lib.rs:65
│   ├── bridge_settings(&self) -> PathBuf ///  lib.rs:74
│   └── bridge_token(&self) -> PathBuf ///     lib.rs:78
├── [accounts]
│   ├── struct QuotaBucket                     lib.rs:85
│   ├── struct OmpCredentialPin                lib.rs:97
│   ├── struct Account                         lib.rs:103
│   ├── struct ToolStatus                      lib.rs:128
│   └── struct IdentityEvidence                lib.rs:339
├── [routing / policy]
│   ├── enum RouteScope { Directory, Repository }          lib.rs:140
│   ├── enum RouteMode { Pinned, Automatic, Unmanaged }    lib.rs:146
│   ├── struct ProjectRoute (deny_unknown_fields)          lib.rs:153
│   ├── struct RouteResolution                             lib.rs:163
│   ├── enum AllocationMode { Smart*, Priority }           lib.rs:172
│   ├── struct Policy + impl Default                       lib.rs:183,200
│   ├── struct Candidate                                   lib.rs:361
│   └── struct Decision                                    lib.rs:369
├── [sessions / leases]
│   ├── struct ProcessIdentity { pid, started_at, boot_id } lib.rs:218
│   ├── struct Session                                     lib.rs:225
│   ├── struct ObservedSession ///                         lib.rs:252
│   ├── struct ObservedAttribution                         lib.rs:271
│   ├── struct Takeover ///                                lib.rs:294
│   ├── struct LaunchIntent                                lib.rs:324
│   ├── struct LaunchPlan                                  lib.rs:346
│   └── struct LeaseGrant                                  lib.rs:354
├── [snapshot]
│   ├── struct Notice                                      lib.rs:285
│   └── struct Snapshot                                    lib.rs:306
├── [rpc]
│   ├── struct RpcRequest { version, id, method, params }  lib.rs:376
│   │   └── new(method: &str, params: Value) -> Self       lib.rs:384
│   ├── struct ApiError { code, message, retryable }       lib.rs:395
│   │   └── new(code: &str, message: impl Into<String>)    lib.rs:401
│   ├── struct RpcResponse { version, id, result, error }  lib.rs:411
│   │   ├── ok(id, result: Value) -> Self                  lib.rs:418
│   │   └── err(id, error: ApiError) -> Self               lib.rs:426
│   ├── read_frame<R: Read, T>(&mut R) -> io::Result<T>    lib.rs:435
│   ├── write_frame<W: Write, T>(&mut W, &T) -> io::Result<()> lib.rs:449
│   └── call(&Paths, method, params) -> Result<Value, ApiError> lib.rs:458
└── [process]  (mod process, re-exported lib.rs:2)
    ├── process_identity(pid: u32) -> Result<ProcessIdentity, ApiError> /// process.rs:70 (macOS) / :113 (UNSUPPORTED_PLATFORM stub)
    └── process_alive(&ProcessIdentity) -> bool ///        process.rs:121

/// = has rustdoc comment (8 of 47 public items)
```

### 이슈
- [medium] Function: 39 of 47 public items lack rustdoc; the IPC contract (read_frame/write_frame lib.rs:435-457, call lib.rs:458, MAX_FRAME/PROTOCOL_VERSION lib.rs:12-13) and error codes it emits (DAEMON_UNAVAILABLE/IPC_ERROR/PROTOCOL_MISMA
- [low] Function: NATIVE_DEFAULT_MODEL explanation is a plain `//` comment (lib.rs:14), invisible to rustdoc.
- [medium] Example: Frame codec (zero-length/oversize rejection lib.rs:439,451), call() id/version mismatch (lib.rs:475) and Paths::discover socket derivation (lib.rs:40-64) have no tests in the crate; only indirect coverage via launcher/se
- [low] Example: crates/protocol/tests/process.rs is gated `#![cfg(target_os = "macos")]` (line 1); non-macOS stub process.rs:113 is untested. No README/examples showing client usage.
- [low] Function: launcher bounded_call (crates/launcher/src/lib.rs:42-72) duplicates call() (lib.rs:458-488) only to vary timeout/error text; protocol::call hardcodes 60s read / 5s write timeouts (lib.rs:465-470).
- [low] Module: Types are hand-mirrored in apps/desktop/src/types.ts with no generation/check; e.g. TS LaunchIntent (types.ts:171-178) omits Rust nativeSessionId/adopted (lib.rs:331-335) — drift risk (tolerated today by #[serde(default)

### 다음 할 일
- [ ] Function: add //! crate doc describing wire format (4-byte BE length + JSON, MAX_FRAME, PROTOCOL_VERSION) and /// docs on read_frame, write_frame, call (timeouts + error codes), Paths::discover/prepare, RpcRequest/RpcResponse/ApiError; convert lib.rs:14 to ///.
- [ ] Example: add in-crate tests for frame round-trip, len==0 / >MAX_FRAME rejection, and call() PROTOCOL_MISMATCH via a UnixListener fake (pattern exists at crates/launcher/src/lib.rs:987-992).
- [ ] Function: consider `call_with_timeout(paths, method, params, Duration)` in protocol so launcher bounded_call reuses it instead of duplicating framing/validation.
- [ ] Module: add a Rust↔TS drift check (e.g. ts-rs/specta generation or a serde snapshot test) for apps/desktop/src/types.ts.
