# AAM 계정 브릿지 — omp·Claude Code·Codex 계정 선택 단일화

작성: 2026-09-26 · 상태: omp 브릿지 구현·설치 완료, Claude Code·Codex 자동 배정은 사용자 설정 대기 · 관련: `2026-09-21-omp-auth-broker-hub.md`

> **2026-09-27 제거 기록.** 이 문서가 "사용하지 않는다"고 적은 관리 omp(`aam-omp` 18.2.6·hard-control)와 Grok·agy CLI 어댑터의 실행 경로는 이 날짜에 코드에서 삭제했다(관리 omp는 broker 설정과 공존 불가, Grok·agy는 gate로 실행 불가). 3.1의 "이전 방식 `models.yml` 관리 블록 되돌리기"(`legacyOverride`·receipt)도 이 기기에서 더 이상 해당 사항이 없어 제거했다. 이제 omp·Grok·Antigravity 계정은 이 브릿지로만 배정하고, AAM이 직접 실행하는 CLI는 Claude Code·Codex다.

## 1. 목표

Orca와 터미널에서 실행하는 omp·Claude Code·Codex의 계정 선택을 **AAM 한 곳에서** 결정한다.
omp·Orca·native CLI가 각자 계정을 고르는 경로는 제거하거나 AAM 경로로 수렴시킨다.

- 모델별 한도 범위를 반영한다. 모델 전용 한도(예: Fable)는 그 모델만 막고, 공용 5시간·7일 한도는 계정 전체를 막는다.
- 세션 단위로 계정을 고정한다(프롬프트 캐시 보존). 한도 소진 시에만 다음 계정으로 옮긴다.
- 같은 계정에 동시 세션이 몰리지 않도록 AAM이 전체 세션을 보고 분산한다.

## 2. 현재 상태 (2026-09-26 실측)

| 도구 | 실행 경로 | 계정 선택 주체 | 문제 |
|---|---|---|---|
| omp (Orca·터미널) | 원본 `~/.local/bin/omp`, broker(`127.0.0.1:8765`)에서 계정 8개 공급 | omp 자체 랭킹 | AAM 정책 미적용. 동시 세션이 같은 계정에 몰릴 수 있음 |
| Claude Code | AAM shim | AAM(자동 배정 꺼짐 → 선호 계정 project-b) | Orca가 키체인 active credential을 교체해 어긋날 수 있음 |
| Codex | AAM shim | AAM(자동 배정 꺼짐) | Orca가 pane별 `CODEX_HOME`을 지정(`codex-pane-accounts.json`) |

관리 omp(`aam-omp` 18.2.6)는 broker 설정과 구조적으로 공존할 수 없어(`integrations/omp/hard-control/bootstrap.ts:46-48`) 이번 설계에서 사용하지 않는다.

## 3. 설계

### 3.1 omp: `/login`의 AAM 공급자

```
omp /login → "Ojak · Claude" ─▶ 브릿지 토큰을 ojak-claude OAuth로 broker에 저장
omp /model → ojak-claude/<원래 모델> ──pi-native──▶ AAM bridge 127.0.0.1:4020 ──▶ 계정 고정 gateway[계정 k] ──▶ 공급사
                                                       └─ 선택기: 모델·한도·동시 세션으로 계정 k 선택, options.sessionId에 고정
```

