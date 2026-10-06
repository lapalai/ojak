# Ojak 자동 연결과 일관된 계정 배정

- 작성: 2026-10-01
- 상태: macOS 구현·설치 검증 완료. Windows 구현·회귀 검증을 확장했으며 최종 설치본의 실행 승인 및 사용자 참여 항목은 아래 참조.
- 방향: 기존 구독·공식 CLI를 유지하는 통합 설치 및 계정 관리
- 영향: 코드 4 / 런타임 4 / UX 4 / 데이터 2 / 외부 연동 4 / 테스트 4 = 22/30

## 사용자 목표
설치 후 계정을 연결하면 기존 Claude Code·Codex·omp 실행 습관을 유지한다. 사용자가 shim/PATH/broker 명령을 학습하지 않아도 연결 설정과 적용 여부를 앱에서 확인한다. 여유 있는 계정을 우선하고 대안이 없으면 남은 한도를 사용한다. 실제 배정과 사용 현황 설명이 서로 달라서는 안 된다.

## 범위와 안전 경계
- Claude/Codex는 공식 CLI와 프로필 격리·lease를 그대로 사용한다. 실행 후 요청별 계정 교체를 지원한다고 표시하지 않는다.
- omp는 기존 broker/bridge/observer 연동을 재사용한다. 새 OAuth 프록시나 공급자 토큰 교체 경로를 만들지 않는다.
- API key/base URL/auth helper 충돌은 기존 AUTH_OVERRIDE_CONFLICT로 유지한다. 사용자 설정을 무조건 덮지 않는다.
- 계정 로그인은 사용자 동작이 필요하다. 설정 설치, 실제 명령 연결 확인, 실제 요청 관측은 서로 다른 근거로 표시한다.
- 인증·소진·동시 슬롯·프로젝트 정책·관측 신선도 등의 하드 제약을 완화하지 않는다.
- 스트림 중 재시도, 불확실한 spawn 재시도, lease 타임아웃 해제 금지를 유지한다.
- CLI 도구 업데이트 후 원본 진입 경로를 계속 사용한다. 기존 연결 소유권 및 제거·복원 절차를 재사용한다.

## 현행과 변경
### 설치·연결
현재 setup은 서비스/셸 설정 파일/claude·codex shim 존재로 ready를 판단한다. 이것을 실제 실행 경로 확인과 구분하고, 기존 omp 연결 기능을 준비 흐름 안에서 선택·실행·확인할 수 있게 통합한다.
- 준비 화면에서 서비스, 도구별 계정, 명령 연결, 검증 결과, omp 연결을 한 번에 확인한다.
- 새 로그인 셸의 명령 해석을 제한된 시간 안에 검사한다. 셸 설정 실행은 사용자 코드 실행이므로 설치/명시적 재점검에서만 수행하고 매 상태 폴링에서는 실행하지 않는다. 검사 결과는 설치 상태와 구분한다.
- 명령 충돌·검사 실패·미설치·재실행 필요를 성공으로 바꾸지 않는다.
- omp 설치가 확인되면 사용자가 동의한 준비 작업에서 broker → bridge → observer를 기존 안전한 설치 함수로 연결한다. 로그인/기존 설정 충돌 시 중단 이유와 재점검 경로를 제공한다.
- 준비 화면의 완료는 설정 적용 완료이며, 실행 중인 세션까지 전환됐다는 의미가 아니라고 명시한다. 실요청 경유 관측은 기존 observer/bridge 근거만 사용한다.

### 배정
- scheduler의 SAFETY_RESERVE 하드 제외를 소프트 우선순위로 바꾼다.
- 하드 제약을 통과한 roomy 후보가 있으면 reserve 후보보다 우선한다. 없으면 reserve 후보를 선택한다.
- 명시 계정/프로젝트 pin/재개 소유자 등 기존 강제 제약은 유지한다. 공급자 선호 pin은 roomy 후보가 있을 때 reserve 후보를 우선하지 않는다.
- 남은 한도 0 및 실제 exhausted는 계속 제외한다. 모델별 한도도 해당 모델에만 적용한다.
- 설명에는 RESERVE_FALLBACK 등 기계 판독 가능한 이유를 제공한다.

### 서비스 근거 기반 사용 현황
- 계정 한도 요약(available/reserve/partial/resting/excluded/login/unknown)을 서비스에서 계산하여 snapshot으로 제공한다.
- 이것은 모델·프로젝트·동시 슬롯이 정해지지 않은 한도 요약이다. 특정 실행의 허용 여부는 route.explain/lease.acquire가 결정한다.
- 화면에서 자체 verdictOf로 다시 계산하지 않는다. bridge의 실제 차단 상태도 서비스 요약에 반영한다.
- 같은 실제 계정에 여러 도구 기록이 있을 때 계정 그룹을 기준으로 일관되게 요약한다. 서비스와 화면의 그룹 키 계약을 명시한다.
- 기간별 요청 수와 현재 한도 상태를 혼동하지 않는다. 한도 초기화 표시와 기존 경로 분류 수정은 보존한다.

