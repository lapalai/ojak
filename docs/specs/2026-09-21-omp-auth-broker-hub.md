---
created: 2026-09-21
status: in-progress
impact_score: 23
impact_size: large
direction: native-omp-with-managed-auth-broker
scope: macOS-local-OMP
---

# AAM 인증 허브와 원본 OMP 연결 스펙

> **2026-09-27 제거 기록.** 이 문서가 "정리 대상"으로 적은 관리 OMP(`aam-omp` 18.2.6·`integrations/omp/hard-control`)와 그 sidecar 빌드·shim·`aam run omp` 경로, `aam omp-broker login` 하위 명령은 이 날짜에 삭제했다. 관리 OMP는 broker 설정과 공존할 수 없어 브릿지 구조에서 영구히 실행 불가능했기 때문이다. 원본 OMP + broker + 계정 브릿지 구조는 그대로 유지한다. 아래 본문의 hard-control 언급은 당시 기록이다.

## 1. 목적과 승인 범위

AAM은 계정 등록·상태 조회·인증 서버 수명 관리를 담당하고, 원본 OMP는 모델 선택·대화·확장·실행을 담당한다. 일반 `omp` 명령을 AAM shim으로 가로채지 않는다.

2026-09-22 사용자는 **기존 로그인 그대로 사용**을 선택하고 구현보다 스펙 확정을 먼저 요청했다. 현재 작업은 설계만 수행한다. 아래 §13이 이전의 재로그인 대안 및 축소된 구현 범위보다 우선한다.

완결된 첫 지원 범위는 **같은 Mac의 기본 OMP 프로필을 Terminal과 Orca 터미널에서 사용하는 흐름**이다. Orca 내장 채팅 및 독립 Claude/Codex CLI는 이 프로토콜을 지원한다고 가정하지 않는다. 기존 Claude/Codex AAM 연결은 유지하며, 전체 제품의 다른 도구 연결은 별도의 adapter 검증 후 확장한다.

## 2. 확인된 사실과 근거

- 설치 원본 CLI: `~/.local/bin/omp`, 버전 18.2.7. 실제 `omp auth-broker --help`에서 serve/login/logout/import/migrate/status/list 지원을 확인했다.
- `/login`은 provider를 선택하고 provider별 인증을 수행한다. 모델과 provider와 계정은 서로 다른 개념이다. 같은 provider의 여러 OAuth 계정을 저장할 수 있다.
- 원본 18.2.7 `AuthStorage.login()`은 내장 또는 확장 등록 provider의 login을 호출한다. 저장소에 `upsertAuthCredentialRemote`가 있으면 원격 저장하며, OAuth도 `#upsertOAuthCredential`에서 같은 경로를 사용한다. 따라서 broker 연결 후 `/login`을 금지하거나 반드시 broker CLI로만 로그인해야 한다고 설명하면 안 된다.
- `auth.broker.url/token` 설정 또는 `OMP_AUTH_BROKER_URL/TOKEN` 환경변수로 원본 OMP의 저장소를 전환할 수 있다. broker URL이 없으면 local SQLite를 사용한다. 환경변수가 파일보다 우선한다.
- broker는 refresh token의 정본을 관리하고 갱신한다. broker client에는 access token이 전달될 수 있다. **토큰이 OMP에 전혀 전달되지 않는다고 주장하지 않는다.** access token까지 숨기는 gateway는 다른 구성이다.
- client account pool은 클라이언트 측 OAuth 선택 필터이며 권한 격리가 아니다. 누락 provider는 무제한이고 API key는 이 필터 대상이 아니다.
- 현재 AAM private OMP는 18.2.6 기반이며 `integrations/omp/hard-control`에서 broker 설정을 차단하도록 패치돼 있다. 새 구조에는 이 실행본을 사용하지 않는다.
- 이번 장애는 Orca가 주입한 `--extension`을 AAM launcher가 거부하고, `-c`를 관리 lease에 한정해 해석한 데서 발생했다. 현재 OMP shim만 제거하여 원본 새 실행과 실제 기존 대화 재개를 확인했다.

