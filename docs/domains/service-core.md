# service-core (`crates/service`)

## 개요
`aam-service` 데몬. 로컬 RPC 서버(macOS Unix socket, Windows named pipe) + SQLite 저장소. 계정·정책·lease/세션·라우팅·배정·takeover·quota refresh·진단 내보내기를 맡는다. omp 브릿지는 [omp-bridge](omp-bridge.md) 참조.

## 진입점
- `main.rs:1` — `aam-service --foreground`
- `server::run` (`server.rs:92`) — `service.lock` 단일 인스턴스(`server.rs:112-117`), peer uid == euid 검사(`server.rs:70-77`), 연결 최대 64개(`server.rs:159`)
- `Service::dispatch` (`lib.rs:346`) — RPC 분기
- `Service::start_background` (`lib.rs:1557`) — reconcile 2초 주기, quota refresh 225–375초(지터), 브릿지 시작

## RPC 메서드 (`lib.rs:346-452`)
`status.read`의 Snapshot은 `serviceVersion`(서비스 실행 파일의 `CARGO_PKG_VERSION`)을 담는다. 앱·`aam`이 자기 버전과 비교해 덮어쓴 앱 뒤에 남은 예전 서비스를 찾는 데 쓴다. 이 필드가 없던 서비스는 응답에 빠져 있고, 받는 쪽은 '알 수 없음'(재시작 필요)으로 본다. `service.prepareUninstall`은 `aam service restart`도 쓰는 lease 검사 창구다.
`status.read`, `quota.refresh`, `service.prepareUninstall`, `service.cancelUninstall`, `service.resumeAdmission`, `route.resolve`, `route.explain`, `takeover.adopt`, `takeover.release`, `lease.validate-child`, `policy.update`, `bridge.status`, `diagnostics.export`, `account.register`, `account.update`, `lease.acquire`, `lease.starting`, `lease.started`, `lease.heartbeat`, `lease.release`, `lease.abort`. 나머지는 `METHOD_NOT_FOUND`.

## 저장소 (`store.rs:62-104`)
SQLite `state.sqlite3`, WAL, `synchronous=FULL`, busy_timeout 5초. 테이블 `accounts`, `metadata`, `leases`. `user_version` 1보다 크면 거부.

## Lease 상태기계
- 용량을 점유하는 상태: `PREPARED`, `STARTING`, `ACTIVE`, `SUSPECT`, `ORPHANED` (`scheduler.rs:11-16`). 종료: `EXITED`, `ABORTED`, `FAILED`.
- `lease.acquire` → PREPARED. `requestId` 기준 멱등, 같은 id·다른 payload는 `IDEMPOTENCY_CONFLICT` (`lib.rs:853-866`).
- `lease.starting` → STARTING. generation 불일치 `LEASE_FENCED`, 만료 `LEASE_EXPIRED`, 정책 변경 `POLICY_CONFLICT`, 재입장 검사 실패 `ADMISSION_CHANGED`. evidence는 `preflight-verified`이고 identity_key가 같아야 한다 (`lib.rs:1140-1149`).
- `lease.started` → 검증된 자식 프로세스면 ACTIVE, 아니면 SUSPECT.
- reconcile(`lib.rs:1464-1565`): 타임아웃만으로 슬롯을 풀지 않는다. 불확실하면 SUSPECT/ORPHANED로 남긴다. 자손 기록(`backgroundProcesses`)이 없는 세션은 기록된 process·supervisor가 모두 이전 부팅이면(`process::all_from_previous_boot`) EXITED로 반환한다. 같은 부팅 안에서는 null을 빈 목록으로 보지 않는다. 부팅 기반 반환은 macOS만. Windows는 `lease.started` 때 연 Job 핸들의 활성 프로세스 수 0만 EXITED 근거로 쓴다(`Service::job_finished`, [windows-port](../specs/2026-09-29-windows-port.md)).
- capability는 UUID 2개 연결, 상수 시간 비교, 불일치 `LEASE_FORBIDDEN` (`lib.rs:201-217`).
- 환경변수: `AAM_PREPARED_TIMEOUT_MS`(기본 30000), `AAM_SUSPECT_THRESHOLD_MS`(기본 45000) (`lib.rs:242-243`).