## 데이터 흐름 및 트리거
- 사용자가 준비 실행/재점검 → desktop setup command → launcher 설치/연결 검사 → 구조화된 setup 결과 → 준비 화면.
- CLI 실행 → shim → route.resolve/scheduler → lease → 공식 CLI 프로필 실행. reserve는 후보 우선순위에서 처리한다.
- omp 요청 → 기존 bridge choose → 계정 gateway → 공급자. 기존 soft reserve 동작을 유지한다.
- status.read → 계정/한도/bridge 차단 스냅샷 → 서비스 한도 요약 → 사용 현황. 비밀·프롬프트는 출력하지 않는다.

## 레퍼런스와 채택 범위
- https://github.com/farion1231/cc-switch : 자동 설정·백업·도구별 연결 상태 UX. 임의 endpoint/auth 변경은 복제하지 않는다.
- https://github.com/automazeio/vibeproxy : 메뉴바 앱과 서비스 통합 경험. 구독 OAuth 중개 허용의 근거로 삼지 않는다.
- https://github.com/router-for-me/CLIProxyAPI : 계정별 상태·프로토콜 경계 참고. 엔진 교체 없음.
- https://github.com/steipete/CodexBar : 한도 및 초기화 표시.
- https://code.claude.com/docs/en/legal-and-compliance : 제3자 구독 인증 중개 제약. 기존 omp Claude 연동도 별도 정책 검토 대상이며 신규 인증 방식 확대 없음.
- https://code.claude.com/docs/en/llm-gateway : gateway 연결 가능성과 구독 자격증명 풀링은 별개.
- https://developers.openai.com/codex/auth : 구독 로그인과 API 과금 구분.

## 완료 기준 및 검증
1. 준비 화면에서 각 도구의 설치/연결 검증/필요한 다음 행동을 구분한다. shim 파일만 존재한다고 실행 검증 완료로 표시하지 않는다.
2. 기존 설정을 보존하고 설치·재점검을 반복해도 불필요하게 재시작하거나 설정을 중복하지 않는다.
3. omp 연결도 준비 흐름에서 기존 안전 경로로 수행할 수 있고 오류를 숨기지 않는다.
4. 소진 계정 + 잔여 5% 계정이면 CLI 새 배정과 omp는 하드 제약을 통과한 잔여 계정을 쓴다. roomy가 있으면 우선하며 실제 차단·용량 예약은 우회하지 않는다.
5. 서비스가 전달한 한도 요약과 사용 현황이 일치한다. 이전 서비스처럼 요약을 제공하지 않으면 추정 성공 대신 갱신 필요를 표시한다.
6. 기존 영문·한국어·인도네시아어, 모델별 한도, 주간 초기화, 개인정보 가림을 유지한다.
7. 관련 Rust 및 desktop 테스트, 실제 setup 명령, 실제 snapshot과 설치 앱 화면으로 검증한다. 실모델 호출은 임의로 생성하지 않는다.
8. 서비스 변경 배포 전 진행 중인 브릿지에 대한 영향을 알리고 배포 후 상태를 확인한다.

## 작업 분리
- 설치 흐름: launcher setup 및 해당 CLI/Tauri 명령, SetupGuide/API의 setup 타입·번역.
- CLI 배정: scheduler 및 해당 회귀 테스트.
- 통합 담당: protocol snapshot의 서비스 한도 요약, 서비스 구현, UsageView와 타입·불필요한 frontend verdict 제거, 문서와 최종 검증·배포.

## 실행 검증
- Rust 전체 145개, desktop 13개 통과. 최종 launcher 정리 뒤 해당 40개도 통과. TypeScript 검사와 release 앱/DMG 빌드 통과.
- 설치된 CLI에서 `setup --status`는 미검사(null), `setup --check`는 Claude·Codex 명령 검증 성공을 반환했다. 임시 ZDOTDIR로 별칭 충돌과 5초 셸 타임아웃을 확인했으며, 상태 조회는 셸 시작 파일을 실행하지 않았다.
- `setup --with-omp`에서 broker·bridge·observer 준비 완료, 서비스 추가 재시작 없음 확인. 실제 snapshot의 17개 계정이 9개 한도 그룹에 중복·누락 없이 포함됐다.
- 실제 계정 형태를 메모리에 복제한 임시 실행기로 잔여 5%/소진 조합의 `RESERVE_FALLBACK` 및 roomy 우선 선택을 확인했다. 운영 계정·정책 변경과 모델 요청은 하지 않았다. 임시 실행기는 제거했다.
- 설치 앱에서 재점검 후 Claude·Codex 검증 성공 및 준비 완료 화면, 사용 현황의 서비스 기반 배지와 주간·모델별 초기화 표시를 확인했다.
- 로컬 설치본은 `~/Applications/Ojak.app`. 서비스 재시작 후 연결 상태를 확인했다. 빌드는 로컬 ad-hoc 서명이며 Apple 공증은 자격증명 미설정으로 수행하지 않았다.

