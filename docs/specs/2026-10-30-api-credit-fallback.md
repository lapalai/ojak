# Claude API 크레딧 fallback

상태: macOS 구현·로컬 반영 완료(0.18.x). 0.19.0에서 Windows Credential Manager를 구현하고 로그인된 Windows 사용자 컨텍스트에서 실제 저장·읽기·덮어쓰기·삭제를 검증했다. Mac 등록 키로 Anthropic 소액 직접 호출 2회도 성공했다. 실제 구독 소진→API 자동 전환·사용액 기록의 실계정 종단 검증과 Windows 실제 API 키 호출은 미검증이다. 설치·배포 상태는 CHANGELOG를 따른다. 사용자 승인: 방향 A(구독 소진 뒤 동의한 경우에만 API 크레딧 사용), Ojak만 수정하고 claude CLI·codex CLI·omp·client-forks는 바꾸지 않음(2026-10-30).

## 목표

Claude 구독 계정의 한도를 모두 쓴 뒤, 사용자가 동의했다면 Max 플랜에 포함된 월 API 크레딧(Max 20x는 $200)으로 이어서 쓴다. 이 크레딧은 Claude Console 조직의 API 키로만 쓸 수 있다.

## 근거 (확인된 사실)

- 월 API 크레딧은 Console 조직 잔액이다. Claude API·Agent SDK·API 키로 실행한 `claude -p`에는 적용되고, 대화형 Claude Code와 extra usage에는 적용되지 않는다. 크레딧이 소진됐을 때 조직에 따로 산 크레딧이나 auto-reload가 있으면 거기서 계속 과금되고, 없으면 요청이 멈춘다. ([Monthly API credits for Max and Team plans](https://support.claude.com/en/articles/17154008))
- `claude -p`는 `ANTHROPIC_API_KEY`가 있으면 항상 그 키를 쓴다. 대화형 모드는 키를 처음 쓸 때 사용자 승인을 묻는다. ([env-vars](https://code.claude.com/docs/en/env-vars))
- omp `18.12.0-ojak.2`의 `auth-gateway`에 broker snapshot으로 `credential:{type:"api_key", key}`를 넘기면 upstream `/v1/messages`로 `x-api-key` 헤더를 보낸다. `oauth-2025-04-20`·Claude Code beta 헤더는 붙이지 않고 `cache_control`은 그대로 유지한다(2026-10-30 무과금 fixture: 실제 설치 바이너리 + 가짜 broker·upstream). 진짜 Anthropic 엔드포인트에서는 아직 검증하지 않았다.
- 사용량·비용 Admin API(`cost_report`·`usage_report`)는 Admin 키가 필요하고 개인 계정에서는 쓸 수 없다. 크레딧 잔액을 조회하는 공개 API는 찾지 못했다.
- 범위에서 제외: omp `openai-codex`(ChatGPT backend 전용이라 Platform 키를 받지 않음), 대화형 `claude`(크레딧 미적용), Codex API 키(Max 크레딧과 무관하고 정가로 과금됨).

## 안전 불변식

- 사용자 환경·설정에 있는 API 키·base URL·auth helper는 지금처럼 `AUTH_OVERRIDE_CONFLICT`로 막는다. 이번 변경은 Ojak이 Keychain에 직접 저장한 키를 Ojak이 직접 넣는 경로만 추가한다.
- API 키 경로(omp gateway spawn, 내부 broker의 키 제공, `claude -p` 실행 준비)는 서비스·부모 환경에 Claude 인증·엔드포인트 재정의(`ANTHROPIC_*`의 키·토큰·base URL·헤더, `CLAUDE_CODE_USE_*`, `NODE_OPTIONS`·`DYLD_INSERT_LIBRARIES` 등)가 있으면 지우고 진행하지 않고 `AUTH_OVERRIDE_CONFLICT`로 멈춘다. 중앙 검사는 `aam_adapters::check_api_credit_env`(`safety::reject_auth_env`의 claude 규칙 재사용, cmux `NODE_OPTIONS` 예외 없음)다.
- 기본값은 꺼짐이다. 구독 후보가 하나도 없고, 그 이유가 신선한 관측으로 확인된 한도 소진일 때만 쓴다. 일시 차단·gateway 장애·관측 부족은 fallback 사유가 아니다(기존 `paid_cover` 규칙과 같다).
- 키 값은 로그·오류·진단 내보내기·snapshot·UI에 노출하지 않는다. 키를 가리킬 때는 sha256 앞 12 hex(`account_key`)만 쓴다.
- 과금 차단의 정본은 Console 설정이다(auto-reload 끄기, 크레딧 추가 구매 안 하기). Ojak 상한은 보조 장치이며, 이 상한이 실제 청구액을 보증한다고 표시하지 않는다.
- lease와 불확실 실행 규칙은 바꾸지 않는다. 전송 뒤 실패를 다른 계정이나 키로 재시도하지 않는다.

## 구현 계약

### 1. 키 등록 (`aam account api-key add|remove`, 데스크톱 연결 화면)
- 저장: macOS Keychain generic password(service `ai.aam.api-key`, account = 내부 ID). Windows Credential Manager generic credential(target `ai.aam.api-key:<내부 ID>`, `CRED_PERSIST_LOCAL_MACHINE`). 파일에 평문으로 저장하지 않는다.
- 등록 검증: `GET https://api.anthropic.com/v1/models`(과금 없음)로 키 유효성만 확인한다. 실패는 `API_KEY_INVALID`로 처리하고 원문 응답은 저장하지 않는다.
- `Account`: `tool:"api"`, `provider:"anthropic"`, `auth_kind:"api_key"`, `identity_key:"apikey:<sha256 앞 12>"`. 이메일과 org는 없다. 구독 계정 목록과 한도 요약에는 섞지 않고 별도 그룹으로 보인다.
- 등록 시 "이 키의 조직에서 auto-reload를 끄고 크레딧을 따로 사지 않았는지" 확인 체크를 받는다. 체크하지 않으면 정책 스위치를 켤 수 없다.

### 2. 정책
- `Policy.use_api_credit_after_limit: bool`(기본 false), `Policy.api_credit_monthly_limit_usd: Option<f64>`(기본 200). `policy.update`·protocol·`types.ts`를 함께 갱신한다. 기존 `use_extra_usage_after_limit`과는 독립이다. 둘 다 켜져 있으면 extra usage를 먼저 쓴다(구독 계정 쪽 경로이기 때문).

### 3. omp 브릿지
- `bridge_native.rs`: 지금은 `entry()`가 `type:"oauth"`로 고정돼 있다. 이걸 `NativeSource` 대신 쓸 `ApiKeySource`로 분리하고, snapshot에는 `{"type":"api_key","key":…}`를 낸다. refresh·disable·block 경로는 키가 바뀌지 않으므로 no-op 응답을 주고, 401이 오면 키를 `auth-required`로 표시한다.
- `bridge.rs::choose()`: API 키 후보는 `Candidate.api_credit: bool`로 구독 후보와 **명시적으로 분리**한다. `buckets`가 비어 있는 후보는 지금 코드에서 정상 eligible이 되므로(`bridge.rs:339-353`), 기존 `paid_fallback` 분기에 그대로 끼우면 유료 키가 일반 후보가 된다. 다음 순서를 지킨다.
  1. 일반 eligible과 "모두 소진" 판정(`:359-361`)은 `api_credit=false` 후보만 대상으로 평가한다.
  2. 기존 Codex 크레딧·Claude extra usage fallback(구독 계정 쪽)도 같은 구독 풀에서만 고른다.
  3. 위 둘이 모두 비었고 구독 풀이 신선한 관측으로 전부 소진됐을 때만, 별도 API 풀(`api_credit=true`, healthy, enabled, 미차단, 미시도, 월 상한 미도달, 단가를 아는 모델)에서 고른다.
  4. sticky 세션이 API 키에 고정돼 있어도 1·2에서 구독 후보가 생기면 그쪽을 쓴다(고정은 eligible 안에서만 유지되는 기존 규칙을 따른다).
  모델 전용 한도 소진만으로는 API 풀로 넘어가지 않는다(`paid_cover`와 같은 기준).
- 구독 한도가 회복되면 새 요청부터 구독 계정으로 돌아간다. 이미 진행 중인 요청은 옮기지 않는다.
- 로그: 기존 usage 줄 끝에 ` api-credit` 표식을 붙인다. `apps/desktop/src-tauri/src/main.rs` 파서도 함께 고친다.

### 4. `claude -p`
- launcher: `arguments::is_interactive`가 false이고 scheduler가 API 키 fallback을 고른 경우에만, Ojak이 만든 child env에 `ANTHROPIC_API_KEY`를 넣는다. 사용자 env 검사(`safety.rs`)는 child env를 만들기 전에 그대로 돈다.
- native identity probe는 구독 전용(`apiKeySource=none`)이라 이 경로에서는 건너뛰고, 대신 등록된 키의 지문으로 검증 단계를 대체한다. 대화형 실행은 이 fallback을 쓰지 않고 기존처럼 `NO_ELIGIBLE_ACCOUNT` → 원본 통과 규칙을 따른다.
- `LeaseGrant.api_credit_fallback`을 추가하고, 실행 안내에 "API 크레딧 사용"을 표시한다.

### 5. 사용액 추정과 상한
- omp: 브릿지가 응답 스트림을 그대로 넘기면서 마지막 `done` 이벤트의 `usage`(input·output·cacheRead·cacheWrite)를 읽어 모델별 공식 단가로 USD를 누적한다. 프롬프트는 저장하지 않는다.
- `claude -p`: `--output-format json`이 아닐 수 있으므로 정확한 사용량을 얻지 못한 실행은 추정치를 "미확인"으로 표시한다.
- 월 누적액(청구 주기는 UTC 달력월로 근사하고, 화면에 근사임을 표시)이 상한에 닿으면 새 요청을 `API_CREDIT_LIMIT`으로 거절한다. 진행 중인 요청은 끊지 않는다.
- 단가표는 `crates/service/src/pricing.rs` 한 곳에 둔다. 모르는 모델은 fallback 대상에서 제외한다(추정 불가 = 사용 안 함).

### 6. 화면
- 사용 현황 → 크레딧·추가 사용량 설정에 "Claude API 크레딧" 항목을 추가한다. 스위치, 월 상한, 이번 달 추정 사용액, "Console 설정이 실제 차단 장치" 안내를 둔다.
- 활성 상태의 요약 kind는 `api-credit`로, "API 크레딧 사용 중 ($12.40 / $200, 추정)"처럼 보인다. 3개 언어 문구를 모두 넣는다.

## 검증

| ID | 시나리오 | 기대 |
|---|---|---|
| T1 | 구독 남음(여유량 안쪽 계정 포함) + 스위치 켬 + buckets가 빈 API 키 후보 존재 | 구독 계정 선택, API 키는 일반 후보로 절대 선택되지 않음 |
| T2 | 모든 구독이 신선한 관측으로 소진 + 스위치 켬 | omp 요청이 `x-api-key`로 나감(fixture upstream에서 헤더 캡처), 로그에 `api-credit` |
| T3 | 소진이지만 관측 stale / gateway 장애 / 일시 차단 / 모델 전용 한도만 소진 / 구독 계정 일부만 소진 | API 풀로 넘어가지 않음 |
| T3b | 구독 소진 + extra usage도 켜져 있고 사용 가능 | extra usage 먼저, API 키는 그다음 |
| T4 | 스위치 꺼짐 | 기존 동작과 동일 |
| T5 | 사용자 env에 `ANTHROPIC_API_KEY` | 여전히 `AUTH_OVERRIDE_CONFLICT` |
| T6 | 누적액이 상한에 도달 | 새 요청 `API_CREDIT_LIMIT`, 진행 중 요청 유지 |
| T7 | 대화형 `claude` + 구독 소진 | API 키를 넣지 않음 |
| T8 | `claude -p` + 구독 소진 + 스위치 켬 | child env에만 키가 있음, 부모·로그에 값 없음 |
| T9 | 키 401 | 키 계정 `auth-required`, 다른 요청에 영향 없음 |
| T10 | 실제 Anthropic 소액 호출 1회(사용자 승인 후) | 200 응답, Console Usage에 반영, 구독 한도 불변 |
| T11 | 로그·진단 내보내기 grep | 키 문자열 0건 |

## 구현 기록 (0.18.0)

- **계획과 다른 점**
  - 단가표는 `crates/adapters/src/api_credit.rs` 한 곳에 뒀다. launcher도 같이 쓰기 때문이다.
  - 정책 상한은 `Option`이 아니라 `f64`(기본 200)다.
  - 키 계정은 `auth_kind` 필드가 아니라 `tool: "api"`로 구분한다.
  - 0.18.0 구현 시점에는 Windows Credential Manager가 미구현이었다. 0.19.0에서 구현하고 Windows 실기기 저장소 왕복을 검증했다.
- **RPC**
  - `account.apiKeyAdd {label,key,confirmedNoAutoReload}`
  - `apiCredit.record {accountId,model,usage|null,sessionId?,capability?}`
  - `policy.update`의 `useApiCreditAfterLimit`·`apiCreditMonthlyLimitUsd`(1~100000)
  - `account.remove`는 OS 보안 저장소의 키도 지운다.
- **CLI**
  - `pbpaste | aam account api-key add --label <이름> --confirm-no-auto-reload`
  - `aam account api-key remove <ID>`
- **omp 브릿지**
  - 키 계정마다 `("anthropic","apikey:<12hex>")` gateway를 띄운다.
  - 내부 broker `/native/<accountId>`가 `{"type":"api_key","key":…}` snapshot을 낸다. pool 파일에는 anthropic 식별자를 넣지 않는다.
  - 응답은 그대로 넘기면서 종료 이벤트(`done`/`error`)의 `usage`만 읽는다. 버퍼는 최대 256KiB이고, HTTP chunked를 직접 해석한다.
  - 응답 처리:
    - 키 거절은 계정을 `auth-required`로 바꾼다.
    - `credit balance` 오류는 다음 달 1일(UTC)까지 그 키를 막는다.
    - 어느 경우든 소진된 구독으로 되돌아가지 않는다.
- **`claude -p`**
  - `-p`/`--print`만 비대화형으로 본다(`arguments::non_interactive`).
  - 자식 환경은 `PATH`와 `ANTHROPIC_API_KEY`만 갖는다. `CLAUDE_CONFIG_DIR`는 넣지 않는다.
  - 실행이 끝나면 `apiCredit.record`를 `usage: null`로 보낸다. 그래서 금액에 넣지 못한 요청으로 센다(stdout을 가로채지 않음).
  - 대화형 실행에 키 grant가 오면 lease를 abort하고 실행하지 않는다.

## 검증 기록 (2026-10-30, Mac)

| ID | 결과 | 근거 |
|---|---|---|
| T1 | 통과 | `bridge.rs` choose 테스트(여유량 안쪽 구독 포함), scheduler `src/tests/api_credit.rs` |
| T2 | 통과(fixture) | 브릿지 `stream()` 종단 테스트(`exhausted_subscription_routes_stream_to_api_key_only_when_enabled_and_meters_usage`)가 키 gateway 선택, ` api-credit` 로그, $6.40 집계를 확인했다. 설치된 omp 18.12.0-ojak.2 `auth-gateway`에 `api_key` snapshot을 넣은 fixture에서 upstream `x-api-key` 전송과 실제 `done` 이벤트 usage 형식(`cost` 중첩 포함)을 확인했다. |
| T3·T3b·T4 | 통과 | bridge·scheduler 단위 테스트 |
| T5 | 통과 | `api_credit_env_stops_on_every_endpoint_or_auth_override_and_hides_values`, `api_key_path_refuses_auth_override_env_without_serving_or_spawning`(키 제공 409·gateway 미기동·값 미노출) |
| T6·T7·T8·T9 | 통과 | 서비스·launcher·bridge 단위 테스트 |
| T10 | 미실시 | 실제 키·사용자 승인 필요 |
| T11 | 통과(부분) | 브릿지 로그·응답·CLI 오류에 키 문자열 없음을 테스트·격리 실행으로 확인했다. 진단 내보내기는 계정에 키가 없다는 구조로만 확인했다. |
| Keychain | 통과 | 실제 로그인 키체인에서 고유 임시 ID로 저장→읽기→덮어쓰기→삭제→없음 확인(`keychain_roundtrip_store_overwrite_delete`, `--ignored`로만 실행). 남은 항목 0. 등록·삭제의 Keychain 호출은 서비스 잠금·DB 트랜잭션 밖에서 실행한다. |
| 화면 | 통과 | 실제 React 화면(IPC만 모의, RPC는 격리 `aam-service`로 전달)에서 다음을 확인했다. 연결 탭의 키 목록·`$12.40 (추정)`, 키 추가 대화상자의 체크 전 '추가' 비활성, 실제 Anthropic 검증의 `API_KEY_INVALID` 표시, 입력란 비움, 크레딧 설정 스위치·상한 150 → `policy.update` 저장, 상단 'API 크레딧 사용 중 · $12.40 / $150 (추정)'. |

## 여러 키 (0.18.0 추가)

- 키는 여러 개 등록할 수 있다. 같은 키(같은 `apikey:` 지문)는 다시 등록할 수 없다.
- **월 상한은 모든 키의 합계에 건다.** 같은 Console 조직의 키는 크레딧 잔액 하나를 함께 쓰므로, 키를 늘려도 크레딧이 늘지 않는다. 배정 검사(`scheduler`, `bridge_api_key::usable`)와 요약(`quota_summary::api_credit_of`), 화면(`apiCreditOverview`)이 모두 같은 합계를 쓴다.
- 상한 안에서는 이번 달에 덜 쓴 키부터 쓴다. 이 기준은 omp 브릿지(`Candidate.api_used`)와 `claude -p`가 같다. 진행 중인 omp 대화는 고정된 키를 그대로 쓴다.
- 연결 탭에서 키마다 켜고 끌 수 있고(`account.update {enabled}`), 이름을 바꾸거나(`account.update {label}`, API 키 계정만, 1~120자) 지울 수 있다. 키 값은 다시 보여 주지 않는다.
- 모델 단가는 정확한 모델 ID로만 찾는다. 허용하는 변형은 `anthropic/` 접두와 공식 날짜 접미(`-YYYYMMDD`)뿐이고, 비슷한 미등록 버전은 모르는 모델로 본다.

## 남은 일
- Windows 실제 API 키 등록·모델 응답과 구독 소진 후 자동 전환의 실계정 종단 검증(실제 Credential Manager 왕복은 통과).
- `claude -p` 사용량 집계(현재 금액에 넣지 못한 요청으로만 셈).
- 크레딧 청구 주기 시작일은 API로 알 수 없어 UTC 달력월로 근사한다.