## 배정 (`scheduler.rs`)
- 동시성은 identity 그룹(같은 provider + identity_key 또는 omp credential pin) 단위로 센다 (`scheduler.rs:181-210`).
- 안전 여유량은 소프트 우선순위다. 하드 제약을 통과한 roomy 후보가 있으면 reserve 후보를 뒤로 보내고(`RESERVE_DEPRIORITIZED`), 없으면 남은 한도를 사용한다(`RESERVE_FALLBACK`). 100% 사용은 계속 `QUOTA_EXHAUSTED`로 제외한다. 명시 계정·프로젝트 pin·재개 제약은 유지하며 공급자 선호 pin보다 roomy 후보가 우선한다.
- 자동 배정은 신선한 관측(`stale_after_seconds` 기본 900초)과 미래 reset이 필요하다. pinned 실행은 신선도 검사를 건너뛴다 (`scheduler.rs:259-264,319-344`).
- 기본 모델 요청은 모델 전용 버킷이 막지 않고 우선순위만 낮춘다 (`scheduler.rs:26-42,272-298`).
- `quota_summary.rs`가 계정 그룹별 `available/reserve/partial/resting/excluded/login/unknown`을 `Snapshot.quotaSummaries`로 제공한다. 최신 bucket과 실제 bridge 차단을 합치되, 이메일 근거가 여러 workspace에 걸치면 차단을 억지로 붙이지 않는다. 모델별 한도만 있는 공급자도 지원한다. 모델·프로젝트·슬롯이 없는 요약이므로 배정 허용은 별도 scheduler 판단이다.
- bridge 차단의 `quota` 구분을 요약에 유지한다. 오래된 버킷 때문에 소진된 한도의 이름을 특정하지 못하더라도 실제 사용량 제한을 요청 속도 제한으로 바꾸지 않으며, 공급자가 지정한 차단 시각은 그대로 유지한다.
- 곧 리셋(`quota_summary::expiring`): 모델 전용·5시간 같은 짧은 한도를 뺀 신선한 공용 한도가 `expiring_window_hours`(기본 48) 안에 리셋되고, 안전 여유량을 뺀 남은 양이 `expiring_min_percent`(기본 30) 이상이면 `quotaSummaries[].expiring`(라벨·리셋 시각·쓸 수 있는 %·시간당 %)을 채운다. 여러 개면 시간당 써야 할 양이 가장 큰 것 하나. `resting`/`unknown` 요약에는 붙이지 않는다.
- `expiring_boost`(기본 꺼짐)를 켜면 스마트 배정에서만 입장 조건을 통과한 곧 리셋 계정끼리 먼저 비교한다(`EXPIRING_PREFERRED`). 명시 계정·공급자 수동 배정·소비 순서 모드가 먼저이고, 안전 여유량 안쪽 후보는 여전히 뒤로 간다. 세 값은 `policy.update`로 바꾸며 범위는 1~168시간, 1~100%.
- 크레딧·추가 사용량 fallback(둘 다 정책 옵트인, 기본 꺼짐, 단위가 달라 섞지 않는다): `use_credits_after_limit`는 Codex 전용(`Account.credits` = 공식 app-server `account/rateLimits/read`의 `credits{hasCredits,unlimited,balance}`+`ordinaryUsageAllowed`, ChatGPT 크레딧 단위·금액 아님), `use_extra_usage_after_limit`는 Claude 전용(`Account.extra_usage` = omp 사용량 보고의 `anthropic:extra` USD 항목, `used/limit`). 둘 다 `policy.update`로 바꾼다. 꺼져 있으면 배정은 바뀌지 않는다. 켜져 있어도 구독 한도가 남아 입장 가능한 계정이 하나라도 있으면 그 계정이 항상 이긴다. 그런 계정이 하나도 없고(동시 사용 자리만 없는 계정이 있어도 기다린다), 후보가 `QUOTA_EXHAUSTED` 하나만으로 제외됐으며 소진 관측이 신선하고 리셋 시각이 남아 있고(모델 전용 한도가 아닌 공용 한도), 해당 과금 경로가 신선한 관측에서 명시된 계정(크레딧: `has_credits||unlimited`이고 잔액이 0 이하가 아님, `ordinaryUsageAllowed!=true`, `observed_at`이 `stale_after_seconds` 안 / 추가 사용량: `enabled`이고 `limit`이 없거나 `used<limit`)만 마지막 수단으로 뽑는다. 이유 코드 `CREDITS_FALLBACK`/`EXTRA_USAGE_FALLBACK`, `LeaseGrant.credits_fallback`/`extra_usage_fallback`. 선택 상태는 저장하지 않고 매 판정에서 새로 계산하므로 리셋·회복 관측이 들어오면 다음 판정부터 구독 계정이 이긴다. 크레딧 응답에 없거나 오래된 값은 "없음"으로 본다.
- `quotaSummaries`는 매 스냅샷마다 다시 계산한다. 옵트인이 켜져 있고 같은 도구의 구독 한도가 남은 묶음이 없을 때 `resting` 묶음이 `kind: "credits"`/`"extra"`가 되고(`credits.active`/`extraUsage.active`), 옵트인이 꺼져 있으면 `resting`/`available` 그대로 두고 `credits`/`extraUsage` 사실(`active:false`)만 붙여 화면이 중립 안내("크레딧 있음 (꺼짐)")를 보여 준다. 크레딧 `balance`는 숫자로 읽힐 때만 보관하며 화면은 "N개"로 반올림한다. 금액(USD)은 추가 사용량에만 쓴다.
- 한계(미검증): 이 Mac의 어떤 계정도 `has_credits=true`나 Claude extra usage 켜짐을 관측한 적이 없다. 공식 JSON 스키마(`codex app-server generate-json-schema`의 `CreditsSnapshot`)와 omp `claude.ts` 소스의 필드 형태로 만든 fixture로만 시험했다. Ojak은 자신이 라우팅하는 새 세션·요청만 통제한다. 이미 실행 중인 공식 Codex·Claude Code 세션이 구독 한도 뒤에 과금 경로로 넘어가는 것은 공급자가 정하므로 Ojak이 멈출 수 없다. 쓴 금액도 Ojak은 볼 수 없다.