공식 근거:
- https://github.com/can1357/oh-my-pi/blob/v18.2.7/docs/auth-broker-gateway.md
- https://github.com/can1357/oh-my-pi/blob/v18.2.7/packages/ai/src/auth-storage.ts
- https://github.com/can1357/oh-my-pi/blob/v18.2.7/packages/ai/src/auth-broker/discover.ts

문서의 일부 read-only 설명보다 해당 버전의 실제 async remote write 구현을 우선한다. broker 연결을 실제로 적용한 통합 시험은 아직 하지 않았다.

## 3. 영향도와 선택 근거

|축|점수 / 5|이유|
|---|---:|---|
|코드|4|Tauri·서비스·adapter·연결 UI·launcher 설치 정책 변경|
|런타임|4|broker 수명과 토큰 갱신·클라이언트 cache|
|UX|4|도구 로그인 중심에서 provider 계정·연결 중심으로 변경|
|데이터|4|계정 identity 대응과 안전한 인증정보 이관|
|외부|4|원본 OMP 버전·OAuth·Orca 환경 우선순위|
|검증|3|로그인·동시 실행·장애·복원 실기|

선택안: AAM이 검증된 원본 OMP의 broker를 관리한다. 자체 OAuth/broker 재구현은 중복 유지보수 때문에 제외하고, launcher 통제를 유지하는 안은 이번 장애를 구조적으로 해결하지 못하므로 제외한다. gateway 방식은 모델 요청 프록시까지 범위가 커져 이번 OMP 연결에는 채택하지 않는다.

## 4. 사용자 흐름

### 4.1 처음 연결

1. 연결 화면에서 OMP 원본 경로·버전·대상 프로필·기존 broker/환경변수·shim 상태를 진단한다.
2. 사용자가 `OMP 연결`을 누르면 변경 미리보기를 표시한다. 인증정보 이동 대상, 설정 키, 서비스, 영향을 받는 새 실행과 기존 실행을 구분한다.
3. 원본 기본 프로필 계정이 있으면 provider·email·accountId·org/project를 비밀 없이 제시하고 가져올 계정을 명시 선택한다. 이메일만으로 병합하지 않는다.
4. AAM 전용 broker 저장소를 만들고 준비 상태를 확인한다. 비밀정보를 파일/로그/argv에 덤프하지 않고 검증된 전달 경로로 선택 계정을 이관한다.
5. 인증 서버의 실제 인증된 조회 성공 후 OMP 설정에 broker 연결을 적용한다. health HTTP 200만으로 연결 완료로 판정하지 않는다.
6. 새 원본 OMP에서 effective auth source와 선택 가능한 identity를 확인한 후 연결 완료를 표시한다. 기존 실행 중인 OMP는 자동 종료·전환하지 않는다.

### 4.2 계정 추가

AAM `계정 추가` → provider 선택 → 원본 OMP 로그인 기능을 사용한 사용자 인증 → broker 저장 → metadata 재조회 → 등록된 identity 확인.

초기 구현은 기존 `login_account`의 native Terminal 인증 UX를 재사용할 수 있다. 임의로 새 OAuth client를 만들거나 브라우저 cookie를 가져오지 않는다. 터미널이 열렸다는 응답만으로 로그인 성공 처리하지 않고 취소·실패·실제 저장 여부를 분리한다. 사용자 브라우저 로그인·추가 인증은 사용자에게 맡긴다.

OMP에서 `/login`하는 경우에도 provider별 로그인은 그대로 보인다. broker 연결 상태에서 생성된 계정은 broker에 저장되고 AAM 조회에 반영돼야 한다. CLI 명령 경로가 실제 어느 저장소에 쓰는지 버전별 시험으로 확인한다.

### 4.3 사용

Terminal/Orca에서 평소처럼 `omp`, `omp -c`, `omp --model ...` 실행. provider별 가용 계정 중 실제 선택과 재시도는 원본 OMP 정책을 따른다. 로그인했다고 모델이 자동 변경됐다고 표시하지 않는다.

이번 연결은 **계정 공유**를 제공하며 기존 AAM의 단일 계정 강제 고정을 동일하게 보장하지 않는다. 연결 화면에서 이를 명시하고 OMP에 대해 효력이 없는 기존 pinned/automatic 정책을 적용 중으로 표시하지 않는다. 엄격한 계정별 접근 제어·창별 강제 배정은 이번 범위가 아니다. 향후 trusted-client pool을 활용해도 보안 경계라고 부르지 않는다.

