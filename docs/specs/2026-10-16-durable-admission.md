# 작업 보존과 자동 대기·재개

상태: 구현 및 Windows 격리 실행 검증 완료. 실제 설치본 교체·Mac 검증·공개 배포는 하지 않았다. 사용자 승인: 설계 및 구현, Ojak·omp·Orca 본체를 함께 수정하는 범위(2026-10-16).

## 목표

일시적인 로컬 메모리 압박 때문에 모델 요청을 시작하지 못한 경우 사용자 입력을 버리거나 재시도 예산 소진으로 종료하지 않는다. 작업을 보존하고 취소 가능한 대기를 표시하며 여유가 생기면 같은 작업을 자동 실행한다. 이미 전송된 요청의 결과가 불확실하면 자동 재전송하지 않는다.

## 확인한 경계

- 현재 모델 요청은 pi-native이며 그 transport는 custom API를 우회한다. 설치된 omp 18.11.0의 실제 격리 실행에서 extension-owned API의 streamSimple과 @oh-my-pi/pi-ai의 streamSimple 재사용, options.fetch 주입이 동작했다.
- 새 세션은 첫 assistant 메시지 전까지 파일이 없을 수 있다. appendEntry만으로 최초 사용자 입력의 내구성을 주장하지 않는다.
- Ojak 확장은 단일 index.js로 설치된다. 지원 API를 사용하며 omp 실행 파일이나 Orca app.asar를 임의 패치하지 않는다.
- before_subagent_spawn은 30초 handler 예산이 있어 무기한 spawn 대기 구현으로 사용할 수 없다. Orca/omp 생성 단계의 영구 작업 큐는 별도 실행기 계약이 필요하다. 모델 호출 대기와 동일하다고 표현하지 않는다.

## 안전 불변식

- 기존 bridge.token 인증, 공급자 배정, AUTH_OVERRIDE_CONFLICT, lease 불확실 상태 유지 정책을 보존한다.
- 실행 의도는 전송 전에 기록한다. 전송 시작 후 단절은 unknown이며 timeout만으로 재실행 가능 상태로 바꾸지 않는다.
- 명시적으로 upstream 이전임이 증명된 혼잡 응답만 자동 대기로 돌린다. 일반 503·ECONNRESET·SSE 실패는 자동 대기 재전송으로 바꾸지 않는다.
- 대기열은 건수와 데이터 크기에 상한을 두고 프롬프트·인증 정보는 진단 로그에 기록하지 않는다.
- 취소는 terminal이며 회복 이벤트·늦은 응답·세션 재시작이 취소된 작업을 되살리지 않는다.
- 자동 복구 대상은 보존된 동일 작업이다. 사용자 메시지나 도구 호출을 복제하지 않는다.

## 구현 계약