## 라우팅 (`routes.rs:189-427`)
우선순위: takeover → resume/parent(원래 계정 고정, 다르면 `SWITCH_UNSUPPORTED`) → 가장 깊은 폴더 규칙 → 저장소 규칙(git) → 명시 계정/전역. 경로가 바뀌면 `ROUTE_CHANGED`.

### 다른 계정에서 이어 가기 (Claude Code·Codex)
- `LaunchIntent.continue_elsewhere` + `resume_session_id`(관리 세션). `routes::resolve`가 원래 세션 계정을 `RouteResolution.excluded_account_id`로 넘기고 모델은 원래 대화 모델, 프로젝트 규칙은 적용하지 않는다. `scheduler`는 그 계정과 같은 실제 계정을 `CONTINUE_SOURCE`로 뺀다. 새 세션은 같은 native ID를 쓰고 `Session.continued_from`에 원래 세션을 남긴다.
- `routes::original`은 같은 native 대화의 관리 세션을 시간순으로 보고 마지막 세션의 계정으로 재개한다. 계정이 바뀐 곳은 반드시 바로 앞 세션에서 이어 간 기록(`continued_from`)이어야 하고, 아니면 `RESUME_UNVERIFIED`.
- 대화 기록 복사(`adapters::copy_conversation`: Claude `projects/<폴더>/<native>.jsonl`, Codex `sessions/<년>/<월>/<일>/rollout-*-<native>.jsonl`, 원자적 교체·사용자 전용)는 실행기가 preflight 안에서 한다. 로그인·설정은 옮기지 않는다.
- Codex는 시작 때 대화 ID를 정할 수 없다. 실행기가 실행 중 자기 프로세스 트리(spawn 때 확인한 birth identity의 루트와 관측한 자손, 매 샘플에서 birth가 같은 프로세스만; 루트 birth가 바뀌면 그 PID는 더 조회하지 않음)가 쓰기 모드로 연 `.jsonl`을 200ms마다 모으고(`launcher::WriterWatch`, macOS libproc; 다른 OS는 비어 있어 매핑 안 함), 종료 뒤 그중 그 계정 프로필 `sessions/` 안 rollout이면서 작업 폴더가 같은 대화가 정확히 하나일 때만 ID로 쓴다(`adapters::codex_session_from`). 시각만 맞는 외부 Codex의 rollout은 근거로 쓰지 않는다. 찾은 ID는 `lease.release`의 `nativeSessionId`로 보고하고, 서비스는 다른 세션이 쓰지 않는 ID만 기록한다. 재개는 `codex resume <id>`, 비대화형은 `codex exec resume <id>`.
- 실행기: 대화형 실행이 끝났을 때 쓰던 계정이 `QUOTA_EXHAUSTED`이고 다른 계정이 있으면 `[Y/n]`으로 묻는다(Enter=예). 수동으로는 `aam continue [--tool] [--session] [--account] [-- <인수>]`(최신 사용량 조회 후 실행).