### 4.4 해제

`OMP 연결 해제`는 AAM이 소유한 config 키만 원래 값으로 복원한다. 사용자가 이후 수정한 값은 덮어쓰지 않고 충돌로 안내한다. 계정 삭제·provider 로그아웃·broker 중지는 별개의 작업이다.

broker에서 새로 추가/갱신된 계정이 local 저장소에 자동으로 돌아온다고 가정하지 않는다. 해제 전 로컬 계정 재로그인 필요 여부를 안내한다. 이전 local refresh token을 무조건 복사 복원하면 안 된다. 기존 broker client가 남아 있거나 확인 불가능하면 broker를 자동 중지하지 않는다.

## 5. 구조와 정본

```text
AAM UI → Tauri → 기존 AAM 사용자 서비스 → 원본 OMP broker 프로세스
                                           ↕
                              AAM 전용 OMP SQLite 인증 저장소
                                           ↑
                 원본 OMP (Terminal / Orca 터미널)
```

- token 정본: AAM 전용 프로필에서 원본 broker가 관리하는 OMP 저장소. AAM state.sqlite3에는 token을 저장하지 않는다.
- AAM metadata: provider, broker identityKey, native credential ID, 표시 정보, 마지막 확인 시각. credential ID만 영구 identity로 취급하지 않는다.
- 기존 AAM Account와 broker 계정은 `(provider, identityKey)`로 연결한다. provider별 org/project 식별을 보존하고 미확인·중복은 사용자가 구분할 수 있게 남긴다.
- OMP 대화 파일은 이동하거나 AAM lease ID로 변환하지 않는다. 이전 관리 세션의 경로도 보존한다.
- UI의 실제 적용 계정은 관측 증거가 있을 때만 표시한다. 등록됨·가용함·선택 정책·실제 요청 계정을 분리한다.

## 6. 연결·보안 계약

### 프로세스

AAM 서비스가 원본 절대경로·실행 버전·전용 agent/config root를 지정해 broker를 자식으로 관리한다. profile/config root 분리와 `serve`가 여는 DB는 구현 전 smoke 검증 필수다. `OMP_AUTH_BROKER_*`를 그대로 자식에게 상속하여 재귀 client가 되는 것을 막는다.

loopback에만 bind한다. 기본 8765 충돌 시 임의의 기존 서버에 연결하지 않는다. AAM 소유 receipt와 process identity·전용 token·인증된 handshake로 소유권을 확인한다. 다른 포트를 사용하면 실제 주소를 저장·설정하고 새 포트가 준비되기 전 기존 연결을 내리지 않는다. 기존 정상 원본 broker가 있으면 소유권을 인수하지 않고 별도 명시 승인 또는 연결 중단으로 처리한다.

앱 창 종료는 broker를 중지하지 않는다. AAM 서비스의 자동 재시작은 bounded backoff를 적용한다. 서비스 중지·업데이트 때 기존 client 영향을 표시한다. 사용자 요청 없이 운영 서버에 배포하거나 네트워크 밖으로 공개하지 않는다.

### 설정과 비밀

- 연결 주소와 token source를 OMP의 지원 config에 기록한다. token을 argv·프론트엔드·클립보드·진단 JSON에 전달하지 않는다.
- broker token root는 AAM 전용 0700, token 파일은 0600. 클라이언트 기본 token 파일이 이미 존재하면 덮어쓰지 않는다. config token resolver 또는 승인된 전용 token-file 경로를 사용하되 현재 버전 호환성을 시험한다.
- 실제 token 저장 경로와 config overlay 방식은 probe 결과로 확정한다. shell 전역 export나 원본 auth DB symlink 공유로 우회하지 않는다.
- config.yml/yaml의 기존 값·주석·다른 키를 보존한다. 두 파일 공존·중복 키·symlink·권한 오류는 안전하게 중단한다. 적용 직전 content digest 재검사, 같은 디렉터리 임시 파일과 atomic rename, private backup, 소유 receipt를 사용한다.
- receipt에는 대상 경로·이전 키 존재 여부·이전 값/설정의 private 참조·적용 digest·operation ID를 기록한다. 비밀이 포함될 수 있는 백업은 공개하지 않는다.
- URL/token 환경변수, 별도 profile, config overlay가 우선하면 기본 연결 성공을 해당 실행의 성공으로 확대하지 않는다.
- snapshot에는 access token이 포함될 수 있으므로 backend에서 metadata만 추출한다. raw snapshot과 login URL/code는 로그·UI 상태 저장·오류 추적에 남기지 않는다.