- **omp 확장** `integrations/omp/aam-accounts.js`(`aam omp-bridge connect`가 `~/.omp/agent/extensions/aam-accounts`에 설치)가 `/login`에 Ojak 공급자 다섯 개를 등록한다: `ojak-claude`(anthropic), `ojak-codex`(openai-codex), `ojak-antigravity`(google-antigravity), `ojak-grok`(xai-oauth), `ojak-zai`(zai).
- 로그인: 브릿지 토큰과 해당 공급자의 실행 중 계정 수(`GET /v1/providers`)를 확인한 뒤 토큰을 OAuth 형식(`access`=`refresh`=토큰, 만료 10년)으로 돌려준다. broker 모드에서는 omp가 이를 broker에 올린다.
- 모델: `modifyModels`가 원래 공급자의 모델을 복사해 공급자만 `ojak-*`로, 전송을 `transport: pi-native`·`baseUrl: 브릿지`로 바꾼다. omp가 복사본을 `buildModel`로 다시 만들어 thinking 단계·비용·컨텍스트가 원래 모델과 같다. omp는 `models`가 하나 이상 있는 공급자에만 `modifyModels`를 걸기 때문에 로그인 전 자리표시 모델 하나를 등록한다.
- 원래 공급자(`anthropic/…` 등)는 바꾸지 않는다. 사용자가 `enabledModels`로 공급자를 제한하고 있으면 연결 시 `ojak-*/*` 패턴을 더하고 해제 시 뺀다(omp `config set` 사용).
- **2026-09-27 공급자 ID 변경(`aam-*` → `ojak-*`)**: 연결할 때 broker에 `aam-*` 로그인이 있고 `ojak-*`가 없으면 같은 브릿지 토큰으로 `ojak-*` 로그인을 올린다(`POST /v1/credential`). `enabledModels`의 `aam-*/*`와 `modelRoles`의 `aam-*/<모델>` 참조는 `ojak-*`로 바꾼다. `aam-*` 로그인은 지우지 않으며, 브릿지는 이미 그 이름으로 모델을 불러 둔 실행 중인 omp 세션의 요청을 계속 받는다.
- 내장 공급자 이름에 로그인 항목을 붙이는 방식은 쓸 수 없다. 2026-09-26 실험에서 `/login`에 이름은 보이지만 선택하면 원래 공급자의 OAuth가 실행됐다.
- **계정별 gateway**: 서비스가 broker snapshot의 위 다섯 원래 공급자 OAuth 계정마다 `omp auth-gateway serve`를 띄운다. 각 프로세스는 `OMP_AUTH_BROKER_ACCOUNT_POOL_FILE`로 해당 계정 하나만 보고(다른 공급자와 `ojak-*`는 `[]`), `PI_CODING_AGENT_DIR=<AAM_HOME>/bridge/agent`로 사용자 설정을 읽지 않는다. broker 주소·토큰은 환경변수로 준다.
- **bridge**: `POST /v1/pi/stream`에서 공급자를 원래 이름으로 바꾸고(`ojak-claude` → `anthropic`), `modelId`를 `<원래 공급자>/<모델>`로 고쳐 gateway가 자기 목록에서 찾게 한다. 계정을 고른 뒤 응답 첫 이벤트가 한도 소진(429, usage limit)이면 계정을 막고 다음 후보로 같은 요청을 다시 보낸다. 스트림이 시작된 뒤에는 바꾸지 않는다.
- 세션 키: `options.sessionId`. 세션 고정은 마지막 사용 후 60분까지 유지한다.
- 이전 방식(내장 `anthropic`·`openai-codex`를 `models.yml`에서 브릿지로 덮어쓰기)의 관리 블록이 남아 있으면 연결·해제 때 원래대로 되돌린다.

### 3.2 Claude Code·Codex: AAM shim 단일화

- Orca·터미널 모두 PATH의 AAM shim을 거친다(Orca는 bare command를 로그인 셸에서 실행).
- shim은 선택한 계정의 프로필(`CLAUDE_CONFIG_DIR`/`CODEX_HOME`)을 직접 지정하고, 실행 직전 identity를 확인해 다르면 차단한다(`crates/launcher/src/lib.rs` IDENTITY_MISMATCH). Orca가 키체인이나 `CODEX_HOME`을 바꿔도 AAM이 고르지 않은 계정으로 실행되지 않는다.
- 자동 배정을 켜고, 새 세션 시작 시 scheduler가 계정을 고른다. 실행 중 세션의 계정은 바꾸지 않는다(native CLI 제약).
- Orca 자체 계정 관리(Claude 키체인 교체, Codex pane 계정)는 쓰지 않는다. 기본 프로필(`~/.claude`) 계정은 Orca 키체인 교체의 영향을 받아 실행이 차단될 수 있으므로 격리 프로필로 옮기는 것을 권장한다. AAM은 `orca-data.json`에 직접 쓰지 않는다.

### 3.3 scheduler 정책

- 후보 필터: 요청 모델에 적용되는 한도(공용 + 해당 모델 전용)가 소진된 계정 제외, 동시 세션 상한 초과 제외.
- 순위: 리셋 전 소진이 급한 계정 우선(omp `claudeRankingStrategy`와 같은 기준), 5시간 한도 85% 이상 후순위.
- 같은 identity를 여러 도구가 공유하므로 사용량·동시 세션은 identity 단위로 합산한다(도구별 binding을 별도 계정으로 세지 않음).

## 4. 구현 단계

| Phase | 내용 | 상태 |
|---|---|---|
| 0 | 검증: 계정 풀 적용, 세션 키, pi-native 경로 | 완료 (4.1) |
| 1 | 서비스에 계정별 gateway 감독과 bridge(`crates/service/src/bridge.rs`) | 완료 |
| 2 | 계정 선택(세션 고정, 한도 소진 시 다른 계정으로 재전송, 동시 세션 분산) | 완료 |
| 3 | `aam omp-bridge connect|disconnect|status`(`crates/launcher/src/omp_bridge.rs`), 원본 `models.yml` 기록·복원 | 완료 |
| 4 | 앱 연결 화면의 "OMP 계정 브릿지" 패널 | 완료. Claude Code·Codex 자동 배정은 앱 배정 정책에서 사용자가 켠다 |

