# service-core (`crates/service`)

## 개요
`aam-service` 데몬. 로컬 RPC 서버(macOS Unix socket, Windows named pipe) + SQLite 저장소. 계정·정책·lease/세션·라우팅·배정·takeover·quota refresh·진단 내보내기를 맡는다. omp 브릿지는 [omp-bridge](omp-bridge.md) 참조.

## 진입점
- `main.rs` — Windows 기본 실행/`--supervise`는 독립 감독 프로세스, `--worker`는 그 자식 서비스다. `--foreground`는 감독 없는 개발 실행이며 macOS의 기존 실행 방식은 유지한다.
- `server::run` (`server.rs:104`) — `service.lock` 단일 인스턴스(`service_lifetime_lock` `server.rs:90-94`, 호출 `server.rs:126`), peer uid == euid 검사(`aam_protocol::peer_is_self`; Unix는 연결 직후 `server.rs:181`, Windows는 요청 프레임을 읽은 뒤 `server.rs:206`), 연결 최대 64개(`server.rs:184`)
- `Service::dispatch` (`lib.rs:446`) — RPC 분기
- `Service::start_background` (`lib.rs:1864`) — reconcile 2초 주기, quota refresh 225–375초(지터), 브릿지 시작

### Windows 복구와 런타임 진단
- `supervisor.rs`는 작업자 종료를 확인한 뒤 1초부터 최대 60초까지 지수 간격으로 재시작한다. 연속 실패 8회면 멈추고, 120초 이상 안정적으로 실행한 작업자 뒤에는 실패 횟수를 초기화한다. 응답 지연·준비 시간 초과만으로 작업자를 죽이거나 lease를 반환하지 않는다.
- 중지·업데이트는 `Fence`로 생성만 일시 정지하고, lease 검사와 실제 작업자 종료를 확인한 뒤에만 감독 종료를 확정한다. 거절되면 관문을 놓아 자동 복구가 계속된다. 시작 중인 자식과 감독 없이 실행된 peer도 보호한다.
- `AAM_HOME/service-supervisor.{lock,gate,retire,child,process}`는 현재 사용자 전용 조정 파일이다. PID는 생성 시각과 프로세스 핸들로 검증하며, 실행 파일 복원 전에는 단순 잠금 해제가 아니라 프로세스 종료도 확인한다.
- `runtime_health.rs`는 `logs/service-health.log`에 버전·PID·시작·비정상 이전 실행·종료·재시작·메모리 수치만 기록한다. 파일당 256 KiB에서 한 번 회전한다. 패닉은 소스 위치만 남기며 패닉 메시지·오류 원문·토큰·이메일·프롬프트는 남기지 않는다. WER 미니덤프와 대응 PDB 보존 절차는 [기여 안내](../../CONTRIBUTING.md#windows-서비스-충돌-진단--service-crash-diagnostics)를 따른다.
- Windows commit 여유를 10초마다 관측한다. 여유가 `max(512 MiB, commit 한도 × 2%)` 미만이면 새 모델 요청만 제한하며, 그 기준의 두 배 이상을 3회 연속 관측하면 해제한다. 표본 실패는 회복 연속 횟수를 초기화한다. 표본 부재·마지막 성공 표본 45초 초과·미지원 OS는 `unknown`으로 보고하고 새 요청을 막지 않는다.

## RPC 메서드 (`lib.rs:446-550`)
`status.read`의 Snapshot은 `serviceVersion`(서비스 실행 파일의 `CARGO_PKG_VERSION`)을 담는다. 앱·`aam`이 자기 버전과 비교해 덮어쓴 앱 뒤에 남은 예전 서비스를 찾는 데 쓴다. 이 필드가 없던 서비스는 응답에 빠져 있고, 받는 쪽은 '알 수 없음'(재시작 필요)으로 본다. `service.prepareUninstall`은 `aam service restart`도 쓰는 lease 검사 창구다. 기본(`{}`)은 점유 lease가 하나라도 있으면 `SESSION_BUSY`(아무것도 쓰지 않음), 없으면 새 배정을 막는 permit을 저장한다. `{"restart": true}`는 사용자가 끊김에 동의한 서비스 재시작에서만 쓰며, lease가 있어도 permit(배정 차단)만 저장하고 lease 기록·상태는 건드리지 않는다. 이 매개변수를 모르는 예전 서비스(0.18.3 등)는 무시하고 기본처럼 거절하므로 launcher가 그 경우를 따로 처리한다. 끊길 작업 미리보기는 새 RPC 없이 `status.read`의 `sessions`와 `bridge.status`의 `connections.public.longActive`·`connections.modelQueue.waiting`을 읽어 launcher가 만든다.
`status.read`·`quota.refresh`에 `{"observe": false}`를 주면 omp 프로세스 관측(`discover_sessions`)을 건너뛰고 저장된 상태만 돌려준다(`observedSessions`가 비고 관측 알림도 없다). 관측은 Windows에서 조회 한 번에 수십~수백 ms라, 관측 세션을 쓰지 않는 트레이·회복 감시·팝오버 조회는 이 옵션을 쓴다. 이 매개변수를 모르는 예전 서비스는 무시하고 전체 스냅샷을 돌려준다.
`status.read`, `quota.refresh`, `service.prepareUninstall`, `service.cancelUninstall`, `service.resumeAdmission`, `route.resolve`, `route.explain`, `takeover.adopt`, `takeover.release`, `lease.validate-child`, `policy.update`, `bridge.status`, `diagnostics.export`, `account.register`, `account.remove`, `account.update`, `account.apiKeyAdd`, `apiCredit.record`, `lease.acquire`, `lease.starting`, `lease.started`, `lease.heartbeat`, `lease.release`, `lease.abort`, `resetCredits.read`, `resetCredits.redeem`. 나머지는 `METHOD_NOT_FOUND`.
`bridge.status`는 기존 gateway·세션·차단 목록 외에 `connections{totalLimit,public,native,modelQueue}`, `authRefresh{active,waiting,limit}`, `nativeAuth[]`, `nativeErrors`를 돌려준다. 연결 lane은 `active/limit`, `longActive/longLimit`, `rejected`, `listening`, `port`, 모델 대기열은 대기·만료·본문 예약 수를 담는다. `nativeAuth`의 익명 키·실패 코드·실제 단계·소요 시간·회복 횟수로 연결 혼잡과 공식 CLI 인증 작업 실패를 구분한다. 상세 예산·deadline·로그 규약은 [omp-bridge](omp-bridge.md)를 따른다.
native 인증 요청의 사전검증은 해당 계정 한 행만 읽고, 전체 계정·한도 JSON을 매번 다시 해석하지 않는다. 미확인 인증 상태는 503, 명시적인 로그인 필요는 401, 배정 제외·프로필 연결 변경은 409로 구분하며, 캐시된 자격 증명으로 검증을 건너뛰지 않는다.

## Claude API 크레딧 fallback (0.18.0, `api_keys.rs`, [스펙](../specs/2026-10-30-api-credit-fallback.md))
- `account.apiKeyAdd {label,key,confirmedNoAutoReload}`(`api_key_add`): auto-reload 확인이 없으면 `API_CREDIT_CONFIRM_REQUIRED`, 형식(`api_credit::validate_key_format`)·identity(`apikey:<sha256 앞 12 hex>`) 중복(`ACCOUNT_ALREADY_CONNECTED`)·`/v1/models` 유효성 확인을 거쳐 `tool:"api"` 계정을 만든다. 키는 OS 보안 저장소(macOS Keychain·Windows Credential Manager)에만 저장하고 계정·응답·로그에는 넣지 않는다. 저장소 읽기·쓰기·삭제는 서비스 잠금·DB 트랜잭션 밖에서 실행하며, 계정 저장이 실패하면 방금 넣은 키를 지운다. `account.remove`는 `tool:"api"`도 받아 저장소 키를 지운다.
- `apiCredit.record {accountId,model,usage|null,sessionId?,capability?}`(`api_credit_record`): `usage`가 있으면 `api_credit::cost_usd`로 추정액을 더하고, `null`·단가 없는 모델이면 `unmetered_requests`만 올린다. `store::record_api_credit`는 Immediate 트랜잭션 안에서 읽고 써 동시 덧셈이 사라지지 않게 하며, 저장된 `period_start`가 현재 UTC 달력월이 아니면 0부터 다시 센다.
- `policy.update`는 `useApiCreditAfterLimit`·`apiCreditMonthlyLimitUsd`(유한한 1~100000)를 받는다. 기존 `use_extra_usage_after_limit`와 독립이다.
- API 키 사용액 원장(0.18.1, `store.rs`): 월 사용액의 정본은 계정 본문이 아니라 별도 표 `api_credit_usage(account_id, period_start, used_usd, unmetered_requests, updated_at)`(`PRIMARY KEY(account_id, period_start)`, `CREATE TABLE IF NOT EXISTS`, `user_version` 그대로)다. `record_api_credit`는 이 표에 `ON CONFLICT … DO UPDATE`로 더하고 달이 바뀌면 새 행에서 0부터 센다. `accounts()`·`account()`는 키 계정의 `Account.api_credit`을 원장의 가장 최근 기간 행으로 채우고(소비자는 그대로 `scheduler::api_credit_used`로 현재 달만 읽는다), `save_account`는 `api_credit`을 본문에 쓰지 않는다. `account.remove`는 키 계정의 계정 행만 지우고(`retain_api_credit_ledger`가 원장 행을 확인) 원장은 남기므로, `store::api_credit_removed(now)`(계정 행이 없는 원장 행의 이번 달 합)가 지운 키의 사용액이다. 이 값은 `scheduler::decide_with(…, removed_api_usd)`(`lease.acquire`·`lease.starting` 재검사·`refresh_plan`·`route.explain`), `quota_summary::summaries_with`, 브릿지 `candidates`→`api_key::total_used`가 모두 월 상한 합계에 더한다(꺼 둔 키도 센다). 지운 키는 후보·계정 목록에 나타나지 않고 `Snapshot.apiCreditRemoved`(`ApiCreditState`, 이번 달 합계가 0이면 생략)로만 화면에 전해진다. 시작할 때 `migrate_api_credit_bodies`가 0.18.0이 본문에 남긴 `apiCredit`를 원장으로 한 번 옮기고(같은 계정·기간 행이 이미 있으면 `MAX`로 합쳐 두 번 세지 않는다) 본문에서는 지운다. 지운 키에 늦게 도착한 `apiCredit.record`는 원장 행이 있으면 받는다. 한계: 이 표는 이전 판(0.18.0)이 읽지 않으므로, 0.18.0으로 내리면 지운 키의 사용액은 합계에서 빠진다.
- 키 계정(`tool:"api"`)은 일반 계정이 아니다: 스캔 병합(`apply_scan`)은 건드리지 않고, `account_priority`·`provider_pins`·`projectAllowlist`에는 `reject_api_account`가 `INVALID_PARAMS`로 거절하며, `account.update`의 `maxConcurrency`도 거절한다(등록 때 `max_concurrency: 0` = 개수 제한 없음 고정). 구독 후보 루프(`account.tool == intent.tool`)에도 들어가지 않는다.

## 계정별 초기화권
- `reset_credits.rs`가 `resetCredits.read`·`resetCredits.redeem`을 처리한다. 자동 배정·사용량 갱신은 쿠폰을 소비하지 않는다. Claude·Codex native 프로필과 정확한 identity로 연결된 omp OAuth 계정만 지원하며 다른 공급자는 `unsupported`, 조회 실패는 수량 미확인이다.
- `reset_provider.rs`는 고정된 Anthropic/Codex 원본 API만 호출한다. Claude Cedar/Juniper와 Codex rate-limit reset credit을 정규화한다. HTTP redirect·재시도는 끄고 요청 12초·연결 3초·본문 1 MiB 한도를 둔다. 공식 CLI의 native Source와 omp broker가 기존 규칙대로 토큰을 갱신하며 인증 덮어쓰기 충돌·identity 검사를 우회하지 않는다.
- SQLite FULL metadata의 `resetQuote:`는 2분 유효한 단일 사용 확인, `resetRequest:`는 request UUID별 사용 기록, `resetGate:`는 익명화한 물리 계정별 직렬화 관문이다. fresh 공급자 제안·계정 binding을 다시 검사한 뒤 `submitted`를 디스크에 저장하고 POST를 한 번 보낸다. 토큰·이메일·원문 응답은 이 기록에 저장하지 않는다.
- 서비스 instance UUID가 달라졌을 때 미전송 `preparing`은 `declined`, 전송된 `submitted`는 `unknown`으로만 복구한다. 불명확한 응답·연결 종료는 자동 재전송하지 않으며 다른 alias의 쿠폰도 막는다. 명시적 조회에서 기존 쿠폰의 소진·만료가 확인되면 `resolved`; Cedar 잔여량의 일부 감소만으로는 해제하지 않고 request-id가 없는 Juniper는 이전 주기 종료와 새 주기 확인이 필요하다.
- 확정 성공 뒤에만 기존 강제 한도 갱신을 요청한다. lease·사용 중인 세션·다른 모델 한도를 직접 해제하지 않으며, 쿠폰 사용 후 낡은 잔여량은 미확인으로 표시한다.


## 저장소 (`store.rs:61-116`)
SQLite `state.sqlite3`(`crates/protocol/src/lib.rs:107`), WAL·`synchronous=FULL`(`store.rs:75`), busy_timeout 5초(`store.rs:64`). 테이블 `accounts`, `metadata`, `leases`(`store.rs:80-82`). `user_version` 1보다 큰 값은 `SCHEMA_VERSION_UNSUPPORTED`로 거부(`store.rs:69-71`). 마이그레이션 없이 `user_version=1`(`store.rs:97`) 하나를 쓴다. Windows `cancel_stopped_uninstall`(`lib.rs:42`)의 버전 검사는 1과 다른 값에 `SCHEMA_VERSION_UNSUPPORTED`를 반환(`lib.rs:58`).
- 보조 색인 `leases_account_id`, `leases_capacity_snapshot`, `leases_recent_history`는 `CREATE INDEX IF NOT EXISTS`로 만들어 `user_version`을 올리지 않는다(`store.rs:103-111`).
- `api_credit_usage`(0.18.1): API 키 월 사용액 원장 표. 키 계정을 지워도 행이 남고 `accounts`와 FK 관계가 없다(위 "API 키 사용액 원장" 참고).
- `metadata` 키는 `policy`, `takeovers`, `tools`, `notices`, `lastRefreshAt`, `uninstallPermit`과 `resetQuote:`·`resetRequest:`·`resetGate:` 접두사 기록이다. `accounts.body`·`leases.body`는 Account·Session JSON이고, `leases`는 본문 외에 `account_id`·`request_id`·`client_id`·`payload_hash`·`capability`·`policy_revision`·`binding_hash`·`account_body`·`expires_at`·`heartbeat_at`·`manual` 열을 따로 둔다.
- 계정 새로고침은 DB 잠금 밖에서 병렬 조회하고 `publish_account`로 완료된 계정부터 짧게 저장한다. 진행 중 `refreshing`은 유지하며, 다른 계정을 stale로 바꾸거나 `lastRefreshAt`을 완료로 기록하지 않는다. 마지막 `apply_scan`에서 전체 조회 결과·안내·시각을 반영한다.
- 진행 결과를 저장한 값을 다음 병합의 비교 기준으로 써서 새 사용량을 자기 진행 결과와 충돌시키지 않는다. 조회 중 사용자 재등록·identity 변경은 덮지 않으며, 최신 배정 포함 여부·동시 실행 설정·사용자 라벨을 유지한다.
- `snapshot_sessions`는 SQLite에서 용량 보유 상태 전체와 최근 종료 이력 200개만 고르고 세션 본문만 해석한다. 계정 사본·capability·전체 종료 이력을 읽지 않으며, 스냅샷 가공은 DB 잠금 밖에서 수행한다. 최근 이력·용량 보유 상태에 partial index, lease의 `account_id`에 인덱스를 둔다.
- reconcile은 용량 보유 lease만, 예약 만료 확인은 `PREPARED`만 읽는다. 전체 이력은 보존하고, 재개·멱등성·진단 등 전체 기록이 필요한 경로는 기존 조회를 유지한다. 슬롯 반환 조건과 `synchronous=FULL`은 완화하지 않는다.
- 사용자 설정을 보존한 계정 결과가 현재 행과 같으면 계정 쓰기를 생략한다. native에 명시적으로 병합된 omp 관측을 정리할 때도 최신 행이 조회 기준과 일치하고 배정 포함 상태인지 확인하므로, 조회 도중 제외·동시 실행 수 변경·재등록한 행을 삭제하지 않는다.

## Lease 상태기계
- 용량을 점유하는 상태: `PREPARED`, `STARTING`, `ACTIVE`, `SUSPECT`, `ORPHANED` (`scheduler.rs:11-16`). 종료: `EXITED`, `ABORTED`, `FAILED`.
- `lease.acquire` → PREPARED. `requestId` 기준 멱등, 같은 id·다른 payload는 `IDEMPOTENCY_CONFLICT` (`lib.rs:1021`).
- `lease.starting` → STARTING.
  - generation 불일치 `LEASE_FENCED`(`lib.rs:1256`), 만료 `LEASE_EXPIRED`(`lib.rs:1282`), 정책 변경 `POLICY_CONFLICT`(`lib.rs:1291`), 재입장 검사 실패 `ADMISSION_CHANGED`(`lib.rs:1333`).
  - evidence tier는 `preflight-verified`(`lib.rs:1335`), `identity_key`는 계정의 현재 값과 비교(`lib.rs:1336`).
- `lease.started` → 검증된 자식 프로세스면 ACTIVE, 아니면 SUSPECT.
- reconcile(`lib.rs:1736-1863`):
  - 시간 초과는 슬롯 반환 근거로 쓰지 않음(`lib.rs:1848`). 불확실한 세션은 SUSPECT/ORPHANED로 남긴다(`lib.rs:1823, 1848`).
  - 자손 기록(`backgroundProcesses`)이 없는 세션의 EXITED 반환 근거는 `process::all_from_previous_boot`의 참 결과(`lib.rs:1808`). 같은 부팅 안에서는 null을 빈 목록으로 보지 않는다.
  - macOS 이외 빌드에서 `all_from_previous_boot`는 false를 반환(`process.rs:24-26`).
  - Windows는 `lease.started` 때 연 Job 핸들의 활성 프로세스 수 0을 EXITED 근거로 사용(`Service::job_finished` `lib.rs:1371`, [windows-port](../specs/2026-09-29-windows-port.md)).
- capability는 UUID 2개 연결, 상수 시간 비교, 불일치 `LEASE_FORBIDDEN` (`lib.rs:295-311`).
- 환경변수: `AAM_PREPARED_TIMEOUT_MS`(기본 30000), `AAM_SUSPECT_THRESHOLD_MS`(기본 45000) (`lib.rs:338-339`).

## 배정 (`scheduler.rs`)
- 동시성은 identity 그룹(같은 provider + identity_key 또는 omp credential pin) 단위로 센다 (`scheduler.rs:170-179,243-262`).
- `Account.maxConcurrency`는 기본 1이며 `account.update` 또는 사용 현황 → 배정 설정에서 1~32 또는 0(개수 제한 없음)을 지정한다. 0·2 이상은 `supports_shared_profile_concurrency`가 확인한 Claude 계정만 허용하며 미검증 도구는 `ADAPTER_UNVERIFIED`로 거절한다. 같은 identity의 실행 가능한 바인딩 중 양수 제한이 하나라도 있으면 가장 작은 제한을 유지하고, 모두 0이면 개수로 배정을 막지 않는다. 화면은 표시 그룹과 별개로 정확한 identity/pin별 연결을 나눠 변경하며, 기존 값은 사용자가 선택하기 전까지 유지한다. quota·인증·프로젝트·중복 재개 보호와 lease 추적은 그대로다. 근거: Claude 공식 [여러 세션 병렬 실행](https://code.claude.com/docs/en/best-practices#run-multiple-claude-sessions)·[worktrees](https://code.claude.com/docs/en/worktrees).
- 안전 여유량은 소프트 우선순위다. 하드 제약을 통과한 roomy 후보가 있으면 reserve 후보를 뒤로 보내고(`RESERVE_DEPRIORITIZED`), 없으면 남은 한도를 사용한다(`RESERVE_FALLBACK`). 100% 사용은 계속 `QUOTA_EXHAUSTED`로 제외한다. 명시 계정·프로젝트 pin·재개 제약은 유지하며 공급자 선호 pin보다 roomy 후보가 우선한다.
- 자동 배정 신선도 입력: `stale_after_seconds` 초기값 900초(`crates/protocol/src/lib.rs:347`), 비교는 `scheduler.rs:360-363`.
- 오래된 관측은 `QUOTA_STALE`(`scheduler.rs:389`), 리셋 시각 미확인은 `RESET_UNCONFIRMED`(`scheduler.rs:404`)로 배제 사유가 붙음. pinned 실행은 `manual` 분기라 이 검사 블록을 건너뛴다(`scheduler.rs:382`).
- `lease.acquire`의 `readiness`는 계정 없음·미확인/빈 한도·오래된 한도에 조회를 시작한다. 배정 가능 여부는 기존 `scheduler::decide_with`를 재사용하며, 실제 선택 가능한 계정이 있으면 다른 공급자의 조회를 기다리지 않는다. 필요한 조회만 최대 20초 기다리고 미확인 상태의 배정발 조회는 마지막 시도부터 15초 간격으로 합친다. 마지막 전체 scan 직후 첫 실행도 한도 복구를 시도할 수 있다.
- 새 배정의 대기 판단은 용량 보유 세션 본문만 읽는다. 재개는 기존 `normalize_resume`·라우팅으로 원래 계정을 확정하고 그 계정의 한도 회복을 기다린다. 이미 처리한 요청 ID는 새 조회 없이 기존 멱등 결과를 사용한다. 배정 실패 메시지는 계정 정보 없이 제외 코드만 추가하며, `NO_ELIGIBLE_ACCOUNT`와 실행기의 기존 원본 통과 계약은 유지한다.
- 기본 모델 요청은 모델 전용 버킷이 막지 않고 우선순위만 낮춘다 (`scheduler.rs:27-55,312-358`).
- `quota_summary.rs`가 계정 그룹별 `available/reserve/partial/resting/excluded/login/unknown`을 `Snapshot.quotaSummaries`로 제공한다. 최신 bucket과 실제 bridge 차단을 합치되, 이메일 근거가 여러 workspace에 걸치면 차단을 억지로 붙이지 않는다. 모델별 한도만 있는 공급자도 지원한다. 모델·프로젝트·슬롯이 없는 요약이므로 배정 허용은 별도 scheduler 판단이다.
- bridge 차단의 `quota` 구분을 요약에 유지한다. 오래된 버킷 때문에 소진된 한도의 이름을 특정하지 못하더라도 실제 사용량 제한을 요청 속도 제한으로 바꾸지 않으며, 공급자가 지정한 차단 시각은 그대로 유지한다.
- 곧 리셋(`quota_summary::expiring`): 모델 전용·5시간 같은 짧은 한도를 뺀 신선한 공용 한도가 `expiring_window_hours`(기본 48) 안에 리셋되고, 안전 여유량을 뺀 남은 양이 `expiring_min_percent`(기본 30) 이상이면 `quotaSummaries[].expiring`(라벨·리셋 시각·쓸 수 있는 %·시간당 %)을 채운다. 여러 개면 시간당 써야 할 양이 가장 큰 것 하나. `resting`/`unknown` 요약에는 붙이지 않는다.
- 5시간 창이 없는 계정(0.22.3 조사, 2026-11-03): Anthropic은 Max 5x를 **Max**($100), Max 20x를 **Ultra**($200)로 개명했고(support.claude.com 11647753·17728952, @claudeai 2026-10-30) 인증된 Ultra에는 5시간 한도가 없다(미인증 Ultra만 자동 인증 전까지 임시 5시간 한도, `/usage`에 Current session 막대가 없으면 인증됨). Pro·Max는 5시간+주간을 유지한다. OpenAI는 "Pro plans currently have no five-hour limit"(learn.chatgpt.com/docs/pricing), Plus·Standard Business는 5시간 창이 있다. 그래서 5시간 한도는 계정마다 있다 없다 하고, 한 계정에서도 사라질 수 있다(미인증→인증 Ultra, 플랜 변경). 규칙: 5시간 한도가 **없는 것은 단기 압박 없음**이지 미확인·차단이 아니다. `bridge::primary_used`는 5시간 한도가 없으면 0이라 `HOT_PRIMARY_PERCENT`(85%) 뒤로 밀리지 않고, `scheduler`의 `pressure`·`use_before_reset`의 5시간 조건도 5시간 한도가 있을 때만 적용된다. 한도 목록 자체는 관측 소스가 성공하면 통째로 교체한다(공식 `/usage` `record_usage_success`, `native::codex_usage`, `add_omp_reports`/`refresh_native_from_omp`의 새 관측). 같은 실제 계정을 본 **다른 행**(예전 omp 관측 등)에 남은 5시간 한도는 `scheduler::without_vanished_short_windows`가 거른다: 다른 행에 같은 한도 ID가 없고, 같은 범위(공용/같은 모델 전용)에 5시간 한도가 하나도 없으며, 더 새롭고 신선한(`known`·`exhausted`, 신선 기준 안, 리셋 전) 관측이 있을 때만 사라진 창으로 보고 뺀다. `bridge::account_view`(소진 판정·`block_until`·`choose`)와 `quota_summary::summaries_with`가 같은 함수를 쓴다. 새 관측이 없거나(조회 실패는 `record_usage_failure`가 낡은 값으로만 표시) 새 관측이 5시간 한도를 `unknown` 등으로 보고하면 그대로 남아 낡음 처리된다. 회귀 테스트: `tests/allocation.rs`의 `a_five_hour_window_that_a_newer_observation_no_longer_reports_…`·`five_hour_less_accounts_are_ranked_…`, `bridge.rs`의 `five_hour_window_dropped_by_a_newer_observation_…`·`absent_five_hour_window_is_no_pressure_…`, adapters `verified_ultra_report_without_a_five_hour_window_…`.
- `expiring_boost`(기본 꺼짐)를 켜면 스마트 배정에서만 입장 조건을 통과한 곧 리셋 계정끼리 먼저 비교한다(`EXPIRING_PREFERRED`). 명시 계정·공급자 수동 배정·소비 순서 모드가 먼저이고, 안전 여유량 안쪽 후보는 여전히 뒤로 간다. 세 값은 `policy.update`로 바꾸며 범위는 1~168시간, 1~100%.
- 크레딧·추가 사용량 fallback(둘 다 정책 옵트인, 기본 꺼짐, 단위가 달라 섞지 않는다): `use_credits_after_limit`는 Codex 전용(`Account.credits` = 공식 app-server `account/rateLimits/read`의 `credits{hasCredits,unlimited,balance}`+`ordinaryUsageAllowed`, ChatGPT 크레딧 단위·금액 아님), `use_extra_usage_after_limit`는 Claude 전용(`Account.extra_usage` = omp 사용량 보고의 `anthropic:extra` USD 항목, `used/limit`). 둘 다 `policy.update`로 바꾼다. 꺼져 있으면 배정은 바뀌지 않는다. 켜져 있어도 구독 한도가 남아 입장 가능한 계정이 하나라도 있으면 그 계정이 항상 이긴다. 그런 계정이 하나도 없고(동시 사용 자리만 없는 계정이 있어도 기다린다), 후보가 `QUOTA_EXHAUSTED` 하나만으로 제외됐으며 소진 관측이 신선하고 리셋 시각이 남아 있고(모델 전용 한도가 아닌 공용 한도), 해당 과금 경로가 신선한 관측에서 명시된 계정(크레딧: `has_credits||unlimited`이고 잔액이 0 이하가 아님, `ordinaryUsageAllowed!=true`, `observed_at`이 `stale_after_seconds` 안 / 추가 사용량: `enabled`이고 `limit`이 없거나 `used<limit`)만 마지막 수단으로 뽑는다. 이유 코드 `CREDITS_FALLBACK`/`EXTRA_USAGE_FALLBACK`, `LeaseGrant.credits_fallback`/`extra_usage_fallback`. 선택 상태는 저장하지 않고 매 판정에서 새로 계산하므로 리셋·회복 관측이 들어오면 다음 판정부터 구독 계정이 이긴다. 크레딧 응답에 없거나 오래된 값은 "없음"으로 본다.
- `quotaSummaries`는 매 스냅샷마다 다시 계산한다. 옵트인이 켜져 있고 같은 도구의 구독 한도가 남은 묶음이 없을 때 `resting` 묶음이 `kind: "credits"`/`"extra"`가 되고(`credits.active`/`extraUsage.active`), 옵트인이 꺼져 있으면 `resting`/`available` 그대로 두고 `credits`/`extraUsage` 사실(`active:false`)만 붙여 화면이 중립 안내("크레딧 있음 (꺼짐)")를 보여 준다. 크레딧 `balance`는 숫자로 읽힐 때만 보관하며 화면은 "N개"로 반올림한다. 금액(USD)은 추가 사용량에만 쓴다.
- API 크레딧 fallback(`scheduler::decide_with`의 별도 패스, `API_CREDIT_FALLBACK`/`API_CREDIT_LIMIT`): `use_api_credit_after_limit`이고 `intent.non_interactive`인 `claude`만 대상이다(수동 지정 제외). 구독·크레딧·extra usage 선택이 모두 비었고, 사용자가 제외하지 않은 구독 계정이 모두 신선한 공용 한도 소진 관측으로 막혀 있고 동시 슬롯 대기·열린 후보가 없을 때만 키 계정에서 고른다. 사유는 `ACCOUNT_DISABLED`·`AUTH_REQUIRED`·`MODEL_UNPRICED`(`api_credit::priced`)·`API_CREDIT_LIMIT`(`api_credit_used` ≥ 월 상한). 그중 이번 달 추정액이 가장 작은 키를 고른다. extra usage가 켜져 있고 쓸 수 있으면 그쪽이 먼저다. `lease.acquire`는 `LeaseGrant.api_credit_fallback`(`scheduler::chose_api_credit`)을 채우고 `lease.starting` 재검사는 `non_interactive`를 같이 확인한다.
- `quota_summary.rs`: 키 계정은 구독 묶음과 따로 묶는다(`api_credit_of`: `ApiCreditSummary`, kind `available`/`resting`(상한 도달)/`excluded`). 옵트인이 켜져 있고 구독 Claude 묶음이 모두 소진(`resting`, 속도 제한 아님)이며 구독 여유가 없을 때만 `active=true`·kind `api-credit`이 된다. 구독 묶음에는 API 사실이 섞이지 않는다. `claude_left` 계산(extra usage 판정)에서도 키 계정은 뺀다.
- 한계(미검증): 이 Mac의 어떤 계정도 `has_credits=true`나 Claude extra usage 켜짐을 관측한 적이 없다. 공식 JSON 스키마(`codex app-server generate-json-schema`의 `CreditsSnapshot`)와 omp `claude.ts` 소스의 필드 형태로 만든 fixture로만 시험했다. Ojak은 자신이 라우팅하는 새 세션·요청만 통제한다. 이미 실행 중인 공식 Codex·Claude Code 세션이 구독 한도 뒤에 과금 경로로 넘어가는 것은 공급자가 정하므로 Ojak이 멈출 수 없다. 쓴 금액도 Ojak은 볼 수 없다.

## 소비 균형 배정과 직접 확인한 API 크레딧 허용 기간 (0.20.0, `AllocationMode::ExpiryBalanced` = `"expiry-balanced"`)
- 목적: 곧 사라질 허용량(구독 주간 한도, 사용자가 직접 확인한 API 크레딧 기간)을 **새 대화에서만** 먼저 쓴다. 진행 중 대화(sticky)·공급자 수동 배정·소진/안전 여유량/인증/동시 슬롯/프로젝트 가드·`CAPACITY_RESERVED` 대기는 `smart`와 같다. 기존 `smart`·`priority` 판정은 바뀌지 않는다(`allocationMode` 기본 `smart`, 새 필드는 모두 기본값이 기존 동작).
- 구독 순서(`scheduler::decide_with`): 요청에 걸리는 긴 주기(5시간이 아닌) 한도의 `(안전 여유량을 뺀 남은 %/100) ÷ (리셋까지 시간/168h)`(pace, 단위 없음)가 큰 계정이 새 대화를 받는다. 이 비율은 플랜과 무관하게 비교 가능해 플랜별 공정 순서를 건너뛴다. `account_priority`와 `expiring_boost` 스위치는 이 모드에서 쓰지 않는다. 후보 사유 `EXPIRY_BALANCED:`.
- 리셋 전 사용(0.22.0, `scheduler::use_before_reset`, `expiry-balanced` 한정): 안전 여유량은 다음 리셋까지 대화가 중간에 끊기지 않게 남겨 두는 완충이다. 그러나 요청에 걸리는 가장 빡빡한(사용률이 가장 높은) 긴 주기 한도가 `expiring_window_hours`(기본 48시간, `quotaSummaries.expiring`과 같은 정책 값 재사용·새 필드 없음) 안에 리셋되면 남겨 둔 완충은 리셋과 함께 사라지므로, 안전 여유량 안쪽이어도 새 대화를 여유 있는 계정과 같은 풀에서 받고 **리셋이 가장 이른 계정이 점수와 무관하게 먼저**다(그다음 기존 pace 순서). 조건: 안전 여유량>0, 계정이 여유량 안쪽(`in_reserve`)이고 모델 전용 한도가 `ModelOnly`가 아님, 적용 한도 중 `exhausted`/100% 없음, 5시간 한도가 여유량 안쪽이거나 `SHORT_WINDOW_HOT_PERCENT`(85%) 이상이 아님, 가장 빡빡한 긴 주기 한도의 `resets_at`이 지금 이후이고 창 이내(경계 포함). 이미 입장 조건(`QUOTA_STALE`·`RESET_UNCONFIRMED`·인증·동시 슬롯·프로젝트·소진)을 통과한 계정에만 적용하므로 소진·차단·장애·제외·낡은 관측 계정은 절대 고르지 않고, 유료 fallback(크레딧·extra usage·API 키)도 이 규칙으로 열리지 않는다. 명시 계정·공급자 수동 배정·프로젝트 규칙은 그대로 먼저다. 사유 코드 `USE_BEFORE_RESET:`(해당 계정의 `RESERVE_DEPRIORITIZED`/`RESERVE_FALLBACK`을 대체). `smart`·`priority`와 `expiring_boost`는 그대로 안전 여유량을 지킨다. 한계: 새 대화의 입장 순서만 바꾸므로 진행 중 대화(sticky)는 옮기지 않고, 리셋 전에 새 작업이 시작되지 않으면 남은 양을 끝까지 쓴다고 보장할 수 없다. 남은 양이 한 번의 입장에서 얼마나 소비될지는 알 수 없다.
- 허용 기간 입력(`Policy.apiCreditGrant` = `{id,budgetUsd,startsAt,expiresAt,confirmedAt,source:"manual"}`): Console 잔액을 **조회한 값이 아니다**. `policy.update.apiCreditGrant`는 `{budgetUsd(0.01~1e6),startsAt,expiresAt,confirmed:true,id?}`이며 `null`은 삭제, 필드 생략은 변경 없음이다. `confirmed:true`가 아니면 `API_CREDIT_GRANT_CONFIRM_REQUIRED`. 만료가 지났거나 `expiresAt<=startsAt`·기간 366일 초과·비유한 금액은 `INVALID_PARAMS`. `id`를 생략하면 새 기간(`new_id`)이고 사용액이 0부터 시작하며, 현재 id와 같으면 같은 기간의 수정으로 사용액을 이어 간다(다른 id는 `INVALID_PARAMS`). `confirmedAt`은 서비스가 찍는다. `budgetUsd`는 저장 시점에 사용자가 확인한 남은 금액이고, Ojak은 저장 뒤 자신이 기록한 추정액만 뺀다. UTC 달력월 근사를 쓰지 않고 입력한 기간(`startsAt`~`expiresAt`)을 그대로 쓴다.
- 결제 기반 추정 만료일(0.21.0): 월 결제 구독 계정의 다음 결제일을 수동 입력해 허용 기간 만료일을 **추정**할 수 있다. 결제 API·Console 조회는 없고 입력 전부 사용자가 직접 한 값이다. `Policy.apiCreditBilling = {accountId,cycle:"monthly"|"annual",nextBillingAt(ms),updatedAt}`(기본 `null`)이며 `policy.update.apiCreditBilling = {accountId,cycle,nextBillingAt}|null`(생략=변경 없음). `accountId`는 있는 Anthropic 구독 계정(`provider=="anthropic" && tool=="claude"`, `scheduler::is_billing_account`)이어야 하며 API 키·codex·omp 행은 `INVALID_PARAMS`, 없는 id는 `ACCOUNT_NOT_FOUND`. `nextBillingAt`은 지금 이후여야 하고 월 35일·연 367일 이내(`INVALID_PARAMS`). 결제 정보를 저장·삭제·수정해도 저장된 허용 기간의 만료일·예산·사용액·id는 **바뀌지 않는다**(자동 연장 없음). 연결한 계정을 삭제하면(`forget_account`) 결제 정보만 지운다. 서비스는 지난 날짜를 달 단위로 굴리지 않고 시간대·하루 끝을 추정하지 않는다: `nextBillingAt`은 클라이언트가 보낸 순간이다.
- 만료 근거(`ApiCreditGrant.expirySource` `exact`(기본·이전 저장분)|`billing-estimate`, `billingEstimate{accountId,nextBillingAt}` 근거 기록): `policy.update.apiCreditGrant.expirySource:"billing-estimate"`는 저장된 결제 정보가 월 결제이고 계정이 유효하며 날짜가 미래일 때만(아니면 `API_CREDIT_ESTIMATE_UNAVAILABLE`, 연 결제는 항상), `estimateConfirmed:true`가 있어야 하고(`API_CREDIT_ESTIMATE_CONFIRM_REQUIRED`), `expiresAt`이 `nextBillingAt`과 정확히 같아야 한다(`INVALID_PARAMS`). 기존 `confirmed:true`(현재 잔액 확인)는 추정에도 그대로 필요하다. 같은 id의 추정 기간을 `exact`로 날짜 변경 없이 다시 저장하면 `API_CREDIT_EXPIRY_SOURCE_REQUIRED`라서 추정이 조용히 확정으로 바뀌지 않는다(날짜를 직접 확인해 바꾸면 `exact`). id·사용액·`api_credit_grant_usage` 의미는 그대로다. `Snapshot.apiCreditGrant`에 `expirySource`·`billingEstimate`·`estimateDrift`(추정 근거가 지금 결제 정보와 다름; 허용 기간은 그대로)·`billing{accountId,cycle,nextBillingAt,estimatedExpiresAt,estimateBlockedBy}`가 추가된다. `estimatedExpiresAt`은 월 결제+미래 날짜일 때만 있고 아니면 `estimateBlockedBy`=`ANNUAL|DATE_PASSED|ACCOUNT_INVALID`. 연 결제 갱신일은 월 크레딧 만료일로 쓰지 않는다. 추정 만료일의 허용 기간도 만료 판정·선제 사용 조건은 `exact`와 같다.
- 합산 풀: 모든 API 키가 같은 Console 조직 잔액을 나눠 쓴다고 보고 키별 예산을 두지 않는다. 사용액은 별도 표 `api_credit_grant_usage(grant_id PK, used_usd, unmetered_requests, updated_at)`(`CREATE TABLE IF NOT EXISTS`, `user_version` 그대로)에 `record_api_credit`과 **같은 트랜잭션**으로 더한다. 키 계정과 무관해 키를 지워도 남고, 요청이 끝난 시각이 `[startsAt,expiresAt)` 밖이면 더하지 않는다(월 합계 원장 `api_credit_usage`에는 계속 들어간다). `policy.update`가 현재가 아닌 기간의 행을 지운다. `store::api_ledger(policy, now)` → `scheduler::ApiLedger{removed_usd,grant_used_usd,grant_unmetered}`를 `decide_with`·`summaries_with`·브릿지 `PaidPolicy`가 받는다(예전의 `removed_api_usd: f64` 인자를 대체).
- 상태(`scheduler::GrantView`, 저장하지 않고 `now`로 매번 계산): `upcoming`(시작 전)·`active`·`exhausted`(추정 잔액 0)·`expired`(`now>=expiresAt`가 가장 먼저)·`unknown`(입력 없음). `Snapshot.apiCreditGrant`는 항상 채워지며 `remainingUsd`·`perDayUsd`(남은 금액÷남은 일수, active만)·`unmetered`·`monthlyLimitReached`·`proactive{allowed,blockedBy}`를 담는다. `blockedBy`는 `MODE`→`OPT_IN_OFF`→`GRANT_NONE|PENDING|EXPIRED|EXHAUSTED|UNMETERED`→`NO_KEY`→`MONTHLY_LIMIT`→`AHEAD_OF_PACE` 순으로 첫 사유 하나다.
- 선제 사용(구독이 남아도 새 대화를 API로): `allocationMode==expiry-balanced` **그리고** `balanceApiCredit` **그리고** `useApiCreditAfterLimit` **그리고** 허용 기간 `active` **그리고** 사용량을 읽지 못한 요청(`unmetered_requests`)이 0 **그리고** 키 사용 가능 **그리고** 월 상한 미만. 하나라도 빠지면 선제 사용은 없다. 미확인이 한 번이라도 생기면 그 기간의 선제 사용은 닫힌다(새 기간 입력 전까지). 대화형·`claude -p`(스케줄러 경로)는 선제 사용을 하지 않는다 — 선제 사용은 omp 브릿지 새 세션에만 있다. 정확한 단가가 없는 모델은 쓰지 않는다.
- `claude -p`를 선제 사용에서 뺀 구체적 이유: 실행기는 키로 나간 `claude -p`의 stdout을 가로채지 않으므로 사용량을 읽지 못하고 `apiCredit.record`를 `usage:null`로만 보낸다(그래서 미확인 요청으로 세어진다). 스케줄러가 구독이 남아 있는데 `-p`를 키로 보내면 첫 요청이 곧바로 `unmetered_requests`를 올려 그 기간의 선제 사용이 닫히고 예산 차감도 0이 되어, 허용 기간 예산을 지킬 근거가 없다. 응답 스트림을 읽어 금액을 확정하는 경로는 omp 브릿지(`UsageScanner`)뿐이라 선제 사용은 거기에만 연다. 대화형 Claude Code는 어떤 경우에도 API로 가지 않는다(`non_interactive`가 아니면 키 후보가 열리지 않는다).
- 기존 한도 뒤 API fallback도 허용 기간이 **있으면** 그 기간이 `active`일 때만 열린다(`scheduler::grant_allows_fallback`; 사유 `API_GRANT_PENDING|EXPIRED|EXHAUSTED`, `quotaSummaries`의 `api-credit` 활성도 같은 조건). 허용 기간이 없으면 0.18.x 동작 그대로다. 월 상한(`apiCreditMonthlyLimitUsd`)은 허용 기간과 별개로 항상 같이 적용한다.
- 한계: 사용액은 Ojak이 기록한 추정치(스트림 usage×공개 단가)이고 Console 청구·다른 클라이언트의 같은 조직 사용·auto-reload 설정을 볼 수 없다. 동시 요청은 입장 때 예약하지 않고 응답이 끝난 뒤 기록하므로 하드 상한이 아니다(브릿지는 진행 중 API 대화 수만큼 `PROACTIVE_MIN_REMAINING_USD`($5)를 보수적으로 남겨 두는 휴리스틱을 쓴다). 실제 청구 차단은 Console의 auto-reload 끄기와 구매하지 않은 크레딧뿐이다.

## 라우팅 (`routes.rs:191-461`)
우선순위: takeover → resume/parent(원래 계정 고정, 다르면 `SWITCH_UNSUPPORTED`) → 가장 깊은 폴더 규칙 → 저장소 규칙(git) → 명시 계정/전역. 경로가 바뀌면 `ROUTE_CHANGED`.

### 다른 계정에서 이어 가기 (Claude Code·Codex)
- `LaunchIntent.continue_elsewhere` + `resume_session_id`(관리 세션). `routes::resolve`가 원래 세션 계정을 `RouteResolution.excluded_account_id`로 넘기고 모델은 원래 대화 모델, 프로젝트 규칙은 적용하지 않는다. `scheduler`는 그 계정과 같은 실제 계정을 `CONTINUE_SOURCE`로 뺀다. 새 세션은 같은 native ID를 쓰고 `Session.continued_from`에 원래 세션을 남긴다.
- `routes::original`은 같은 native 대화의 관리 세션을 시간순으로 보고 마지막 세션의 계정으로 재개한다. 계정이 바뀐 곳은 반드시 바로 앞 세션에서 이어 간 기록(`continued_from`)이어야 하고, 아니면 `RESUME_UNVERIFIED`.
- 대화 기록 복사(`adapters::copy_conversation`: Claude `projects/<폴더>/<native>.jsonl`, Codex `sessions/<년>/<월>/<일>/rollout-*-<native>.jsonl`, 원자적 교체·사용자 전용)는 실행기가 preflight 안에서 한다. 로그인·설정은 옮기지 않는다.
- Codex는 시작 때 대화 ID를 정할 수 없다. 실행기가 실행 중 자기 프로세스 트리(spawn 때 확인한 birth identity의 루트와 관측한 자손, 매 샘플에서 birth가 같은 프로세스만)가 쓰기 모드로 연 `.jsonl`을 200ms마다 모은다(`launcher::WriterWatch`: macOS libproc, Windows 프로세스 handle 조회 + named Job 자손). 종료 뒤 그중 계정 프로필 `sessions/` 안 rollout이면서 작업 폴더가 같은 대화가 정확히 하나일 때만 ID로 쓴다(`adapters::codex_session_from`). 시각만 맞는 외부 Codex의 rollout은 근거가 아니다. 찾은 ID는 `lease.release.nativeSessionId`로 보고하고, 서비스는 다른 세션이 쓰지 않는 ID만 기록한다. 재개는 `codex resume <id>`, 비대화형은 `codex exec resume <id>`.
- 실행기는 대화형 Claude·Codex의 공식 구조화 기록에서 한도 소진을 감지하면 기존 실행을 종료한 뒤, 서비스가 `lease.release`에 `EXITED`와 같은 native ID를 응답했을 때만 다른 계정으로 자동 재개한다. 사용자의 중단이나 정상 종료 뒤에는 자동으로 되살리지 않는다. 방문 계정 배제·최대 8회 제한은 실행기가 관리하며, 배정·인증·lease 규칙은 서비스의 기존 경로를 따른다. 수동 `aam continue [--tool] [--session] [--account] [-- <인수>]`도 유지한다.

### 공급자 수동 배정 (`Policy.provider_pins`)
- 키는 공급자(`aam_protocol::pin_provider`: claude·anthropic→`anthropic`, codex·openai-codex→`openai`, …), 값은 계정 ID. 없으면 그 공급자는 자동 배정.
- 고정이 아니라 우선순위다. `scheduler::decide_with`는 입장 조건을 통과한 후보 중 고정 계정(같은 실제 계정, `same_identity`)을 먼저 고르고, 통과하지 못하면 자동 선택으로 넘어가며 `Decision.pin_unavailable`·`LeaseGrant.pin_unavailable`로 알린다. 실행기는 터미널에 안내를 출력한다. 자동 배정을 꺼 두면 고정 계정만 쓰고 넘기지 않는다.
- omp 브릿지는 진행 중 대화(sticky)를 유지하고, 새 대화만 고정 계정(`Candidate.pinned`)을 먼저 준다. 진행 중 대화는 프롬프트 캐시를 지키려고 안전 여유량 안쪽이어도 같은 계정에 두고, 소진·차단·장애로 후보에서 빠질 때만 옮긴다. 새 대화는 고정 계정이 소진·차단·여유량 안쪽이면 다른 계정으로 간다.
- 이전 판의 도구별 `preferredAccounts`는 읽을 때 공급자 키로 옮긴다(`store::policy`).

## 진단 (`diagnostics.rs:48-150`)
`redact:false`는 거부. 허용 필드만 다시 조립하고 계정·세션은 임시 별칭으로 바꾼다.

## 테스트
`crates/service/src/tests.rs` + `src/tests/{allocation,api_credit,credits,expiry_balanced,routing,takeover}.rs`. 다수가 macOS 전용. `api_credit.rs`는 스케줄러 API 패스·quota_summary·`lease.acquire` grant를, `expiry_balanced.rs`는 허용 기간 입력 검증·합산 원장·상태·fallback 게이트·소비 균형 구독 순서를, 브릿지 `choose`·종단 `stream()` 시험은 `bridge.rs`의 `mod tests`에 있다(소비 균형 입장·sticky·pin 포함). 실제 Keychain 왕복(`adapters`의 `keychain_roundtrip_store_overwrite_delete`)은 `--ignored`로만 실행한다.

## Screen Flow / Lifecycle
<!-- screen-flows-v3: 2026-09-27, type=web-backend -->

| Stage | 상태 | 근거 |
|---|---|---|
| Account.Create | ✅ | RPC account.register lib.rs:514 → Service::register lib.rs:829-944 (label validation 830-838, SESSION_BUSY guard 862-871, supersede/policy rewrite 873-940, save |
| Account.Read | ✅ | status.read lib.rs:449-450 → snapshot lib.rs:353-445 (store.rs accounts(), stale-bucket marking in snapshot_with_observations lib.rs:386-401). Callers: apps/desktop |
| Account.Update | ✅ | account.update → update_account (enabled; maxConcurrency 0=제한 없음 또는 1..32; 공유 프로필의 0·2 이상은 검증된 Claude만 허용) |
| Account.Delete | ✅ | `account.remove {id}` → `remove_account`(lib.rs). claude·codex·API 키(`tool:"api"`) 행만(omp는 `INVALID_PARAMS`), 용량을 쥔 세션이 있으면 `SESSION_BUSY`, 없는 id는 `ACCOUNT_NOT_FOUND`. 한 Immediate 트랜잭션에서 행 삭제 + `forget_account` + 그 계정에 고정된 `projectRoutes` 삭제(응답 `removedRoutes`) + 정책 revision 증가(바뀐 때만) + 그 계정의 인계 등록·`resetQuote:<id>` 캐시 삭제 + 프로필 `{tool,path}`를 metadata `dismissedProfiles`에 추가. lease 이력은 남긴다(`leases.account_id` 외래키는 `store.rs drop_lease_account_reference`가 한 번 제거). 스캔은 `dismissedProfiles`의 기본·관리 프로필을 후보에서 빼고(`adapters::discovery_candidates`), 같은 경로의 `account.register` 성공이 그 기록을 푼다. API 키 계정은 커밋 뒤 OS 보안 저장소의 키를 지운다. CLI `aam account remove --account <ID>`(키는 `aam account api-key remove <ID>`). register supersede의 내부 DELETE는 별도. |
| Policy/ProjectRoute.Create | ✅ | Singleton policy seeded with Policy::default at store.rs:112-114. Routes are created through policy.update lib.rs:511 → update_policy lib.rs:593-720 → routes::v |
| Policy/ProjectRoute.Read | ✅ | Snapshot.policy (lib.rs:366-445, store.rs policy()). route.resolve/route.explain lib.rs:473-507 → routes::resolve routes.rs:191 and scheduler::decide_with sched |
| Policy/ProjectRoute.Update | ✅ | update_policy lib.rs:593-720: CAS on expected_revision (POLICY_CONFLICT lib.rs:600-605), per-field validation, revision++ and set_metadata lib.rs:717. UI: apps/ |
| Policy/ProjectRoute.Delete | ✅ | Route removal is policy.update with a filtered list: apps/desktop/src/AllocationSettings.tsx:210 (routes.filter → save). Policy itself is a non-deletable singleton. Dan |
| Lease/Session.Create | ✅ | lease.acquire lib.rs:516-539 (stale-quota refresh wait) → acquire lib.rs:1002-1210: idempotent by request_id (IDEMPOTENCY_CONFLICT lib.rs:1021), admission_allowed |
| Lease/Session.Read | ✅ | status.read sessions: every capacity-holding lease plus the latest 200 terminal ones (snapshot_sessions(…, 200) lib.rs:378). lease.validate-child lib.rs:510 → managed_ses |
| Lease/Session.Update | ✅ | lease.starting lib.rs:540 → starting lib.rs:1219-1358 (authorize lib.rs:295-311, generation fence, expiry→ABORTED lib.rs:1275-1276, POLICY_CONFLICT lib.rs:1291, |
| Lease/Session.Delete | ✅ | lease.release/lease.abort lib.rs:543-544 → finish lib.rs:1479-1577: PREPARED→ABORTED lib.rs:1530-1531, spawn-failed→FAILED lib.rs:1538, foreground-confirmed→EXI |
| Takeover.Create | ✅ | takeover.adopt lib.rs:508 → adopt_takeover lib.rs:722-777 (checked() requires claude plus UUID lib.rs:158-180, aam_adapters::session_owner proof, rejects already |
| Takeover.Read | ✅ | Snapshot.takeovers, filtered to not-yet-managed conversations, lib.rs:417-428 and 441. CLI aam takeover list crates/launcher/src/main.rs:614-632. Desktop declar |
| Takeover.Update | ❌ | dispatch has no takeover.update RPC (lib.rs:446-550). Re-adopting drops the same tool+native record (filter lib.rs:761-768) and pushes a new one (lib.rs:773), s |
| Takeover.Delete | ✅ | takeover.release lib.rs:509 → release_takeover lib.rs:778-802 (TAKEOVER_NOT_FOUND when nothing was removed, set_takeovers lib.rs:799). Records that are already  |

```mermaid
stateDiagram-v2
  [*] --> PREPARED: lease.acquire (lib.rs:1200)
  PREPARED --> STARTING: lease.starting (lib.rs:1346)
  PREPARED --> ABORTED: expiry / release / abort / restart reconcile (lib.rs:318, 1276, 1531, 1795-1800)
  STARTING --> ACTIVE: lease.started verified (lib.rs:1453)
  STARTING --> SUSPECT: started unverified / heartbeat stale / release unconfirmed
  STARTING --> FAILED: lease.abort spawn-failed (lib.rs:1538)
  SUSPECT --> FAILED: lease.abort spawn-failed, no process
  SUSPECT --> ACTIVE: lease.started / reconcile alive (lib.rs:1846)
  ORPHANED --> ACTIVE: lease.started / reconcile alive (lib.rs:1846)
  ACTIVE --> SUSPECT: child dead + supervisor alive / heartbeat stale (lib.rs:1833, 1847)
  ACTIVE --> ORPHANED: supervisor dead / background unverified (lib.rs:1545, 1823, 1837)
  SUSPECT --> ORPHANED: child dead + supervisor dead (lib.rs:1835)
  ORPHANED --> SUSPECT: release unconfirmed, process not alive (lib.rs:1547)
  ACTIVE --> EXITED: release fg-confirmed / background finished (lib.rs:1541, 1821)
  SUSPECT --> EXITED: release fg-confirmed / background finished
  ORPHANED --> EXITED: release fg-confirmed / background finished / previous-boot identities (lib.rs:1818)
  ABORTED --> [*]
  FAILED --> [*]
  EXITED --> [*]
  note right of ORPHANED: holds capacity (scheduler.rs:11-16), timeout alone is not a release reason (lib.rs:1848)
```

| Entity | Create | Read | Update | Delete |
|---|---|---|---|---|
| Account | ✅ account.register, account.apiKeyAdd(API 키) | ✅ status.read | ✅ account.update (Claude maxConcurrency exposed in AllocationSettings; API 키는 거절) | ✅ account.remove (claude·codex·API 키) |
| Policy/ProjectRoute | ✅ seed + policy.update | ✅ status.read, route.resolve/explain | ✅ policy.update (CAS) | ✅ policy.update filtered list |
| Lease/Session | ✅ lease.acquire | ✅ status.read, validate-child, diagnostics | ✅ starting/started/heartbeat + reconcile | ✅ release/abort (soft, rows kept) |
| Takeover | ✅ takeover.adopt (CLI) | ✅ status.read, aam takeover list | ❌ none (adopt = upsert) | ✅ takeover.release (CLI) |

### 이슈
- [high→partly fixed: released after reboot] Lease/Session.Delete: A stuck lease has no way out. Reconcile leaves ORPHANED unchanged when child and supervisor are both dead and no background list exists (lib.rs:1835-1836). A STARTING/SUSPECT lease with no registered process and a dead s
- [resolved 0.17.0] Account.Create: `leases.account_id REFERENCES accounts(id)` 외래키는 제거됐다(`store.rs` `drop_lease_account_reference`). 이력이 있는 계정도 supersede·`account.remove`로 지울 수 있고 lease 행은 보존된다.
- [medium] Lease/Session.Delete: 종료된 lease 행은 보존 정책 없이 쌓인다(`DELETE FROM leases` 문장은 store.rs에 등장하지 않는다). 스냅샷은 최근 종료 200개를 읽고(lib.rs:378), expire()는 `PREPARED`를(lib.rs:315-325), reconcile은 용량 보유 lease를(lib.rs:1739) 읽는다. 그 밖의 경로는 아직 `leases()`로 전체 이력을 읽는다(lib.rs:478, 558, 729, 862, 957, 983, 1037, 1310, 1562).
- [low] Policy/ProjectRoute.Update: projectAllowlist and autoTakeover can be set via policy.update (lib.rs:593-720). The desktop declares the fields as types (apps/desktop/src/types.ts:79,83) without a setter UI, and no CLI command writes them.
- [low] Takeover.Create: Takeover adopt/list/release is CLI-only (crates/launcher/src/main.rs:614-648). The desktop app neither allowlists takeover.* nor renders snapshot.takeovers.

### 다음 할 일
- [x] Account: supersede DELETE가 이력 있는 옛 binding에서 실패하던 원인(lease 외래키)은 0.17.0에서 제거됐다. lease 행은 `account_body` 사본과 함께 남는다.
- [ ] Lease: add retention that prunes terminal leases past N rows or an age limit. (용량 상태 필터와 색인은 적용됨: expire/reconcile은 전체 이력을 스캔하지 않는다.)
- [x] Account: `account.remove` RPC가 추가됐고 연결 탭·`aam account remove`에서 쓴다(0.17.0).
- [ ] UI: expose projectAllowlist and autoTakeover, or document them as CLI/raw-RPC only; add takeover list/release to the desktop if intended.