### 인증 이관

기존 local/broker가 동시에 refresh할 수 없어야 한다. 공식 API로 원본을 보존하되 인증 후보에서 제외하는 비활성화와 진행 중·신규 local client 차단을 입증한 뒤에만 이전한다. 이 조건이 불가능하면 전환을 중단하고 설계로 돌아온다.

비활성화·복원이 원본 지원 API로 검증되지 않으면 SQLite 테이블을 직접 수정하지 않는다. **재로그인 없는 전환은 필수 요건이다.** 이를 충족하지 못하면 기술 게이트 실패로 보고하고 구현을 중단한다. 재로그인 방식으로 대체하거나 기존 계정을 무조건 복제하지 않는다.

이관 후 rollback은 오래된 refresh token 복사가 아니라 설정 복원과 필요시 native 재로그인이다. 로컬 원본은 자동 삭제하지 않는다. rollback 한계를 실행 전 고지한다.

## 7. 상태·RPC 제안

새 이름은 구현 제안이며 현재 존재하는 API가 아니다. 기존 `native()` → Tauri → 서비스 RPC 패턴을 재사용한다.

- `omp.connection.inspect`: 원본/버전/프로필/실효 설정·conflict·연결 상태
- `omp.connection.preview`: 변경 목록·사전조건·source digest·짧은 수명의 승인용 preview ID
- `omp.connection.connect`: preview ID + request ID로 중복 실행 방지; 오래된 preview 거부
- `omp.connection.disconnect`: 소유권 확인 후 config 복원; 인증 삭제 안 함
- `omp.broker.status`: process identity·버전·health·인증된 조회 결과·마지막 정상 시각
- 계정 추가는 기존 native 로그인 entrypoint를 확장하고 결과를 서비스 계정 snapshot에 반영

연결 상태: DISCONNECTED → PREPARING → VERIFYING → CONNECTED. 실패는 FAILED/CONFIG_CONFLICT/VERSION_UNSUPPORTED로 분리하고 마지막 정상 상태를 보존한다. CONNECTED 이후 broker 장애는 DEGRADED이며 local 계정으로 몰래 fallback하지 않는다. OMP 자체의 유효한 encrypted snapshot cache 사용은 가능하지만 이를 broker 정상으로 표시하지 않는다.

Operation은 시작·현재 단계·완료·취소·실패를 비밀 없이 기록한다. 앱/서비스 재시작 시 receipt와 실제 상태를 재대조하여 미완료 작업을 복구한다. UI timeout 뒤 재클릭이 중복 broker나 OAuth flow를 만들지 않아야 한다.

## 8. 기존 코드 변경 지도와 순서

1. `crates/adapters` 및 원본 OMP probe: 지원 버전·broker capability·실효 인증 출처 확인. npm 최신 버전 자동 추종 금지.
2. `crates/service/src/lib.rs`, `process.rs`, `store.rs`: broker 수명·연결 작업·metadata 저장. 기존 protocol/store 관례 사용. 필요한 persisted 구조는 migration과 rollback을 명시하고 별도 검토한다.
3. `crates/protocol`: typed 요청/응답·상태. private token이 Snapshot에 섞이지 않게 한다.
4. `apps/desktop/src-tauri`: 로그인·설정 preview/apply·서비스 RPC 연결. 원본 CLI를 실행하며 AAM private runtime을 호출하지 않는다.
5. `apps/desktop/src/api.ts`, `types.ts`, `AccountsView.tsx`, `ConnectionsView.tsx`, `ManagementViews.tsx`: provider 계정 등록·연결 상태·지원 범위·사용 불가 정책 표시.
6. `crates/launcher/src/install.rs`: 일반 integration install/update에서 OMP shim이 다시 생성되지 않게 정책을 변경한다. 현재 설치본의 OMP shim 제거 상태를 보존하고 Claude/Codex shim은 유지한다.
7. OMP hard-control의 신규 실행 진입점 및 패키징 의존 제거. 기존 실행 중인 프로세스와 과거 세션 기록은 유지한다. 다른 도구가 사용하는 공통 scheduler는 삭제하지 않는다. source 참조를 추적해 미사용 경로를 정리한다.
8. 기존 `docs/spec.md`의 OMP 강제 실행·lease 재개·지원 보장을 새 계약으로 갱신한다. 기존 검증 이력은 역사로 남기고 현 지원 상태와 구분한다.