### 4.1 Phase 0 결과 (2026-09-26)

- 계정 풀 적용: `OMP_AUTH_BROKER_ACCOUNT_POOL_FILE`에 `anthropic: [bob identityKey]`, `openai-codex: []`를 준 `omp auth-gateway serve`는 `/v1/usage`에 Anthropic bob만 보고하고 `/v1/models`에서 Codex 모델을 모두 제외했다. 같은 gateway로 보낸 `/v1/messages`(`anthropic/claude-opus-5-5`)는 정상 응답했다. 보이는 Anthropic 계정이 하나뿐이므로 해당 계정이 처리했다(응답 헤더에는 계정 식별자가 없음).
- omp 공급자 등록: 격리 `PI_CODING_AGENT_DIR`에서 `aam` 공급자(`discovery.type: proxy`)와 native 공급자 비활성화로 `aam/anthropic/claude-opus-5-5` 요청 성공. gateway를 내리면 모델 목록을 얻지 못해 요청이 나가지 않음을 확인했다.
- identityKey 형식: `email:<email>|org:<orgId>` (broker `/v1/snapshot`).
- 세션 키: pi-native 본문 `options.sessionId`(세션마다 고유), `options.cwd`도 함께 온다.

### 4.2 구현 후 실측 (2026-09-26, omp 18.3.2)

- 연결: gateway 5개(Anthropic 3, Codex 2)가 서비스 자식 프로세스로 기동. `models.yml` 변경은 관리 블록뿐이며 해제 시 원본과 바이트 단위로 동일하게 복원(`cmp` 확인).
- 터미널 omp: Opus·Fable 요청이 브릿지가 고른 계정으로 응답. Orca pane에서 띄운 omp도 같은 경로로 응답(`OK-ORCA-PANE`).
- Codex: 관측 사용량상 두 계정 모두 소진이라 upstream에 보내지 않고 `429` + `Retry-After`로 응답. omp가 즉시 종료(7초, 기존 503일 때는 재시도 10회로 79초).
- 서비스 재시작: 이전 gateway는 함께 종료되고 새 서비스가 같은 포트로 다시 띄움(고아 프로세스 없음).
- 동시 세션: 선택과 세션 기록을 같은 잠금에서 처리해, 동시에 들어온 새 세션도 서로 다른 계정으로 분산한다.
- Orca에서 인자 없이 띄운 omp TUI(기본 모델 Fable, advisor 켜짐)도 브릿지로 응답(`OK-ORCA-TUI`).
- 보조 요청 형태: judge 요청은 `options.model` 없이 `modelId: "<공급자>/<모델>"`만 싣는다. 대화와 같은 `options.sessionId`를 쓰기도 하고(재시도 판정) 자기 sessionId를 쓰기도 한다(시작 시 판정). 처음 구현은 이를 공급자 미지정으로 거부했으나 `request_target`에서 `modelId`로 해석하도록 수정했다. 어느 경우든 judge 요청은 기존 세션 고정을 따르기만 하고 새 세션 기록·활성 세션 집계에 넣지 않는다.
- advisor: `modelRoles.advisor`로 도는 omp advisor는 자기 `sessionId`와 도구를 가진 대화 요청이다. 독립 세션으로 배정한다.
- project-b 사용량 0%: broker `/v1/usage`와 `omp usage` 모두 0%이고 7일 리셋 시각이 약 7일 뒤로 새로 잡혀 있어 실제 리셋으로 판단했다(AAM 병합 문제 아님).
- 운영 로그: 요청마다 `AAM_HOME/logs/bridge.log`에 공급자·모델·세션 끝 8자리·대화 여부·도구 수·배정 계정을 한 줄로 남긴다(프롬프트·토큰 없음, 4MB에서 교체).

## 5. 위험

- 공급사 약관: 본인 구독 계정의 OAuth를 로컬 프록시로 사용한다. 로컬 전용 bind(127.0.0.1)와 AAM 발급 키로 제한한다.
- omp 업데이트로 gateway·계정 풀 동작이 바뀔 수 있다. Phase 0 검증을 회귀 확인 절차로 유지한다.
- gateway 경로에서 omp 전용 기능(Codex websocket, 리셋 크레딧 사용)은 쓰이지 않는다. Codex 리셋 크레딧은 필요 시 계정별로 직접 사용한다.
- Claude Code·Codex는 세션 도중 계정 전환이 불가하다. 한도 소진 시 새 세션이 필요하다.