### 공급자 수동 배정 (`Policy.provider_pins`)
- 키는 공급자(`aam_protocol::pin_provider`: claude·anthropic→`anthropic`, codex·openai-codex→`openai`, …), 값은 계정 ID. 없으면 그 공급자는 자동 배정.
- 고정이 아니라 우선순위다. `scheduler::decide_with`는 입장 조건을 통과한 후보 중 고정 계정(같은 실제 계정, `same_identity`)을 먼저 고르고, 통과하지 못하면 자동 선택으로 넘어가며 `Decision.pin_unavailable`·`LeaseGrant.pin_unavailable`로 알린다. 실행기는 터미널에 안내를 출력한다. 자동 배정을 꺼 두면 고정 계정만 쓰고 넘기지 않는다.
- omp 브릿지는 진행 중 대화(sticky)를 유지하고, 새 대화만 고정 계정(`Candidate.pinned`)을 먼저 준다. 진행 중 대화는 프롬프트 캐시를 지키려고 안전 여유량 안쪽이어도 같은 계정에 두고, 소진·차단·장애로 후보에서 빠질 때만 옮긴다. 새 대화는 고정 계정이 소진·차단·여유량 안쪽이면 다른 계정으로 간다.
- 이전 판의 도구별 `preferredAccounts`는 읽을 때 공급자 키로 옮긴다(`store::policy`).

## 진단 (`diagnostics.rs:47-141`)
`redact:false`는 거부. 허용 필드만 다시 조립하고 계정·세션은 임시 별칭으로 바꾼다.

## 테스트
`crates/service/src/tests.rs` + `src/tests/{allocation,routing,takeover}.rs`. 다수가 macOS 전용.

## Screen Flow / Lifecycle
<!-- screen-flows-v3: 2026-09-27, type=web-backend -->