## 9. 구현 전 기술 게이트

다음은 사용자에게 사실 확인을 떠넘길 질문이 아니라 구현자가 격리 시험으로 해결할 항목이다.

- 원본 18.2.7에서 전용 broker profile/config root 및 native client 저장소 분리를 실제 확인.
- `serve` 생명주기·port 충돌·토큰 경로·표준출력의 비밀 누출 여부 확인.
- GUI 로그인에서 native CLI/SDK 중 유지보수가 적고 결과 확인 가능한 기존 경로 선택.
- broker 연결된 `/login`의 신규 identity 업로드를 격리된 테스트 provider/store로 확인. 실제 OAuth는 사용자 인증이 필요할 때만 진행.
- 선택 identity의 안전한 이관 API와 refresh 경합 방지를 입증한다. 실패하면 이전을 중단하고 보고하며 재로그인 방식으로 대체하지 않는다.
- OMP 재개 세션의 계정 선택은 원본 동작으로 시험. 원래 계정 강제 유지 보장은 별도 기능이므로 표시하지 않음.

## 10. 수용 기준과 검증

- AC01: AAM에서 provider 선택·인증·성공 identity 확인·취소·실패를 끝까지 처리한다. 창 열림을 성공으로 오인하지 않는다.
- AC02: 연결 전 실제 diff와 인증정보 전환 영향을 보여주며, 승인 후 새 원본 OMP가 broker 인증을 사용함을 확인한다.
- AC03: 원본 OMP의 `/login`으로 추가한 계정이 같은 broker와 AAM 목록에 보인다. 다른 provider·계정이 삭제되지 않는다.
- AC04: Terminal과 Orca 터미널에서 `omp`, `omp -c`, 모델 선택과 Orca status 확장이 동작한다. 기존 대화 마지막 내용이 유지된다.
- AC05: 여러 client에서 만료 갱신이 broker 경로로 처리된다. 저장된 refresh token 정본이 경쟁하여 덮어써지지 않는다.
- AC06: UI·stdout/stderr·diagnostics·argv에 token/credential snapshot이 노출되지 않는다. 권한과 loopback bind를 검증한다.
- AC07: port 점유·broker 종료·401·만료 cache·offline·구버전·환경변수 override에서 정확한 상태와 다음 행동을 제공한다. silent local fallback 없음.
- AC08: 설정 동시 수정·symlink·작업 중 crash·중복 클릭에서 사용자 파일을 덮어쓰지 않고 재시도/복원이 가능하다.
- AC09: 연결 해제 후 native local 사용 또는 재로그인 안내가 가능하며 기존 broker client·대화·계정은 삭제하지 않는다.
- AC10: 재설치/업데이트에도 OMP shim을 재생성하지 않는다. Claude/Codex 기존 연결과 실행 중인 작업이 유지된다.
- AC11: AAM UI가 원본 OMP의 계정 선택을 강제한다고 주장하지 않고 실제 적용 계정은 증거가 있을 때만 표시한다.

검증 순서: 격리된 인증 fixture로 protocol/config 복원·refresh 경쟁 회귀 → 원본 broker/OMP 프로세스 smoke → 실제 AAM UI → 사용자 승인하에 최소 live OAuth·추론 → 실제 Terminal/Orca 새 실행·동일 대화 재개. 회귀 테스트는 상태·보안·복원 등 실제 실패 가능한 경계에만 유지한다. 실계정 token을 fixture로 사용하지 않는다.

## 11. 완결성과 출시 조건