## 플랫폼별 확대 검증
- macOS: Rust 148개, desktop 13개, observer 8개 통과. TypeScript 및 앱/DMG 빌드, 실제 설치 앱의 재점검·준비 완료·개인정보 가림/복원 확인.
- Windows 11 개발용 PC(`<windows-pc>`): Rust 기본 104개 통과 후 추가 경계 회귀 3개·명령 우선순위 1개·타 설치 shim 재귀 1개도 통과(합계 109개). desktop 13개 통과, TypeScript 및 x64 NSIS 빌드 성공.
- Windows 런타임: 임시 AAM_HOME에서 named pipe 연결, quotaSummaries, 미검사 상태, 기존 파일 충돌 거부/보존, Claude·Codex shim 설치, 저장 PATH의 실제 해석, 원본 버전 명령 통과, 반복 PATH 설치 멱등성, 제거 후 PATH 원문 복원과 shim 제거를 확인했다.
- 발견·수정: Windows 명령 검증 누락, 복수 `Get-Command` 결과의 첫 실행 경로 선택, 다른 AAM_HOME의 복사형 shim을 원본으로 오인하는 재귀, Windows 미지원 omp가 준비 완료를 막는 UI, Unix symlink 동등 경로 오판.
- 배정 경계: 잔여 5%/소진/roomy, 공급자 선호와 명시 계정, reserve=0, 인증·비활성·프로젝트·리셋·신선도·동시 슬롯, 모델별/공용 한도, rate block 우선순위, workspace 분리·이메일 모호성을 기존 및 추가 회귀 테스트로 확인했다.
- **전체 플랫폼 기능 통과를 뜻하지 않는다.** Windows broker/bridge/observer 및 외부 세션 관측을 추가했고, 이전 observer 7개 실패는 Unix 검사를 제거하는 대신 native ACL writer로 해결했다. 최신 Windows Rust 132개와 observer 8개가 통과했다.
- 미검증: 새 계정 OAuth 로그인 전체 흐름, 실제 공급자 장애를 동반한 장시간 실행, 로그아웃/재부팅 자동 실행, 전체 트레이/팝오버 상호작용. 다른 Windows 사용자 접근 차단은 별도 검증 항목에서 확인했으며, 나머지를 자동화 테스트로 실측한 것처럼 표시하지 않는다.
- NSIS 자동 안전 종료·파일 검증·복구·재시작을 구현했다. 임시 설치의 실제 서비스로 정상 교체와 실패 복구 후 신규 배정 재개를 확인했다. 사용자 설치본의 최종 실행은 AhnLab V3 Lite 승인 대기이므로 성공으로 판정하지 않는다.
- 수정 후 Windows 런타임 결과: adapters `22 passed; 0 failed`(타 설치 shim 거부 회귀 포함), Claude·Codex 모두 `account: true`, `verified: true`, 최종 `WINDOWS_RUNTIME_SMOKE_COMPLETE`. 이는 설치 전 release 바이너리 스모크이며 설치 UI 검증과 구분한다.
- 이전 설치본 확인(Windows omp 지원 추가 전): 안전 종료→NSIS 재설치 후 서비스 SHA-256이 당시 release와 일치했다. 일반 사용자 대화형 세션의 실제 앱에서 Claude·Codex 모두 `Windows 저장 PATH의 명령 연결 확인`, `준비 완료`, omp 미지원 안내를 확인했다(`WINDOWS_UI_SMOKE_COMPLETE`). 이는 아래 최종 빌드의 V3 승인 대기를 해소한 결과가 아니다.
- 추가 실측: 양쪽 OS에서 CLI 없음·로그인 없음 및 CLI 있음·로그인 없음 상태를 각각 확인했다. macOS 실제 OMP Ojak 요청 `OK`, native Codex 관리 실행 `OK`·exit 0·`EXITED`, 호스트 프로필 충돌의 안전 차단을 확인했다.
- 앱과 shim의 실행 진입 경로 차이로 observer 최신 판정이 달랐다. helper 경로를 canonicalize하여 양쪽 `current: true`와 실제 앱의 broker·bridge·observer 준비 완료 화면을 확인했다. 수정 후 launcher 테스트는 macOS 40개, Windows 34개 통과했다.
- 현재 Windows 설치 경로의 `aam.exe`는 V3의 프로그램 실행 알림에서 보류된다. 보안 설정이나 허용 목록은 바꾸지 않았으며, 최종 NSIS 설치·서비스 재개·OMP 실요청·UI 확인은 사용자 승인 이후 이어가야 한다. 상세 증거와 한계는 `2026-09-29-windows-port.md`에 기록했다.