- **작업의 주인:** omp 세션 파일과 Orca의 기존 입력 journal/draft가 사용자 작업을 보존한다. HTTP 연결을 작업 저장소로 쓰지 않는다. Ojak 서비스에는 프롬프트를 담는 별도 작업 DB를 만들지 않는다.
- **전송 전 저장:** 확장은 `ojak.admission.v1` custom entry에 작업 ID·공급자·모델·상태·시각만 기록한다. `ctx.persistSession()`이 첫 사용자 입력과 현재 branch까지 저장한 다음에만 대기 검사/POST를 진행한다. 저장 실패는 전송 실패로 닫는다.
- **상태:** `queued → submitted → completed`. 서비스가 명시적으로 미전송을 보증한 혼잡 응답만 `submitted → queued`로 돌아간다. 전송 결과가 불확실하면 `unknown`, 사용자 취소는 `cancelled`, 대기 상한 도달은 `paused`, 영구 설정 실패는 `stopped`, 명시적인 인증·할당량 HTTP 거절은 `rejected`다.
- **재개:** 세션을 열었을 때 최신 branch의 `queued`만 자동 재개한다. `paused`는 `/ojak-resume`으로 같은 입력을 이어갈 수 있다. `submitted`·`unknown`·`cancelled`는 자동 재실행하지 않는다. 완료된 도구 결과를 다시 실행하거나 사용자 메시지를 복사하지 않는다.
- **상한:** omp 프로세스 내 동시 대기 32건, 대기 context의 보수적인 JSON 데이터 예산 합계 256MiB, 한 번의 대기 15분. 이 값은 전체 시스템 heap 상한이 아니다. 데이터 예산을 계산할 때 요청 전체를 미리 직렬화하지 않는다. 상한에 닿으면 작업은 저장한 채 대기를 일시 정지한다.
- **readiness:** 기존 `bridge.token` 인증으로 `GET /v1/admission`을 호출한다. 응답은 `{version:1, ready, reason, poll_after_ms:2000, resources}`다. reason은 `ready`, `resource_pressure`, `resource_unknown`, `model_capacity` 중 하나다. 기존 resources의 camelCase 필드를 사용한다. 지원 플랫폼의 표본이 불명확하면 새 클라이언트는 기다린다.
- **힌트와 허가 구분:** readiness는 예약이 아니다. 실제 요청은 기존 서비스의 유한 FIFO·모델 permit·본문 예산을 반드시 통과한다. 혼잡 응답 후에는 다시 readiness가 true여도 짧은 지연을 둬 POST 폭주를 막는다. 폴링에는 취소와 짧은 timeout을 적용한다.
- **미전송 증거:** 서비스가 upstream 전 단계에서 거절할 때만 HTTP 503의 `error.type=admission_wait`, `error.submitted=false`, `BRIDGE_BUSY`·`BRIDGE_QUEUE_TIMEOUT`·`BRIDGE_RESOURCE_PRESSURE` 중 하나를 보낸다. 이 경우만 같은 POST를 다시 보낼 수 있다. GET 실패는 모델 전송이 아니므로 서비스 복구를 기다릴 수 있다.
- **불확실 실행:** gateway 연결 자체가 실패해 아무 요청도 보내지 않은 경우에는 기존 계정 선택을 계속할 수 있다. 요청 쓰기/응답 읽기 뒤 실패는 `BRIDGE_EXECUTION_UNKNOWN`으로 닫고 다른 계정으로 넘기지 않는다. omp도 이 상태에서 retry·fallback·자동 continuation을 실행하지 않는다.
- **생성 전 대기:** omp의 `before_subagent_admission`은 기존 scheduler permit 안에서 무거운 child/session/worktree 생성보다 먼저 실행한다. 기존 30초 `before_subagent_spawn` 라우팅 hook을 대기열로 전용하지 않는다. Orca는 Ojak 관리 대상인 로컬 omp만 생성 전에 대기시키고 비대상 실행은 바꾸지 않는다.
- **수정본 관리:** omp `v18.11.0` (`c2cb79e4c227aa4b22c4ccb933ff72fd88bceba4`)과 Orca `v1.4.224` (`2bb20586ebc39d605e16d95f57420e20a16e7485`) 소스에 변경을 적용한다. 설치된 바이너리/app.asar를 직접 편집하지 않는다. 공식 업데이트가 수정본을 대체하면 API 호환성을 다시 검증해야 한다.
- **Orca 실행 상한:** terminal·Pi RPC·omp ACP가 공유하는 host 대기열은 32건/4MiB다. 생성은 한 번에 하나, 최소 2초 간격으로 해제하며 매번 새 readiness를 확인한다. 원문은 기존 SQLite journal/draft에 두고 실행 의도는 같은 host DB에 FULL 동기화로 저장한다. 임의 shell 문자열이나 환경 snapshot을 복구 실행하지 않는다.
- **Orca 복구:** 실제 저장된 workspace/tab의 소유권을 검증해 원래 입력을 재개한다. 닫힌 pane은 자동 복원하지 않으며 사용자가 유효한 로컬 workspace에서 재개할 수 있다. structured 대화는 원래 workspace만 허용한다. `submitted`는 기존 process/lease 증명이 해결하기 전까지 자동 재실행·삭제하지 않는다.
- **재현 가능한 수정본:** `integrations/client-forks/manifest.json`에 upstream commit·tag·수정본 버전·patch SHA-256을 고정한다. `node scripts/prepare-client-forks.mjs <빈-대상-폴더>`로 두 checkout을 재현한다. 기존 checkout·설치본은 덮어쓰지 않는다. Ojak launcher는 지정된 omp 수정본 버전을 확인한 뒤에만 bridge 연결/소유 확장 갱신을 진행한다.