계정 추가·OMP 연결·상태 확인·일상 사용·해제·실패 복구를 모두 제공해야 완료다. 서버만 띄우거나 설정 파일만 쓰는 scaffold는 완료가 아니다. 실제 native 계정 연결 시험 없이 문서/컴파일 통과만으로 지원한다고 표시하지 않는다.

현재 상태: native OMP broker connect/status/disconnect와 Tauri Connections 화면 연결을 구현했다. 전용 `aam-managed` profile로 broker를 실행하고 authenticated snapshot handshake를 통과한 뒤에만 연결 완료로 표시한다. OAuth 로그인·기존 계정 이관·전체 AC01–AC11은 아직 출시 완료 범위가 아니다.

## 12. 검토 기록

외부 검토 결과는 요청 후 아래에 기록한다. 검토 실패는 승인으로 간주하지 않는다.

- 외부 advisor 실행 결과: `preflight:not-logged-in`으로 Codex 검토가 실행되지 않았다. 외부 검토 통과로 간주하지 않는다.
- 자체 검토 반영: broker와 gateway의 비밀 전달 차이, client pool의 비보안 경계, 기존 pinned 정책의 효력 상실, local/broker 동시 refresh 위험, 해제 시 오래된 token 복원 금지, 업데이트의 shim 재생성 방지를 명시했다.
- 문서 구조 검사: 코드블록 균형과 AC01–AC11 존재를 확인했다. 런타임 동작 검증을 대신하지 않는다.

## 13. 기존 로그인 유지형 계정 연결 — 2026-09-22 확정 방향

### 범위와 현재 부족한 부분

사용자는 기존 OMP 로그인을 그대로 사용하기로 선택했다. 대상은 원본 OMP 기본 프로필에 저장된 계정이며, AAM에 표시된 Claude/Codex 계정을 OMP 호환 인증이라고 가정하지 않는다. 기존 프로세스 무중단까지 보장하는 선택은 아니다. 안전한 전환에 기존 client 종료가 필요하면 안내하되 강제 종료하지 않는다.

`apps/desktop/src-tauri/src/main.rs::login_account`는 claude/codex/grok만 허용한다. OMP 계정 연결은 빠져 있다. 현재 `omp_broker.rs`의 snapshot handshake와 설치 앱의 연결 표시는 서버 접속만 검증하며, 계정 이전·native client 사용 가능·서비스 소유 수명 관리를 증명하지 않는다. 기존 §11 기록은 부분 구현 이력이지 완료 승인이 아니다.

`omp auth-broker --help`에서 `migrate --from-local --include-oauth --dry-run`을 확인했다. 명령의 존재만으로 원본 credential 비활성화·원자적 이전·refresh 안전성이 증명되지는 않는다.

### 앱 사용자 흐름

1. 연결 상태 → 원본 OMP → `기존 계정 연결`. 사용자가 터미널 명령을 입력하지 않는다.
2. 원본 버전·경로·프로필·기존 계정·실행 중인 local client·설정 충돌을 비밀 없이 점검한다.
3. provider + identity/org/project 기준으로 이전할 계정을 선택한다. 같은 이메일만으로 병합하지 않는다. 호환되지 않는 다른 도구 계정은 제외하고 이유를 표시한다.
4. 변경 미리보기에서 기존 client 종료 필요 여부, 영향 범위, 실패 복구 한계를 안내한다. 사용자 승인 전 token 이동이나 설정 변경은 없다.
5. 서비스가 source digest와 실행 상태를 재검사하고 작업을 잠근다. 새 local client의 동시 갱신까지 안전하게 배제할 수 없으면 진행하지 않는다.
6. 검증된 공식 경로로 이전하고 source/destination 사이에 refresh 정본이 중복으로 남지 않음을 확인한다.
7. broker identity 조회와 native client의 실효 인증 경로 확인 후에만 `OMP에서 사용 가능`으로 표시한다. 빈 snapshot이나 HTTP 200은 계정 연결 성공이 아니다.
8. Terminal/Orca의 새 원본 `omp`와 `omp -c`를 그대로 사용한다. 대화 파일은 이동하지 않는다.
9. 새 계정 추가는 별도 공식 OAuth 흐름으로 제공하고 저장 결과를 재조회한다. 기존 계정의 재로그인을 새 계정 등록으로 위장하지 않는다.

### 구현 진입 전 기술 게이트