| Stage | 상태 | 근거 |
|---|---|---|
| Account.Create | ✅ | RPC account.register lib.rs:405 → Service::register lib.rs:722-836 (label validation 723-731, SESSION_BUSY guard 754-764, supersede/policy rewrite 765-832, save |
| Account.Read | ✅ | status.read lib.rs:348 → snapshot lib.rs:255-345 (store.rs accounts(), stale-bucket marking in snapshot_with_observations lib.rs:263-290). Callers: apps/desktop |
| Account.Update | ✅ | account.update lib.rs:406 → update_account lib.rs:690-721 (enabled; maxConcurrency 1..32 with ADAPTER_UNVERIFIED for shared-profile >1; save_account lib.rs:718) |
| Account.Delete | ⚡ | dispatch has no account.delete/remove RPC (lib.rs:346-453), and no UI/CLI caller exists. Deletion is internal only: register supersede DELETE lib.rs:774 plus po |
| Policy/ProjectRoute.Create | ✅ | Singleton policy seeded with Policy::default at store.rs:102-104. Routes are created through policy.update lib.rs:402 → update_policy lib.rs:592-594 → routes::v |
| Policy/ProjectRoute.Read | ✅ | Snapshot.policy (lib.rs:263-345, store.rs policy()). route.resolve/route.explain lib.rs:364-398 → routes::resolve routes.rs:189 and scheduler::decide_with sched |
| Policy/ProjectRoute.Update | ✅ | update_policy lib.rs:503-607: CAS on expected_revision (POLICY_CONFLICT lib.rs:510-515), per-field validation, revision++ and set_metadata lib.rs:604. UI: apps/ |
| Policy/ProjectRoute.Delete | ✅ | Route removal is policy.update with a filtered list: apps/desktop/src/AllocationSettings.tsx:140 (routes.filter → save). Policy itself is a non-deletable singleton. Dan |
| Lease/Session.Create | ✅ | lease.acquire lib.rs:407-442 (stale-quota refresh wait) → acquire lib.rs:837-1016: idempotent by request_id (IDEMPOTENCY_CONFLICT lib.rs:856), admission_allowed |
| Lease/Session.Read | ✅ | status.read sessions: every capacity-holding lease plus the latest 200 terminal ones (lib.rs:291-300, cap at 298). lease.validate-child lib.rs:401 → managed_ses |
| Lease/Session.Update | ✅ | lease.starting lib.rs:443 → starting lib.rs:1025-1163 (authorize lib.rs:201-217, generation fence, expiry→ABORTED lib.rs:1081-1082, POLICY_CONFLICT lib.rs:1097, |
| Lease/Session.Delete | ✅ | lease.release/lease.abort lib.rs:446-447 → finish lib.rs:1259-1338: PREPARED→ABORTED lib.rs:1308-1309, spawn-failed→FAILED lib.rs:1316, foreground-confirmed→EXI |
| Takeover.Create | ✅ | takeover.adopt lib.rs:399 → adopt_takeover lib.rs:609-664 (checked() requires claude plus UUID lib.rs:95-121, aam_adapters::session_owner proof, rejects already |
| Takeover.Read | ✅ | Snapshot.takeovers, filtered to not-yet-managed conversations, lib.rs:318-328 and 343. CLI aam takeover list crates/launcher/src/main.rs:428-446. Desktop declar |
| Takeover.Update | ❌ | dispatch has no takeover.update RPC (lib.rs:346-453). Re-adopting drops the same tool+native record (filter lib.rs:647-655) and pushes a new one (lib.rs:660), s |
| Takeover.Delete | ✅ | takeover.release lib.rs:400 → release_takeover lib.rs:665-689 (TAKEOVER_NOT_FOUND when nothing was removed, set_takeovers lib.rs:686). Records that are already  |

```mermaid
stateDiagram-v2
  [*] --> PREPARED: lease.acquire (lib.rs:1009)
  PREPARED --> STARTING: lease.starting (lib.rs:1151)
  PREPARED --> ABORTED: expiry / release / abort / restart reconcile (lib.rs:224, 1082, 1309, 1505-1512)
  STARTING --> ACTIVE: lease.started verified (lib.rs:1233)
  STARTING --> SUSPECT: started unverified / heartbeat stale / release unconfirmed
  STARTING --> FAILED: lease.abort spawn-failed (lib.rs:1316)
  SUSPECT --> FAILED: lease.abort spawn-failed, no process
  SUSPECT --> ACTIVE: lease.started / reconcile alive (lib.rs:1539)
  ORPHANED --> ACTIVE: lease.started / reconcile alive (lib.rs:1539)
  ACTIVE --> SUSPECT: child dead + supervisor alive / heartbeat stale (lib.rs:1527, 1541)
  ACTIVE --> ORPHANED: supervisor dead / background unverified (lib.rs:1323, 1529-1534)
  SUSPECT --> ORPHANED: child dead + supervisor dead (lib.rs:1529)
  ORPHANED --> SUSPECT: release unconfirmed, process not alive (lib.rs:1325)
  ACTIVE --> EXITED: release fg-confirmed / background finished (lib.rs:1319, 1514)
  SUSPECT --> EXITED: release fg-confirmed / background finished
  ORPHANED --> EXITED: release fg-confirmed / background finished / all identities from previous boot (lib.rs:1512)
  ABORTED --> [*]
  FAILED --> [*]
  EXITED --> [*]
  note right of ORPHANED: holds capacity (scheduler.rs:11-15), never released by timeout
```

| Entity | Create | Read | Update | Delete |
|---|---|---|---|---|
| Account | ✅ account.register | ✅ status.read | ✅ account.update (maxConcurrency has no UI) | ⚡ internal only |
| Policy/ProjectRoute | ✅ seed + policy.update | ✅ status.read, route.resolve/explain | ✅ policy.update (CAS) | ✅ policy.update filtered list |
| Lease/Session | ✅ lease.acquire | ✅ status.read, validate-child, diagnostics | ✅ starting/started/heartbeat + reconcile | ✅ release/abort (soft, rows kept) |
| Takeover | ✅ takeover.adopt (CLI) | ✅ status.read, aam takeover list | ❌ none (adopt = upsert) | ✅ takeover.release (CLI) |

### 이슈
- [high→partly fixed: released after reboot] Lease/Session.Delete: A stuck lease has no way out. Reconcile leaves ORPHANED unchanged when child and supervisor are both dead and no background list exists (lib.rs:1528-1529). A STARTING/SUSPECT lease with no registered process and a dead s
- [high] Account.Create: [INFERENCE, not run] register() supersede runs DELETE FROM accounts WHERE id=?1 (lib.rs:774) without the NOT EXISTS leases guard that apply_scan uses (lib.rs:1427). leases.account_id REFERENCES accounts(id) (store.rs:84)
- [medium] Lease/Session.Delete: Lease rows are never pruned: store.rs has no DELETE FROM leases, and the snapshot only trims what it displays to 200 terminal leases (lib.rs:298). expire() (lib.rs:221-232), snapshot, acquire, and the 2s reconcile all do
- [low] Account.Delete: No user-initiated account removal RPC. Disconnected or unobserved identities build up as unverified rows (lib.rs:1435-1443) and are only removed through internal supersede/obsolete paths.
- [low] Account.Update: maxConcurrency is validated in the backend (update_account lib.rs:690-721) but has no UI or CLI caller. PolicyView.tsx:47 only sends enabled.
- [low] Policy/ProjectRoute.Update: projectAllowlist and autoTakeover can be set via policy.update (lib.rs:503-607) but have no UI setter (only declared in apps/desktop/src/types.ts:74,77) and no CLI command.
- [low] Takeover.Create: Takeover adopt/list/release is CLI-only (crates/launcher/src/main.rs:428-459). The desktop app neither allowlists takeover.* nor renders snapshot.takeovers.

### 다음 할 일
- [ ] Account: guard the supersede DELETE at lib.rs:774 like apply_scan (NOT EXISTS leases) or re-point historical leases before deleting; add a test for re-login when the old binding has EXITED lease history.
- [ ] Lease: add retention that prunes terminal leases past N rows or an age limit, and index/filter holds_capacity states so expire/reconcile do not scan all history.
- [ ] Account: decide whether an explicit account.remove RPC (reusing forget_account lib.rs:191-200) is wanted, and wire it to the UI if so.
- [ ] UI: expose maxConcurrency, projectAllowlist, and autoTakeover, or document them as CLI/raw-RPC only; add takeover list/release to the desktop if intended.