## 검증

격리된 loopback fixture와 실제 수정본 실행 파일/렌더러로 검증했다. 외부 모델 공급자에 부하를 보내거나 사용자 호스트의 메모리를 고갈시키지 않았다.

| 검증 | 결과 |
|---|---|
| Ojak `cargo test` (default members, Tauri 제외) | 415 통과 |
| Ojak omp 확장 Node 테스트 | 25 통과 |
| omp admission·session persistence·retry·subagent 관련 9개 파일 | 319 통과 |
| Orca queue·recovery·ACP/Pi·pane·unsent-input 관련 12개 파일 | 204 통과 |
| 타입 검사 | Ojak desktop, omp coding-agent, Orca host/renderer 통과 |
| 빌드 | Ojak CLI/service 0.12.0, omp 18.12.0-ojak.1 실행 파일, Orca main/preload/renderer 통과 |
| 수정본 재현 | 공식 pinned tag를 새 checkout으로 받아 SHA-256 확인 및 두 patch 적용 성공 |

- 서비스의 실제 socket 테스트는 128개 보유 스트림 중 제어 경로 응답, 압박·회복, 구형 요청 거절, 응답 불명 때 다른 계정 재전송 금지를 포함한다.
- 컴파일한 omp에서 압박 대기·명시적 미전송 거절 후 재대기·취소·서비스 장애/복구·프로세스 재시작·전송 후 단절·영구 설정 실패·인증 거절을 실행했다. 각 세션의 원래 사용자 메시지는 하나였다. 취소는 POST 0회, 결과 불명은 POST 1회 뒤 `unknown`으로 끝났고 자동 재전송하지 않았다.
- 실제 격리 Orca에서 terminal 생성 전 대기와 취소, 종료 후 대기 복원, 용량 회복 뒤 agent 하나의 자동 시작을 확인했다. 취소한 작업은 재실행되지 않았다.
- 실제 structured ACP 화면에서 첫 시작 중 종료한 채팅의 예약된 session/tab을 복원하고, 보존된 draft를 확인했다. 공급자-visible index가 생기기 전의 tab도 올바르게 복구하도록 수정했으며 대기 알림이 입력창을 가리지 않음을 이미지로 확인했다.
- 기존 structured 대화에서 새 입력을 Send한 뒤, 자식 생성 전 대기 상태로 Orca를 종료·재실행했다. 용량 회복만으로 **추가 Send/Retry 없이** 원래 입력이 완료됐다. 재시작 뒤 fixture POST는 1회, 원문 표시는 1회였으며 실제 omp 응답 `ADMISSION-OK`가 화면에 표시됐다.
- 구형 설치 omp는 launcher가 `OMP_ADMISSION_CLIENT_REQUIRED`로 설정 변경 전에 거절했다. 수정본은 버전 검사를 통과하고, 서비스가 없는 격리 home에서 `DAEMON_UNAVAILABLE`로 종료했다. 확장 디렉터리를 만들지 않았다.
- 이 결과는 대기·복구·중복 방지 경계의 검증이다. 실제 OpenAI/Anthropic 처리량, 최대 지속 RPS, 전체 시스템 RSS 상한을 측정하거나 보장한 것은 아니다. Rust의 기존 Windows `STOP_GRACE` 미사용 경고와 Orca bundle 크기/dynamic-import 경고는 남아 있다.

## 출시

버전은 Ojak **0.12.0**, omp **18.12.0-ojak.1**, Orca **1.5.0-ojak.1**이다. Windows에서는 소스 빌드와 격리 실행만 검증했으며 설치본을 교체하지 않았다. PC를 재부팅하거나 실제 사용 중인 서비스·IDE·세션을 종료하지 않았다. Mac 설치/검증, release installer 검증, commit/push/공개 배포도 하지 않았다. 구형 클라이언트는 새 모델 POST 규약을 사용할 수 없으므로 실제 반영은 세 수정본과 기존 세션 전환 시점을 함께 조율해야 한다.