- G1: 원본 18.2.7의 migrate/login/storage 구현에서 실제 source/destination·쓰기·삭제·refresh 경로를 추적한다.
- G2: 격리 credential fixture와 제어 가능한 refresh endpoint로 이전 전/중/후 token 정본이 하나임을 검증한다. source 갱신을 공식 API로 안전하게 중단할 수 없으면 실패다.
- G3: 기존 client의 메모리 token, 새 local client, 동시 broker 시작까지 경쟁 조건을 검증한다. 프로세스 목록 조회만으로 안전성을 선언하지 않는다.
- G4: 이전 중 취소·프로세스 종료·네트워크 실패·앱 재시작을 주입한다. 최신 token 소유자가 불명확하면 자동 rollback/재시도를 금지하고 복구 필요로 표시한다.
- G5: native client가 broker identity를 실제 사용하는지 비밀 없는 증거로 확인한다. provider별 지원 여부를 분리한다.
- G6: 실계정 이전과 최소 추론은 구현 후 별도 사용자 승인으로 실행한다. fixture 통과를 실계정 검증으로 표시하지 않는다.

게이트 실패 시 원인과 대안을 보고하고 설계로 돌아온다. 사용자 승인 없이 재로그인으로 요구사항을 축소하거나 SQLite 직접 수정으로 우회하지 않는다.

### 상태 및 계약

- 서버: 미시작 / 준비 중 / 정상 / 인증 오류 / 중단.
- 계정: 발견됨 / 이전 가능 / 기존 세션 종료 대기 / 이전 중 / 사용 가능 / 복구 필요 / 미지원.
- 연결: 미연결 / 사전 점검 / 승인 대기 / 이전 중 / native 검증 중 / 사용 가능 / 연결 장애 / 설정 충돌 / 복구 필요.
- 기존 §7 RPC 제안을 유지한다. 이전 계획에는 planId, sourceDigest, 대상 identity, 영향 범위, 만료 시각을 담고 적용에는 requestId를 사용한다. 서비스가 계획을 소유하고 재검사한다.
- AAM 계정 metadata는 서비스 snapshot으로 갱신한다. UI로 token이나 raw credential snapshot을 전달하지 않는다.
- 앱 종료는 broker 중지가 아니다. 해제·계정 삭제·로그아웃·broker 중지는 별도 동작이다.

### 구현 순서

1. G1–G5 probe와 provider 지원 매트릭스. 통과 전 실제 이전 기능을 출시하지 않는다.
2. 서비스에 broker 수명·작업 잠금·이전·복구 상태를 구현하고 launcher 직접 spawn 경로를 교체한다.
3. protocol/Tauri에 사전 점검·선택·이전·상태·복구 계약을 연결한다.
4. Accounts/Connections에 완결된 흐름을 제공하고 빈 broker를 연결 완료로 표시하지 않는다.
5. native OMP와 충돌하는 hard-control 안내 및 shim 재설치 경로를 정리한다. 기존 실행·기록·Claude/Codex 동작은 보존한다.
6. 자동 검증 → 실제 앱 검증 → 승인된 live 검증 → bundle 설치·재실행까지 수행한다.

### 추가 인수 기준

- AC12: 기존 계정을 재로그인 없이 연결하고 새 원본 OMP에서 해당 provider 최소 요청이 성공한다.
- AC13: 발견됨·서버 정상·계정 사용 가능을 구분하고 미지원·빈 계정·충돌에 다음 행동을 표시한다.
- AC14: 이전 중 중단·중복 클릭·재시작에도 refresh 정본이 중복되지 않고 작업 상태가 복구된다.
- AC15: 앱 및 broker 재시작 후 계정이 유지되고 native client 연결이 회복된다.
- AC16: 해제 시 오래된 local token을 복원하지 않는다. 로컬 복귀 가능 여부와 한계를 실행 전에 명시한다.
- AC17: 사용자에게 build/export/kill 등 개발 명령을 제품 사용 절차로 요구하지 않는다.

기존 AC01–AC11과 AC12–AC17을 모두 검증하기 전에는 완료로 보고하지 않는다. 현재는 설계안이며 기술 게이트와 실계정 전환은 미실시다. 이번 스펙 작업에서 코드·설치·계정·브랜치는 변경하지 않는다.

### 독립 검토 반영

- 읽기 전용 외부 검토에서 재로그인 대안 잔존, source 비활성화/해제, 신규 client 경쟁의 세 문제를 확인했다. 이전 규칙과 기술 게이트를 통일했다.
- 이전 후 local 원본의 목표 상태는 **보존되지만 공식 API로 인증 후보에서 제외됨**이다. 만들거나 확인할 수 없으면 G2 실패다.
- 해제 전에 native OMP가 비활성 local 원본을 선택하거나 refresh하지 않는지 확인한다. 안전한 local 복귀가 불가능하면 계정 사용 불가를 명시한다. 오래된 token 활성화는 금지한다. 해제 후 선택적 재로그인 안내는 최초 전환의 재로그인 없는 요건을 대체하지 않는다.
- AC14 추가: 진행 중/신규 local client가 있으면 이전을 대기 또는 거부한다. config 전환 틈의 source refresh 주입 시 정상 완료되지 않고 복구 필요가 되어야 한다. 이전 완료 후 source의 인증·refresh 후보 제외를 native client로 검증한다.
- 지원 provider 목록은 G1–G5 결과로 명시적으로 동결하고 사용자에게 제시한다. 현재 검증 완료 provider는 없으며 목록 밖 계정은 이전하지 않는다. 검증 실패 provider를 숨기고 전체 계정 연결 완료라고 표시하지 않는다.

### 구현 결과 — 2026-09-22

기술 게이트 결과 **계정 사본을 만들지 않는 경로**를 확인했다. `auth-broker serve`는 활성 프로필의 기존 인증 저장소(`~/.omp/agent/agent.db`)를 그대로 서빙하며, broker 설정이 있어도 스스로 remote client가 되지 않는다. 따라서 broker를 기본 프로필로 실행하면 복사와 재로그인이 필요 없다. `migrate`는 복사만 하고 원본을 비활성화하지 않으므로 채택하지 않았다.

검증한 사실:

- 격리 fixture(api_key)에서 기존 자격증명이 재로그인 없이 broker 경로로 그대로 해석됐다.
- broker 중지 + snapshot cache 제거 시 client는 조용한 local fallback 없이 실패했다.
- 실계정에서 `omp usage`가 기존 계정으로 정상 동작했다.
- broker를 강제 종료해도 launchd가 재기동해 계정 접근이 복구됐다.
- snapshot metadata 집계는 같은 provider의 복수 계정을 합치지 않는다(회귀 테스트 보유).

**G2·G3 동시 refresh 시험 결과(2026-09-22, 수행함):** 제어 가능한 OAuth token endpoint(`mcp_oauth:` 자격증명의 `tokenUrl`)와 만료 OAuth fixture로, 같은 저장소를 공유하는 두 프로세스가 동시에 갱신을 시도하게 했다. 3회 반복 모두 **endpoint 호출 1회**, 회전된 최신 token이 저장되고, 오래된 값으로 덮어쓰이지 않았으며 lease row는 반환됐다. 즉 저장소 lease가 프로세스 간 갱신을 직렬화한다. 다만 시험 참가자는 두 broker 프로세스였고 **native local client가 이미 메모리에 들고 있는 token으로 갱신하는 경로는 직접 재현하지 않았다**(동일한 AuthStorage·lease 구현을 공유하지만 별도 확인 필요). fixture는 정식 업로드 API로 생성해야 저장 형식이 일치한다(수기 SQLite 삽입은 CAS 불일치로 갱신이 적용되지 않았다).

구현 범위: 기본 프로필 broker를 `ai.aam.omp-broker` LaunchAgent(RunAtLoad/KeepAlive)로 감독하고, 인증된 snapshot에서 provider metadata만 추출해 계정 수를 표시하며, 소유 receipt와 digest 검사로 config를 연결·해제한다. 연결 해제는 설정만 복원하며 계정·대화·실행 중 세션을 건드리지 않는다.

남은 범위: G2·G3의 동시 refresh 검증, 앱 내 신규 provider 로그인 흐름, AAM 서비스 소유의 작업 잠금·복구 상태 머신. AC01–AC17 전체 검증은 완료되지 않았다.
